//! Durable pending-teardown queue (#99, generalized in #166).
//!
//! When a workspace is torn down — an explicit `workspaces.delete` OR an MRU
//! eviction — its on-disk bundle is removed synchronously, but the parts that
//! live in **peer services** must be pruned by an async, possibly-unreachable
//! call: its Memgraph footprint (Chunk + now-orphan Entity + `Concept` nodes),
//! its durable workspace-memory tier (`<data_dir>/workspace_memories/<id>/`,
//! #135), and its bound flat-store conversations. A fire-and-forget prune
//! silently orphans that data on any transient blip — the exact rot #99 named
//! for the graph, and #166 named again for the memory tier.
//!
//! This is the durable half of the fix, and there is exactly **one** of it:
//! every removal path enqueues `(workspace id, teardown target)` pairs here
//! before the async prune ever runs, and the drain ([`crate::graph::cleanup`])
//! [`remove`]s a pair only once that target's teardown has actually succeeded.
//! An outage of any one target leaves its pair queued for the next drain (on
//! the next create/activate/delete, or at boot) instead of orphaning forever —
//! and because a re-created workspace reuses its folder-derived id (#28), a
//! pair whose workspace is live again is dequeued *without* being swept, so a
//! delete-then-re-add can never wipe the fresh data.
//!
//! Generalizing to a target rather than standing up a second queue is
//! deliberate (#166): a future fourth peer-service sweep inherits durability by
//! adding a [`TeardownTarget`] variant, not by re-implementing the drain.
//!
//! Persisted to `<data_dir>/workspaces/pending_teardown.json`, encrypt-at-rest
//! (OI-14) like the registry index. A process-wide lock serialises the
//! read-modify-write so a concurrent enqueue and drain can't tear the set. The
//! #99 bare-id file (`pending_graph_cleanup.json`, graph-only) is migrated in
//! place on first read and removed on first write — see [`load_locked`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::persistence::workspaces_dir;

/// Which peer-service store a queued teardown targets. Adding a variant is how
/// a new peer-service sweep inherits the durable drain (#166) — the drain
/// dispatches on this and applies the same dequeue-on-ok / defer-on-failure /
/// skip-if-live rule to every target uniformly.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeardownTarget {
    /// The Memgraph footprint: `Concept`, Chunk, and now-orphan Entity nodes
    /// (#99/#117). Enqueued by BOTH explicit delete and MRU eviction.
    #[serde(rename = "graph")]
    Graph,
    /// The durable workspace-memory tier `<data_dir>/workspace_memories/<id>/`
    /// (#135). Enqueued ONLY on explicit delete — eviction must PRESERVE it.
    #[serde(rename = "memory")]
    Memory,
    /// The workspace's bound conversations in the harness flat store (Route 1).
    /// Enqueued ONLY on explicit delete, same as [`Memory`](Self::Memory).
    #[serde(rename = "conversations")]
    Conversations,
}

/// One queued teardown: a workspace id awaiting a specific target's prune.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PendingEntry {
    pub workspace_id: String,
    pub target: TeardownTarget,
}

impl PendingEntry {
    /// A graph-target entry — the shape the #99 bare-id file migrates into.
    fn graph(workspace_id: String) -> Self {
        Self {
            workspace_id,
            target: TeardownTarget::Graph,
        }
    }
}

/// `<data_dir>/workspaces/pending_teardown.json` — the generalized (#166) queue.
fn pending_path() -> PathBuf {
    workspaces_dir().join("pending_teardown.json")
}

/// `<data_dir>/workspaces/pending_graph_cleanup.json` — the #99 bare-id queue,
/// graph-only. Read once for migration, then removed. See [`load_locked`].
fn legacy_path() -> PathBuf {
    workspaces_dir().join("pending_graph_cleanup.json")
}

/// Serialise every read-modify-write of the pending set. The critical section
/// is pure sync IO (no `.await`), so a `std::sync::Mutex` is correct here.
static LOCK: Mutex<()> = Mutex::new(());

/// Load the pending set, migrating the #99 bare-id queue if that is all that
/// exists. Precedence: the generalized file wins; only when it is absent do we
/// read the legacy bare-id file and map every id to a graph-target entry (the
/// only target #99 ever queued). The legacy file is dropped by [`save_locked`]
/// on the next mutation, so this migration runs at most until the first write.
fn load_locked() -> BTreeSet<PendingEntry> {
    if let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&pending_path()) {
        if let Ok(set) = serde_json::from_str::<BTreeSet<PendingEntry>>(&raw) {
            return set;
        }
        // Defensive: a generalized-path file that won't parse as entries might
        // be a stray legacy bare-id list — try that shape before giving up.
        if let Ok(ids) = serde_json::from_str::<Vec<String>>(&raw) {
            return ids.into_iter().map(PendingEntry::graph).collect();
        }
        return BTreeSet::new();
    }
    // No generalized file yet — migrate the #99 bare-id queue if present.
    if let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&legacy_path()) {
        if let Ok(ids) = serde_json::from_str::<Vec<String>>(&raw) {
            return ids.into_iter().map(PendingEntry::graph).collect();
        }
    }
    BTreeSet::new()
}

fn save_locked(set: &BTreeSet<PendingEntry>) {
    let body = serde_json::to_string_pretty(set).unwrap_or_else(|_| "[]".to_owned());
    if let Err(e) = wylde_shared::encryption::write_at_rest(&pending_path(), body.as_bytes()) {
        tracing::warn!("workspaces.cleanup: persist pending set failed: {e}");
        return;
    }
    // Migration is complete once the generalized file is on disk: drop the #99
    // bare-id file so a later read can't resurrect already-drained ids from it.
    let legacy = legacy_path();
    if legacy.exists() {
        if let Err(e) = std::fs::remove_file(&legacy) {
            tracing::warn!("workspaces.cleanup: could not drop legacy pending file: {e}");
        }
    }
}

/// Enqueue a workspace id for durable teardown of one target. Idempotent (a
/// set), so a re-enqueue of an already-pending `(id, target)` is a cheap no-op.
pub fn enqueue(id: &str, target: TeardownTarget) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut set = load_locked();
    if set.insert(PendingEntry {
        workspace_id: id.to_owned(),
        target,
    }) {
        save_locked(&set);
    }
}

/// Remove a `(id, target)` pair once its teardown has landed (or it is no
/// longer an orphan). No-op if it wasn't queued.
pub fn remove(id: &str, target: TeardownTarget) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut set = load_locked();
    if set.remove(&PendingEntry {
        workspace_id: id.to_owned(),
        target,
    }) {
        save_locked(&set);
    }
}

/// A snapshot of the pending entries (sorted, deduped).
pub fn list() -> Vec<PendingEntry> {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    load_locked().into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn ids_for(target: TeardownTarget) -> Vec<String> {
        list()
            .into_iter()
            .filter(|e| e.target == target)
            .map(|e| e.workspace_id)
            .collect()
    }

    #[test]
    fn enqueue_is_idempotent_and_persists() {
        let _env = TestEnv::new();
        assert!(list().is_empty());
        enqueue("ws-a", TeardownTarget::Graph);
        enqueue("ws-a", TeardownTarget::Graph); // dup — still one entry
        enqueue("ws-b", TeardownTarget::Graph);
        assert_eq!(ids_for(TeardownTarget::Graph), vec!["ws-a", "ws-b"]);
    }

    #[test]
    fn the_same_id_can_queue_distinct_targets_independently() {
        let _env = TestEnv::new();
        // One workspace, three peer-service sweeps — three independent entries.
        enqueue("ws-a", TeardownTarget::Graph);
        enqueue("ws-a", TeardownTarget::Memory);
        enqueue("ws-a", TeardownTarget::Conversations);
        assert_eq!(list().len(), 3);
        // Draining one target must not disturb the others.
        remove("ws-a", TeardownTarget::Graph);
        assert_eq!(ids_for(TeardownTarget::Graph), Vec::<String>::new());
        assert_eq!(ids_for(TeardownTarget::Memory), vec!["ws-a"]);
        assert_eq!(ids_for(TeardownTarget::Conversations), vec!["ws-a"]);
    }

    #[test]
    fn remove_dequeues_only_the_named_pair() {
        let _env = TestEnv::new();
        enqueue("ws-a", TeardownTarget::Graph);
        enqueue("ws-b", TeardownTarget::Graph);
        remove("ws-a", TeardownTarget::Graph);
        assert_eq!(ids_for(TeardownTarget::Graph), vec!["ws-b"]);
        // Removing an absent pair is a no-op.
        remove("ws-a", TeardownTarget::Graph);
        remove("nope", TeardownTarget::Graph);
        assert_eq!(ids_for(TeardownTarget::Graph), vec!["ws-b"]);
    }

    #[test]
    fn list_is_empty_when_file_absent() {
        let _env = TestEnv::new();
        assert!(list().is_empty());
    }

    #[test]
    fn migrates_the_legacy_bare_id_graph_queue_in_place() {
        let _env = TestEnv::new();
        // Simulate a pre-#166 on-disk queue: bare ids at the #99 path.
        let legacy = legacy_path();
        wylde_shared::encryption::write_at_rest(&legacy, b"[\"ws-old-a\",\"ws-old-b\"]").unwrap();
        // Read migrates them to graph-target entries.
        assert_eq!(ids_for(TeardownTarget::Graph), vec!["ws-old-a", "ws-old-b"]);
        // The legacy ids are still only graph-target (no spurious sweeps).
        assert!(ids_for(TeardownTarget::Memory).is_empty());
        // A mutation persists the generalized file AND drops the legacy one.
        enqueue("ws-new", TeardownTarget::Memory);
        assert!(pending_path().exists(), "generalized file written");
        assert!(
            !legacy.exists(),
            "legacy bare-id file dropped after migration"
        );
        // Everything survived the migration.
        assert_eq!(ids_for(TeardownTarget::Graph), vec!["ws-old-a", "ws-old-b"]);
        assert_eq!(ids_for(TeardownTarget::Memory), vec!["ws-new"]);
    }

    #[test]
    fn entries_are_durable_across_a_reload() {
        let _env = TestEnv::new();
        enqueue("ws-a", TeardownTarget::Memory);
        // No in-memory cache — every `list()` re-reads the encrypted file, so a
        // fresh read is exactly what a process restart would see (#166 crit 3).
        assert!(pending_path().exists());
        let reread = list();
        assert_eq!(reread.len(), 1);
        assert_eq!(reread[0].workspace_id, "ws-a");
        assert_eq!(reread[0].target, TeardownTarget::Memory);
    }
}
