//! Per-workspace index write-lock (content-hash manifest P3, §3.3).
//!
//! A manual `reindex` can race the file-watcher's per-file delta — both
//! rewrite `chunks.jsonl` + `manifest.json`. The chunks-then-manifest write
//! pair MUST stay atomic relative to each other (a manifest that ran ahead of
//! its chunks would *skip* a needed embed and leave a missing vector), so every
//! writer holds the same per-workspace mutex across the whole pair.
//!
//! The lock is a process-wide registry of `tokio::Mutex`es keyed by workspace
//! id, so it serialises in-process writers without a lock file. (Cross-process
//! contention isn't a concern: one service process owns a workspace's index.)
//! It is a `tokio::Mutex` rather than `std::sync::Mutex` because the critical
//! section spans `.await` points — the embed + the graph round-trips happen
//! before the persist, and a guard held across an await would deadlock a
//! `std::sync::Mutex`'s single-thread assumption.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Mutex as AsyncMutex;

/// The process-wide map of per-workspace index mutexes.
fn registry() -> &'static Mutex<HashMap<String, Arc<AsyncMutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The index mutex for `workspace_id`, creating it on first use. Callers
/// `.lock().await` the returned handle and hold the guard across the whole
/// chunks-then-manifest persist so a concurrent writer can't tear the pair.
pub fn for_workspace(workspace_id: &str) -> Arc<AsyncMutex<()>> {
    let mut map = registry().lock().unwrap_or_else(|p| p.into_inner());
    map.entry(workspace_id.to_owned())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_id_shares_one_lock_distinct_ids_differ() {
        let a1 = for_workspace("ws-lock-a");
        let a2 = for_workspace("ws-lock-a");
        let b = for_workspace("ws-lock-b");
        assert!(Arc::ptr_eq(&a1, &a2), "same id reuses the one mutex");
        assert!(!Arc::ptr_eq(&a1, &b), "distinct ids get distinct mutexes");
    }

    #[tokio::test]
    async fn lock_serialises_holders() {
        let lock = for_workspace("ws-lock-serial");
        let g = lock.clone().lock_owned().await;
        // A second acquire can't proceed while the first guard is held.
        assert!(lock.try_lock().is_err(), "held lock blocks a second acquire");
        drop(g);
        assert!(lock.try_lock().is_ok(), "released lock is acquirable again");
    }
}
