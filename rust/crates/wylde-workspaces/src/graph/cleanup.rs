//! Durable workspace-teardown cascade (#99; generalized to peer-service sweeps
//! in #166).
//!
//! The executor half of the teardown cascade. Every removal path enqueues
//! `(workspace id, teardown target)` pairs on the durable
//! [`crate::registry::pending`] queue — [`crate::registry::teardown_bundle`]
//! (the one primitive both delete and MRU eviction funnel through) enqueues the
//! graph target, and explicit [`crate::registry::delete`] additionally enqueues
//! the memory + conversation targets that eviction must preserve. This module
//! drains that one queue, dispatching each pair to its target's prune and
//! dequeuing a pair only once its teardown has actually landed. An outage of
//! any target therefore defers — never drops — the cleanup: the pair stays
//! queued for the next drain (fired on the next create/activate/delete, or at
//! service boot).
//!
//! This replaces the old fire-and-forget prunes bolted onto the `delete`
//! handler alone (which (a) missed MRU eviction for the graph and (b) silently
//! orphaned on a transient `warn!` for the graph, the memory tier, and the
//! conversation store alike).
//!
//! The graph cascade's exact statement sequence is
//! [`crate::graph::bolt::WORKSPACE_TEARDOWN_STEPS`]. Concepts were added to it
//! in #117; before that a deleted workspace's `Concept` nodes and `CHILD_OF`
//! edges were left in the graph permanently.

use std::future::Future;

use serde_json::json;
use wylde_shared::ipc::{send_action, Reply};

use crate::graph::BoltClient;
use crate::registry;
use crate::registry::pending::TeardownTarget;

/// The narrow teardown surface the drain needs: prune one workspace's footprint
/// from one peer-service store. Implemented by [`LiveTeardown`] (the real
/// dispatch) and a test mock, so the drain logic is unit-testable without a
/// live Neo4j or harness.
pub trait WorkspaceTeardown {
    fn teardown(
        &self,
        target: TeardownTarget,
        workspace: &str,
    ) -> impl Future<Output = Reply> + Send;
}

/// The production sink: dispatches each target to its owning service. The graph
/// prune runs the Bolt cascade in-process; the memory + conversation sweeps go
/// to the harness (the canonical owner of both stores) over IPC — the exact
/// calls the `delete` handler used to fire-and-forget, now retried durably.
struct LiveTeardown {
    graph: BoltClient,
}

impl LiveTeardown {
    fn new() -> Self {
        Self {
            graph: BoltClient::new(),
        }
    }
}

impl WorkspaceTeardown for LiveTeardown {
    fn teardown(
        &self,
        target: TeardownTarget,
        workspace: &str,
    ) -> impl Future<Output = Reply> + Send {
        let ws = workspace.to_owned();
        async move {
            match target {
                TeardownTarget::Graph => BoltClient::delete_workspace(&self.graph, &ws).await,
                TeardownTarget::Memory => {
                    send_action(
                        "wylde-harness",
                        "memory.workspace.delete_all",
                        json!({ "workspace_id": ws }),
                    )
                    .await
                }
                TeardownTarget::Conversations => {
                    send_action(
                        "wylde-harness",
                        "conversations.delete_by_workspace",
                        json!({ "workspace_id": ws }),
                    )
                    .await
                }
            }
        }
    }
}

/// What one drain pass did — for logs + tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Teardowns that landed (dequeued).
    pub removed: u32,
    /// Pairs skipped because the workspace is live again (dequeued, not pruned).
    pub skipped_live: u32,
    /// Teardowns that failed (left queued for the next drain).
    pub failed: u32,
}

/// Drain the durable pending-cleanup queue against the real backends.
/// Best-effort + idempotent — safe to call from any handler or at boot.
pub async fn run_pending_cleanup() -> DrainReport {
    drain(&LiveTeardown::new()).await
}

/// Testable core: drive `sink` over the pending queue, one entry per target.
pub(crate) async fn drain<T: WorkspaceTeardown>(sink: &T) -> DrainReport {
    let mut report = DrainReport::default();
    for entry in registry::pending::list() {
        let id = entry.workspace_id;
        let target = entry.target;
        // A workspace id derives from its folder (#28), so a delete-then-add of
        // the same folder REUSES the id. If the id is a live workspace again,
        // it is NOT an orphan — dequeue every target for it without touching
        // its (fresh) data, so a re-create can't be wiped by a stale teardown
        // entry. This guard matters MORE for the memory tier than the graph:
        // re-attaching memories the user believed deleted is the #166 privacy
        // failure.
        if registry::get(&id).is_some() {
            registry::pending::remove(&id, target);
            report.skipped_live += 1;
            continue;
        }
        let reply = sink.teardown(target, &id).await;
        if reply.ok {
            registry::pending::remove(&id, target);
            report.removed += 1;
        } else {
            // Leave it queued — the next drain retries. Durable, not
            // fire-and-forget.
            report.failed += 1;
            tracing::warn!(
                "workspaces.cleanup: {target:?} teardown deferred for {id}: {:?}",
                reply.error
            );
        }
    }
    if report.removed > 0 || report.failed > 0 || report.skipped_live > 0 {
        tracing::info!(
            "workspaces.cleanup: drained pending teardown \
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
    use crate::graph::{bolt, cypher};
    use crate::test_support::TestEnv;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A stateful fake graph modelling the workspace-teardown Cypher: Chunk
    /// nodes keyed by `(workspace, id)`, Entity nodes reachable via per-chunk
    /// mentions, and `Concept` nodes with their `CHILD_OF` (concept→concept)
    /// and `MEMBER` (concept→entity) edges.
    ///
    /// Concepts are modelled deliberately (#117): the previous version of this
    /// fake knew only about chunks and mentions, so a teardown that never swept
    /// `Concept` nodes looked perfectly correct here — the universe simply had
    /// no concepts in it to leave behind. A mock can only catch a bug it can
    /// represent.
    ///
    /// `delete_workspace` does not restate the cascade: it REPLAYS
    /// [`crate::graph::bolt::WORKSPACE_TEARDOWN_STEPS`], the same ordered table
    /// the real Bolt client executes, dispatching each step onto this store. A
    /// step dropped from that table therefore stops running here too, and the
    /// regression test below fails. An unrecognised step panics rather than
    /// silently doing nothing, so extending the cascade forces a matching
    /// extension of this model.
    #[derive(Default)]
    struct FakeGraph {
        chunks: Mutex<HashSet<(String, String)>>,
        // (entity, workspace, chunk_id) — a chunk mentioning an entity.
        mentions: Mutex<HashSet<(String, String, String)>>,
        // (workspace, concept_id)
        concepts: Mutex<HashSet<(String, String)>>,
        // (workspace, child_concept_id, parent_concept_id)
        child_of: Mutex<HashSet<(String, String, String)>>,
        // (workspace, concept_id, entity_name)
        members: Mutex<HashSet<(String, String, String)>>,
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
        /// Seed a Concept with its MEMBER targets and optional parent concept.
        fn seed_concept(&self, ws: &str, id: &str, members: &[&str], parent: Option<&str>) {
            self.concepts
                .lock()
                .unwrap()
                .insert((ws.to_owned(), id.to_owned()));
            let mut m = self.members.lock().unwrap();
            for e in members {
                m.insert((ws.to_owned(), id.to_owned(), (*e).to_owned()));
            }
            if let Some(p) = parent {
                self.child_of
                    .lock()
                    .unwrap()
                    .insert((ws.to_owned(), id.to_owned(), p.to_owned()));
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
        fn concept_count(&self, ws: &str) -> usize {
            self.concepts
                .lock()
                .unwrap()
                .iter()
                .filter(|(w, _)| w == ws)
                .count()
        }
        fn child_of_count(&self, ws: &str) -> usize {
            self.child_of
                .lock()
                .unwrap()
                .iter()
                .filter(|(w, _, _)| w == ws)
                .count()
        }
        fn entity_alive(&self, name: &str) -> bool {
            self.mentions
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _, _)| e == name)
        }
        /// MEMBER edges whose Entity target no longer exists — the corruption
        /// #117 leaves behind. The orphan-entity prune `DETACH DELETE`s the
        /// entity, taking the edge with it, so a surviving Concept ends up
        /// pointing at nothing. Any non-empty result is a leak.
        fn dangling_members(&self) -> Vec<(String, String, String)> {
            let members = self.members.lock().unwrap().clone();
            members
                .into_iter()
                .filter(|(_, _, entity)| !self.entity_alive(entity))
                .collect()
        }

        /// Apply one declared teardown step to this store.
        fn apply_step(&self, step: &bolt::TeardownStep, ws: &str) {
            match step.cypher {
                c if c == cypher::DELETE_WORKSPACE_CONCEPTS => {
                    // DETACH DELETE: the concepts and every edge on them.
                    self.concepts.lock().unwrap().retain(|(w, _)| w != ws);
                    self.child_of.lock().unwrap().retain(|(w, _, _)| w != ws);
                    self.members.lock().unwrap().retain(|(w, _, _)| w != ws);
                }
                c if c == cypher::DELETE_WORKSPACE_CHUNKS => {
                    self.chunks.lock().unwrap().retain(|(w, _)| w != ws);
                    self.mentions.lock().unwrap().retain(|(_, w, _)| w != ws);
                }
                c if c == cypher::DELETE_ORPHAN_ENTITIES => {
                    // Global prune of entities with no surviving mention.
                    // DETACH DELETE, so any MEMBER edge into a pruned entity
                    // dies with it — including edges from OTHER workspaces'
                    // concepts, which is why the prune is not ws-scoped.
                    let dead: HashSet<String> = self
                        .members
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(_, _, e)| e.clone())
                        .filter(|e| !self.entity_alive(e))
                        .collect();
                    self.members
                        .lock()
                        .unwrap()
                        .retain(|(_, _, e)| !dead.contains(e));
                }
                other => panic!(
                    "FakeGraph does not model this teardown step — extend the \
                     mock to match the cascade:\n{other}"
                ),
            }
        }
    }
    impl WorkspaceTeardown for FakeGraph {
        fn teardown(
            &self,
            target: TeardownTarget,
            workspace: &str,
        ) -> impl Future<Output = Reply> + Send {
            assert_eq!(
                target,
                TeardownTarget::Graph,
                "FakeGraph only models the graph target"
            );
            let ws = workspace.to_owned();
            let fail = self.fail;
            if !fail {
                // Replay the real cascade, in order.
                for step in bolt::WORKSPACE_TEARDOWN_STEPS {
                    self.apply_step(step, &ws);
                }
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

    /// A peer-service teardown sink for the #166 durability tests: records the
    /// `(target, workspace)` pairs it swept and can be told one target is
    /// unavailable (an unreachable harness). Deliberately store-agnostic — it
    /// proves the SAME drain/queue serves memory + conversations, not a second
    /// bespoke mechanism.
    #[derive(Default)]
    struct FakeSink {
        swept: Mutex<Vec<(TeardownTarget, String)>>,
        /// This target is "down": every teardown for it fails and defers.
        unavailable: Option<TeardownTarget>,
    }
    impl WorkspaceTeardown for FakeSink {
        fn teardown(
            &self,
            target: TeardownTarget,
            workspace: &str,
        ) -> impl Future<Output = Reply> + Send {
            let down = self.unavailable == Some(target);
            if !down {
                self.swept
                    .lock()
                    .unwrap()
                    .push((target, workspace.to_owned()));
            }
            async move {
                if down {
                    Reply::err_msg("unavailable", "peer service down")
                } else {
                    Reply::ok(json!({ "ok": true }))
                }
            }
        }
    }
    impl FakeSink {
        fn swept_targets(&self, ws: &str) -> Vec<TeardownTarget> {
            self.swept
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, w)| w == ws)
                .map(|(t, _)| *t)
                .collect()
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
        registry::pending::enqueue("ws-A", TeardownTarget::Graph);
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

    /// #117 — a torn-down workspace must leave NO `Concept` nodes behind.
    ///
    /// Before the fix the cascade was chunks + orphan entities only, so a
    /// deleted workspace's concepts and their `CHILD_OF` edges stayed in
    /// Memgraph forever. Worse, the orphan-entity prune `DETACH DELETE`d the
    /// entities those concepts pointed at, so the survivors were left holding
    /// `MEMBER` edges into nothing — unreachable from the panel, never
    /// re-created by a rebuild, and invisible to every other cleanup path.
    #[tokio::test]
    async fn drain_removes_concepts_and_their_edges_leaving_no_dangling_members() {
        let _env = TestEnv::new();
        let graph = FakeGraph::default();

        // ws-A: chunks mention "auth"/"token"; both are A-exclusive, so the
        // orphan prune will take them once A's chunks go.
        graph.seed_chunk("ws-A", "a0", &["auth", "token"]);
        graph.seed_chunk("ws-A", "a1", &["auth"]);
        // ...and a two-level concept hierarchy over those entities.
        graph.seed_concept("ws-A", "sem:security", &["auth", "token"], None);
        graph.seed_concept("ws-A", "sem:login", &["auth"], Some("sem:security"));

        // ws-B is untouched by the teardown and must survive intact.
        graph.seed_chunk("ws-B", "b0", &["parser"]);
        graph.seed_concept("ws-B", "sem:syntax", &["parser"], None);

        registry::pending::enqueue("ws-A", TeardownTarget::Graph);
        let report = drain(&graph).await;
        assert_eq!(report.removed, 1);
        assert_eq!(report.failed, 0);

        // The whole ws-A footprint is gone — including the concept layer.
        assert_eq!(graph.chunk_count("ws-A"), 0, "no orphan chunks survive");
        assert_eq!(
            graph.concept_count("ws-A"),
            0,
            "a torn-down workspace must not leave Concept nodes behind (#117)"
        );
        assert_eq!(
            graph.child_of_count("ws-A"),
            0,
            "CHILD_OF edges between the workspace's concepts must go with them"
        );

        // The corruption invariant: no surviving concept anywhere may point at
        // an entity the prune deleted.
        assert!(
            graph.dangling_members().is_empty(),
            "MEMBER edges left pointing at pruned entities: {:?}",
            graph.dangling_members()
        );

        // ws-B is fully intact — teardown is workspace-scoped.
        assert_eq!(graph.chunk_count("ws-B"), 1);
        assert_eq!(graph.concept_count("ws-B"), 1, "ws-B concepts untouched");
        assert!(graph.entity_alive("parser"));
        assert!(!graph.entity_alive("auth"), "A-only entity pruned");
    }

    /// The cascade must sweep concepts BEFORE the entity prune. Ordering is
    /// load-bearing, not cosmetic: the prune `DETACH DELETE`s entities, so any
    /// concept still standing at that point loses its `MEMBER` edges silently.
    #[test]
    fn teardown_sweeps_concepts_before_pruning_entities() {
        let keys: Vec<&str> = bolt::WORKSPACE_TEARDOWN_STEPS
            .iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(
            keys,
            vec![
                "concepts_deleted",
                "chunks_deleted",
                "orphan_entities_deleted"
            ],
            "teardown cascade order changed — see #117"
        );
    }

    #[tokio::test]
    async fn drain_defers_on_graph_failure_keeping_the_id_queued() {
        let _env = TestEnv::new();
        let graph = FakeGraph {
            fail: true,
            ..Default::default()
        };
        graph.seed_chunk("ws-A", "a0", &["foo"]);
        registry::pending::enqueue("ws-A", TeardownTarget::Graph);

        let report = drain(&graph).await;
        assert_eq!(report.failed, 1);
        assert_eq!(report.removed, 0);
        // Durable: a graph blip must NOT drop the cleanup — the id stays queued
        // for the next drain, and the chunk is still there to prune later.
        assert_eq!(
            registry::pending::list()
                .into_iter()
                .map(|e| e.workspace_id)
                .collect::<Vec<_>>(),
            vec!["ws-A".to_owned()]
        );
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
        let def = registry::create(&_env.ws_path("re-created"), None).unwrap();
        registry::pending::enqueue(&def.id, TeardownTarget::Graph);

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

    // ── #166: the memory + conversation sweeps ride the SAME durable queue ──

    /// #166 crit 1+2: an explicit delete's memory sweep, fired while the harness
    /// is unreachable, must be LEFT QUEUED (not lost), and a later drain against
    /// a healthy harness must complete it. This is exactly the durability the
    /// old fire-and-forget `tokio::spawn` in `handle_delete` lacked.
    #[tokio::test]
    async fn memory_sweep_defers_while_the_harness_is_down_then_drains_on_recovery() {
        let _env = TestEnv::new();
        // The delete verb enqueues the memory sweep (see `registry::delete`).
        registry::pending::enqueue("ws-gone", TeardownTarget::Memory);

        // Harness down: the memory target is unavailable.
        let down = FakeSink {
            unavailable: Some(TeardownTarget::Memory),
            ..Default::default()
        };
        let report = drain(&down).await;
        assert_eq!(report.failed, 1, "an unreachable harness defers, not drops");
        assert_eq!(report.removed, 0);
        assert!(
            down.swept_targets("ws-gone").is_empty(),
            "nothing swept yet"
        );
        // Still queued — the sweep is durable.
        assert_eq!(
            registry::pending::list(),
            vec![registry::pending::PendingEntry {
                workspace_id: "ws-gone".to_owned(),
                target: TeardownTarget::Memory,
            }]
        );

        // Harness recovers: the next drain finishes the job and dequeues it.
        let up = FakeSink::default();
        let report = drain(&up).await;
        assert_eq!(report.removed, 1);
        assert_eq!(up.swept_targets("ws-gone"), vec![TeardownTarget::Memory]);
        assert!(registry::pending::list().is_empty());
    }

    /// #166 crit 4 (the destructive case): delete a folder, re-add it — same
    /// folder-derived id (#28) — then drain. The stale memory-sweep entry must
    /// be cleared WITHOUT sweeping, so the user's fresh memories survive. This
    /// is the memory-tier analogue of `drain_skips_a_re_created_workspace_...`.
    #[tokio::test]
    async fn re_created_workspace_is_not_memory_swept() {
        let _env = TestEnv::new();
        // Deleted → memory sweep queued.
        let def = registry::create(&_env.ws_path("re-add"), None).unwrap();
        registry::pending::enqueue(&def.id, TeardownTarget::Memory);
        // ...but it is live again (the re-add already happened).
        let sink = FakeSink::default();
        let report = drain(&sink).await;

        assert_eq!(report.skipped_live, 1);
        assert_eq!(report.removed, 0);
        assert!(
            sink.swept_targets(&def.id).is_empty(),
            "a re-created workspace's fresh memories must NOT be swept (#166)"
        );
        assert!(registry::pending::list().is_empty(), "stale entry cleared");
    }

    /// #166 crit 5: one mechanism, not two. A single drain pass tears down the
    /// graph, memory, AND conversation targets for a deleted workspace — all
    /// through the one `pending`/`drain` primitive, dequeuing each as it lands.
    #[tokio::test]
    async fn one_drain_tears_down_every_peer_service_target() {
        let _env = TestEnv::new();
        registry::pending::enqueue("ws-x", TeardownTarget::Graph);
        registry::pending::enqueue("ws-x", TeardownTarget::Memory);
        registry::pending::enqueue("ws-x", TeardownTarget::Conversations);

        let sink = FakeSink::default();
        let report = drain(&sink).await;

        assert_eq!(report.removed, 3, "all three targets drained in one pass");
        let mut swept = sink.swept_targets("ws-x");
        swept.sort();
        assert_eq!(
            swept,
            vec![
                TeardownTarget::Graph,
                TeardownTarget::Memory,
                TeardownTarget::Conversations,
            ]
        );
        assert!(registry::pending::list().is_empty());
    }
}
