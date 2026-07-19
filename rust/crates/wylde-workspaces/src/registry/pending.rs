//! Durable pending-graph-cleanup queue (#99).
//!
//! When a workspace is torn down — an explicit `workspaces.delete` OR an MRU
//! eviction — its on-disk bundle is removed synchronously, but its Memgraph
//! footprint (Chunk nodes + now-orphan Entity nodes) must be pruned by an
//! async, possibly-unreachable graph write. A fire-and-forget prune silently
//! orphans the whole workspace's graph data on any transient graph blip (the
//! exact rot #99 names).
//!
//! This is the durable half of the fix: [`super::teardown_bundle`] — the ONE
//! primitive every removal path funnels through — [`enqueue`]s the id here
//! before the async prune ever runs. The prune ([`crate::graph::cleanup`])
//! [`remove`]s an id only once its graph teardown has actually succeeded, so a
//! graph outage leaves the id queued for the next drain (on the next
//! create/activate/delete, or at boot) instead of orphaning forever.
//!
//! Persisted to `<data_dir>/workspaces/pending_graph_cleanup.json`,
//! encrypt-at-rest (OI-14) like the registry index. A process-wide lock
//! serialises the read-modify-write so a concurrent enqueue and drain can't
//! tear the set.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use super::persistence::workspaces_dir;

/// `<data_dir>/workspaces/pending_graph_cleanup.json`.
fn pending_path() -> PathBuf {
    workspaces_dir().join("pending_graph_cleanup.json")
}

/// Serialise every read-modify-write of the pending set. The critical section
/// is pure sync IO (no `.await`), so a `std::sync::Mutex` is correct here.
static LOCK: Mutex<()> = Mutex::new(());

fn load_locked() -> BTreeSet<String> {
    match wylde_shared::encryption::read_to_string_at_rest(&pending_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => BTreeSet::new(),
    }
}

fn save_locked(set: &BTreeSet<String>) {
    let body = serde_json::to_string_pretty(set).unwrap_or_else(|_| "[]".to_owned());
    if let Err(e) = wylde_shared::encryption::write_at_rest(&pending_path(), body.as_bytes()) {
        tracing::warn!("workspaces.cleanup: persist pending set failed: {e}");
    }
}

/// Enqueue a workspace id for durable graph teardown. Idempotent (a set), so a
/// re-enqueue of an already-pending id is a cheap no-op.
pub fn enqueue(id: &str) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut set = load_locked();
    if set.insert(id.to_owned()) {
        save_locked(&set);
    }
}

/// Remove a workspace id once its graph teardown has landed (or it is no
/// longer an orphan). No-op if it wasn't queued.
pub fn remove(id: &str) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut set = load_locked();
    if set.remove(id) {
        save_locked(&set);
    }
}

/// A snapshot of the pending ids (sorted, deduped).
pub fn list() -> Vec<String> {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    load_locked().into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn enqueue_is_idempotent_and_persists() {
        let _env = TestEnv::new();
        assert!(list().is_empty());
        enqueue("ws-a");
        enqueue("ws-a"); // dup — still one entry
        enqueue("ws-b");
        let ids = list();
        assert_eq!(ids, vec!["ws-a".to_owned(), "ws-b".to_owned()]);
    }

    #[test]
    fn remove_dequeues_only_the_named_id() {
        let _env = TestEnv::new();
        enqueue("ws-a");
        enqueue("ws-b");
        remove("ws-a");
        assert_eq!(list(), vec!["ws-b".to_owned()]);
        // Removing an absent id is a no-op.
        remove("ws-a");
        remove("nope");
        assert_eq!(list(), vec!["ws-b".to_owned()]);
    }

    #[test]
    fn list_is_empty_when_file_absent() {
        let _env = TestEnv::new();
        assert!(list().is_empty());
    }
}
