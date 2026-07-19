//! Durable workspace-teardown graph cascade (#99).
//!
//! The executor half of the teardown cascade. [`crate::registry::teardown_bundle`]
//! (the one primitive every removal path funnels through) enqueues a torn-down
//! workspace id on the durable [`crate::registry::pending`] queue; this module
//! drains that queue, pruning each workspace's Chunk + now-orphan Entity nodes
//! from Memgraph and dequeuing an id only once its teardown has actually
//! landed. A graph outage therefore defers — never drops — the cleanup: the id
//! stays queued for the next drain (fired on the next create/activate/delete,
//! or at service boot).
//!
//! This replaces the old fire-and-forget prune bolted onto the `delete`
//! handler alone (which (a) missed MRU eviction entirely and (b) silently
//! orphaned on a transient `warn!`).

use std::future::Future;

use wylde_shared::ipc::Reply;

use crate::graph::BoltClient;
use crate::registry;

/// The narrow graph-teardown surface the drain needs: prune one workspace's
/// Chunk nodes + now-orphan Entity nodes. Implemented by [`BoltClient`] and a
/// test mock, so the drain logic is unit-testable without a live Neo4j.
pub trait WorkspaceGraphTeardown {
    fn delete_workspace(&self, workspace: &str) -> impl Future<Output = Reply> + Send;
}

impl WorkspaceGraphTeardown for BoltClient {
    fn delete_workspace(&self, workspace: &str) -> impl Future<Output = Reply> + Send {
        let ws = workspace.to_owned();
        async move { BoltClient::delete_workspace(self, &ws).await }
    }
}

/// What one drain pass did — for logs + tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Workspaces whose graph teardown landed (dequeued).
    pub removed: u32,
    /// Ids skipped because the workspace is live again (dequeued, not pruned).
    pub skipped_live: u32,
    /// Ids whose teardown failed (left queued for the next drain).
    pub failed: u32,
}

/// Drain the durable pending-cleanup queue against the real Bolt backend.
/// Best-effort + idempotent — safe to call from any handler or at boot.
pub async fn run_pending_cleanup() -> DrainReport {
    drain(&BoltClient::new()).await
}

/// Testable core: drive `sink` over the pending queue.
pub(crate) async fn drain<T: WorkspaceGraphTeardown>(sink: &T) -> DrainReport {
    let mut report = DrainReport::default();
    for id in registry::pending::list() {
        // A workspace id derives from its folder (#28), so a delete-then-add of
        // the same folder REUSES the id. If the id is a live workspace again,
        // it is NOT an orphan — dequeue it without touching its (fresh) graph
        // data, so a re-create can't be wiped by a stale teardown entry.
        if registry::get(&id).is_some() {
            registry::pending::remove(&id);
            report.skipped_live += 1;
            continue;
        }
        let reply = sink.delete_workspace(&id).await;
        if reply.ok {
            registry::pending::remove(&id);
            report.removed += 1;
        } else {
            // Leave it queued — the next drain retries. Durable, not
            // fire-and-forget.
            report.failed += 1;
            tracing::warn!(
                "workspaces.cleanup: graph teardown deferred for {id}: {:?}",
                reply.error
            );
        }
    }
    if report.removed > 0 || report.failed > 0 || report.skipped_live > 0 {
        tracing::info!(
            "workspaces.cleanup: drained pending graph teardown \
             (removed={}, skipped_live={}, deferred={})",
            report.removed,
            report.skipped_live,
            report.failed,
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A stateful fake graph modelling the workspace-teardown Cypher: Chunk
    /// nodes keyed by `(workspace, id)`, Entity nodes reachable via per-chunk
    /// mentions, and `delete_workspace` = drop the workspace's chunks + prune
    /// entities left with no mention (the `DELETE_ORPHAN_ENTITIES` shape).
    #[derive(Default)]
    struct FakeGraph {
        chunks: Mutex<HashSet<(String, String)>>,
        // (entity, workspace, chunk_id) — a chunk mentioning an entity.
        mentions: Mutex<HashSet<(String, String, String)>>,
        fail: bool,
    }
    impl FakeGraph {
        fn seed_chunk(&self, ws: &str, id: &str, entities: &[&str]) {
            self.chunks
                .lock()
                .unwrap()
                .insert((ws.to_owned(), id.to_owned()));
            let mut m = self.mentions.lock().unwrap();
            for e in entities {
                m.insert(((*e).to_owned(), ws.to_owned(), id.to_owned()));
            }
        }
        fn chunk_count(&self, ws: &str) -> usize {
            self.chunks
                .lock()
                .unwrap()
                .iter()
                .filter(|(w, _)| w == ws)
                .count()
        }
        fn entity_alive(&self, name: &str) -> bool {
            self.mentions
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _, _)| e == name)
        }
    }
    impl WorkspaceGraphTeardown for FakeGraph {
        fn delete_workspace(&self, workspace: &str) -> impl Future<Output = Reply> + Send {
            let ws = workspace.to_owned();
            let fail = self.fail;
            if !fail {
                // DETACH DELETE the workspace's chunks + their mentions, then
                // orphan-prune is implicit (an entity with zero mentions left
                // simply stops being "alive").
                self.chunks.lock().unwrap().retain(|(w, _)| w != &ws);
                self.mentions.lock().unwrap().retain(|(_, w, _)| w != &ws);
            }
            async move {
                if fail {
                    Reply::err_msg("bolt_connect", "no neo4j")
                } else {
                    Reply::ok(json!({"ok": true, "workspace": ws}))
                }
            }
        }
    }

    #[tokio::test]
    async fn drain_removes_target_workspace_chunks_and_orphan_entities() {
        let _env = TestEnv::new();
        let graph = FakeGraph::default();
        // ws-A: two chunks mentioning "foo" (only A) — foo is A-exclusive.
        graph.seed_chunk("ws-A", "a0", &["foo", "shared"]);
        graph.seed_chunk("ws-A", "a1", &["foo"]);
        // ws-B: one chunk mentioning "shared" (also in A) + "bar" (B-only).
        graph.seed_chunk("ws-B", "b0", &["shared", "bar"]);

        // Only ws-A was torn down.
        registry::pending::enqueue("ws-A");
        let report = drain(&graph).await;

        assert_eq!(report.removed, 1);
        assert_eq!(report.failed, 0);
        // ws-A's graph footprint is gone; ws-B is untouched.
        assert_eq!(graph.chunk_count("ws-A"), 0, "no orphan chunks survive");
        assert_eq!(graph.chunk_count("ws-B"), 1);
        // "foo" was A-exclusive → pruned; "shared"/"bar" still mentioned by B.
        assert!(!graph.entity_alive("foo"), "A-only entity pruned");
        assert!(graph.entity_alive("shared"), "still mentioned by ws-B");
        assert!(graph.entity_alive("bar"));
        // Dequeued.
        assert!(registry::pending::list().is_empty());
    }

    #[tokio::test]
    async fn drain_defers_on_graph_failure_keeping_the_id_queued() {
        let _env = TestEnv::new();
        let graph = FakeGraph {
            fail: true,
            ..Default::default()
        };
        graph.seed_chunk("ws-A", "a0", &["foo"]);
        registry::pending::enqueue("ws-A");

        let report = drain(&graph).await;
        assert_eq!(report.failed, 1);
        assert_eq!(report.removed, 0);
        // Durable: a graph blip must NOT drop the cleanup — the id stays queued
        // for the next drain, and the chunk is still there to prune later.
        assert_eq!(registry::pending::list(), vec!["ws-A".to_owned()]);
        assert_eq!(graph.chunk_count("ws-A"), 1);

        // A later drain against a healthy backend finishes the job.
        let healthy = FakeGraph::default();
        healthy.seed_chunk("ws-A", "a0", &["foo"]);
        let report = drain(&healthy).await;
        assert_eq!(report.removed, 1);
        assert!(registry::pending::list().is_empty());
        assert_eq!(healthy.chunk_count("ws-A"), 0);
    }

    #[tokio::test]
    async fn drain_skips_a_re_created_workspace_without_wiping_it() {
        let _env = TestEnv::new();
        // Same folder deleted then re-created reuses the id (#28). It is queued,
        // but it's live again — the drain must NOT prune its fresh graph data.
        let def = registry::create(&_env.ws_path("re-created"), None);
        registry::pending::enqueue(&def.id);

        let graph = FakeGraph::default();
        graph.seed_chunk(&def.id, "fresh0", &["foo"]);
        let report = drain(&graph).await;

        assert_eq!(report.skipped_live, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(
            graph.chunk_count(&def.id),
            1,
            "a re-created workspace's fresh chunks are preserved"
        );
        // ...and the stale queue entry is cleared.
        assert!(registry::pending::list().is_empty());
    }
}
