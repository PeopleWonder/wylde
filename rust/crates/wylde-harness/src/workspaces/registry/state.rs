//! [`WorkspaceState`] — active-workspace pointer + MRU list.
//!
//! Persisted to `<data_dir>/workspaces/index.json` (the registry
//! index), mirroring the `active_conversation.json` pattern in
//! [`crate::memory::conversations::store`]. The MRU list drives the
//! "MRU-5 dropdown" in the InferenceBar.
//!
//! Kept separate from [`super::definition`] so activating a workspace
//! (a hot, frequent write) doesn't rewrite a workspace's config.
//!
//! ## Static MRU-5 (Q2)
//!
//! [`MRU_WINDOW`] is a hard-coded constant — the redesign deliberately
//! drops the legacy user-configurable `mru_limit` + `set_mru_limit`
//! verb. Changing the window is a one-line edit here. Because eviction
//! past the window is the only way a workspace leaves the registry, the
//! `mru` list is also the authoritative set of workspaces on disk.

use serde::{Deserialize, Serialize};

use crate::memory::common::ensure_dir;

/// MRU window the InferenceBar dropdown shows, and the hard cap on how
/// many workspaces the registry retains. Static (Q2): if this ever needs
/// to change it's a one-line edit.
pub const MRU_WINDOW: usize = 5;

/// The mutable selection state, distinct from the per-workspace config
/// records.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceState {
    /// Currently-active workspace id, or `None` for "no workspace"
    /// (a plain chat turn injects no workspace context).
    #[serde(default)]
    pub active_id: Option<String>,

    /// Most-recently-used workspace ids, newest first. The dropdown
    /// renders the first [`MRU_WINDOW`]; eviction keeps the list at most
    /// `MRU_WINDOW` long.
    #[serde(default)]
    pub mru: Vec<String>,
}

impl WorkspaceState {
    /// Move `id` to the MRU head and mark it active. Returns the ids
    /// evicted past the static [`MRU_WINDOW`] (the caller removes their
    /// on-disk bundles).
    pub fn promote(&mut self, id: &str) -> Vec<String> {
        self.mru.retain(|x| x != id);
        self.mru.insert(0, id.to_owned());
        self.active_id = Some(id.to_owned());
        self.evict_overflow()
    }

    /// Forget `id`: drop it from the MRU and clear the active pointer if
    /// it referenced `id`.
    pub fn forget(&mut self, id: &str) {
        self.mru.retain(|x| x != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
    }

    fn evict_overflow(&mut self) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.mru.len() > MRU_WINDOW {
            if let Some(victim) = self.mru.pop() {
                if self.active_id.as_deref() == Some(victim.as_str()) {
                    self.active_id = None;
                }
                evicted.push(victim);
            }
        }
        evicted
    }
}

/// `<data_dir>/workspaces/index.json`.
pub fn index_path() -> std::path::PathBuf {
    super::persistence::workspaces_dir().join("index.json")
}

/// Read the persisted state (active pointer + MRU). Folds any read error
/// to [`WorkspaceState::default`].
pub fn load() -> WorkspaceState {
    let Ok(raw) = std::fs::read_to_string(index_path()) else {
        return WorkspaceState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist `state` (best-effort, atomic temp + rename).
pub fn save(state: &WorkspaceState) -> std::io::Result<()> {
    let dir = super::persistence::workspaces_dir();
    ensure_dir(&dir)?;
    let path = index_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state).unwrap())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;

    #[test]
    fn promote_moves_to_head_and_sets_active() {
        let mut s = WorkspaceState::default();
        assert!(s.promote("a").is_empty());
        assert!(s.promote("b").is_empty());
        assert_eq!(s.mru, vec!["b", "a"]);
        assert_eq!(s.active_id.as_deref(), Some("b"));

        // Re-promoting an existing id moves it to head (no dup).
        assert!(s.promote("a").is_empty());
        assert_eq!(s.mru, vec!["a", "b"]);
        assert_eq!(s.active_id.as_deref(), Some("a"));
    }

    #[test]
    fn promote_evicts_past_static_mru_window() {
        let mut s = WorkspaceState::default();
        for i in 0..MRU_WINDOW {
            assert!(s.promote(&format!("w{i}")).is_empty());
        }
        assert_eq!(s.mru.len(), MRU_WINDOW);
        // The 6th distinct workspace evicts the least-recently-used (w0).
        let evicted = s.promote("w-new");
        assert_eq!(evicted, vec!["w0".to_owned()]);
        assert_eq!(s.mru.len(), MRU_WINDOW);
        assert_eq!(s.mru[0], "w-new");
        assert!(!s.mru.contains(&"w0".to_owned()));
    }

    #[test]
    fn forget_drops_id_and_clears_active() {
        let mut s = WorkspaceState::default();
        s.promote("a");
        s.promote("b");
        s.forget("b");
        assert_eq!(s.mru, vec!["a"]);
        assert!(s.active_id.is_none());
        // Forgetting a non-active id leaves active intact.
        s.promote("c");
        s.forget("a");
        assert_eq!(s.active_id.as_deref(), Some("c"));
    }

    #[test]
    fn save_then_load_round_trips_through_index_json() {
        let _env = TestEnv::new();
        let mut s = WorkspaceState::default();
        s.promote("x");
        s.promote("y");
        save(&s).unwrap();
        assert_eq!(load(), s);
    }

    #[test]
    fn load_is_default_when_index_absent() {
        let _env = TestEnv::new();
        assert_eq!(load(), WorkspaceState::default());
    }
}
