//! [`WorkspaceState`] — active-workspace pointer + MRU list.
//!
//! Persisted to `<data_dir>/workspaces/index.json` (the registry
//! index), mirroring the `active_conversation.json` pattern in
//! the harness `memory::conversations::store`. The MRU list drives the
//! "MRU-5 dropdown" in the InferenceBar.
//!
//! Kept separate from [`super::definition`] so activating a workspace
//! (a hot, frequent write) doesn't rewrite a workspace's config.
//!
//! ## MRU window is display-only (Q2 / #133)
//!
//! [`MRU_WINDOW`] is a hard-coded constant — the redesign deliberately
//! drops the legacy user-configurable `mru_limit` + `set_mru_limit`
//! verb. Changing the window is a one-line edit here.
//!
//! It bounds only *how many* workspaces the InferenceBar dropdown renders;
//! it is **not** a cap on how many exist. The `mru` list is the full,
//! unbounded enumeration of every workspace on disk, kept in recency order.
//! A workspace leaves the registry **only** by explicit `delete` — never by
//! being pushed past the window. (#133: the old code `remove_dir_all`'d the
//! least-recently-used bundle when a 6th workspace was registered, silently
//! destroying its persona, memory, RAG index and conversations.)

use serde::{Deserialize, Serialize};

/// How many workspaces the InferenceBar dropdown shows (the MRU window).
/// A *display* bound only — not a cap on how many workspaces exist (#133).
/// Static (Q2): if this ever needs to change it's a one-line edit.
pub const MRU_WINDOW: usize = 5;

/// The mutable selection state, distinct from the per-workspace config
/// records.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceState {
    /// Currently-active workspace id, or `None` for "no workspace"
    /// (a plain chat turn injects no workspace context).
    #[serde(default)]
    pub active_id: Option<String>,

    /// Every workspace id that exists on disk, newest-used first. This is
    /// the authoritative enumeration and is **unbounded** — the dropdown
    /// renders only the first [`MRU_WINDOW`], but the list is never trimmed
    /// automatically (a workspace leaves only via explicit `delete`, #133).
    #[serde(default)]
    pub mru: Vec<String>,
}

impl WorkspaceState {
    /// Move `id` to the MRU head and mark it active.
    ///
    /// The `mru` list is the *full, unbounded* enumeration of workspaces
    /// that exist on disk (#133): promotion re-orders it but never drops an
    /// entry, so a 6th (or 600th) workspace can never auto-destroy an older
    /// one's bundle. [`MRU_WINDOW`] governs only how many the dropdown shows
    /// (see [`super::list_mru`]) — it is a *display* window, not a disk cap.
    pub fn promote(&mut self, id: &str) {
        self.mru.retain(|x| x != id);
        self.mru.insert(0, id.to_owned());
        self.active_id = Some(id.to_owned());
    }

    /// Forget `id`: drop it from the MRU and clear the active pointer if
    /// it referenced `id`.
    pub fn forget(&mut self, id: &str) {
        self.mru.retain(|x| x != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
    }
}

/// `<data_dir>/workspaces/index.json`.
pub fn index_path() -> std::path::PathBuf {
    super::persistence::workspaces_dir().join("index.json")
}

/// Why [`load`] could not produce the persisted state.
///
/// Deliberately distinct from "there is no state yet": an ABSENT `index.json`
/// is the legitimate first-run case and reads as an empty
/// [`WorkspaceState`], whereas a file that exists but cannot be read,
/// decrypted, or parsed means the registry's index is damaged and its
/// contents are unknown — NOT that the user has no workspaces.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// `index.json` exists but could not be read or decrypted. Typically a
    /// permissions problem, a torn write, or DPAPI failing to unprotect
    /// (e.g. the file was copied from another machine or user profile).
    #[error("workspace index at {path} is unreadable: {source}")]
    Unreadable {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `index.json` was read and decrypted but is not valid state JSON —
    /// a torn or truncated write.
    #[error("workspace index at {path} is corrupt: {source}")]
    Corrupt {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Read the persisted state, distinguishing "no index yet" from "the index
/// is damaged". Decrypts at rest (OI-14).
///
/// Returns `Ok(default)` when `index.json` is absent — the first-run case.
/// Returns `Err` when the file is present but unreadable/undecryptable
/// (`Unreadable`) or unparseable (`Corrupt`).
///
/// # Why this is not fail-open (#140)
///
/// This function used to fold **every** failure — read, decrypt, and parse —
/// into `WorkspaceState::default()`. That made a damaged index
/// indistinguishable from a brand-new install: the app presented "no
/// workspaces", which is alarming but by itself recoverable, since the bytes
/// were still on disk.
///
/// The destructive part was what happened next. Every mutating path is
/// `load()` → mutate → [`save`], so the first activation or delete after a
/// torn read would write the *empty-plus-one* state straight over the file
/// that still held the real MRU. Since the MRU list is the authoritative set
/// of workspaces the registry retains, that turned a recoverable file problem
/// into permanent loss of every other workspace's registration. Failing here
/// is what stops that sequence at step one; [`save`] refuses to clobber a
/// damaged index as a second, independent guard.
pub fn load() -> Result<WorkspaceState, StateError> {
    let path = index_path();
    let raw = match wylde_shared::encryption::read_to_string_at_rest(&path) {
        Ok(raw) => raw,
        // Absent is not damage — this is a fresh install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorkspaceState::default()),
        Err(source) => {
            tracing::error!(
                "workspaces.registry: index {} is unreadable ({source}); refusing to \
                 present an empty registry — the real workspace list is still on disk",
                path.display(),
            );
            return Err(StateError::Unreadable { path, source });
        }
    };
    serde_json::from_str(&raw).map_err(|source| {
        tracing::error!(
            "workspaces.registry: index {} is corrupt ({source}); refusing to \
             present an empty registry — the real workspace list is still on disk",
            path.display(),
        );
        StateError::Corrupt { path, source }
    })
}

/// [`load`] for the read-only consumers that only want "which workspace is
/// active" and have no way to surface an error — the file watcher and the
/// symbol index.
///
/// A damaged index degrades these to "no active workspace", which is the
/// correct conservative answer for a consumer that merely stops watching or
/// drops an in-memory index. It is still logged at ERROR by [`load`].
///
/// **Never call this on a path that writes state back.** Doing so is exactly
/// the fail-open sequence #140 was about: a defaulted read followed by a save
/// overwrites the real index. Mutating paths must use [`load`] and propagate.
pub fn load_or_default() -> WorkspaceState {
    load().unwrap_or_default()
}

/// Persist `state` (encrypt-at-rest, OI-14; atomic temp + rename).
///
/// Refuses to write when the on-disk index is present but damaged, returning
/// `InvalidData` without touching the file. This is the second guard for
/// #140, and it is independent of the caller: even a path that (wrongly)
/// defaulted a failed read cannot destroy the bytes still sitting on disk.
/// The damaged file is preserved for recovery rather than overwritten.
pub fn save(state: &WorkspaceState) -> std::io::Result<()> {
    if let Err(e) = load() {
        tracing::error!(
            "workspaces.registry: refusing to overwrite a damaged index ({e}); \
             the existing file is preserved for recovery"
        );
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
    }
    let body = serde_json::to_string_pretty(state).unwrap();
    wylde_shared::encryption::write_at_rest(&index_path(), body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn promote_moves_to_head_and_sets_active() {
        let mut s = WorkspaceState::default();
        s.promote("a");
        s.promote("b");
        assert_eq!(s.mru, vec!["b", "a"]);
        assert_eq!(s.active_id.as_deref(), Some("b"));

        // Re-promoting an existing id moves it to head (no dup).
        s.promote("a");
        assert_eq!(s.mru, vec!["a", "b"]);
        assert_eq!(s.active_id.as_deref(), Some("a"));
    }

    /// #133 regression: promotion past the display window must NEVER drop an
    /// id. Before the fix the 6th `promote` evicted (and the caller destroyed)
    /// the least-recently-used workspace; now the enumeration is unbounded.
    #[test]
    fn promote_past_window_keeps_full_unbounded_enumeration() {
        let mut s = WorkspaceState::default();
        for i in 0..MRU_WINDOW {
            s.promote(&format!("w{i}"));
        }
        assert_eq!(s.mru.len(), MRU_WINDOW);
        // Register a 6th (and 7th) distinct workspace.
        s.promote("w-new");
        s.promote("w-newer");
        // Nothing is evicted: the list holds every id, newest-first.
        assert_eq!(s.mru.len(), MRU_WINDOW + 2);
        assert_eq!(s.mru[0], "w-newer");
        assert_eq!(s.mru[1], "w-new");
        // The least-recently-used original is still enumerated (not destroyed).
        assert!(s.mru.contains(&"w0".to_owned()));
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
        assert_eq!(load().unwrap(), s);
    }

    #[test]
    fn load_is_default_when_index_absent() {
        let _env = TestEnv::new();
        assert_eq!(load().unwrap(), WorkspaceState::default());
    }

    /// Write a real index, then corrupt it on disk the way a torn write would.
    fn seed_then_corrupt(bytes: &[u8]) -> WorkspaceState {
        let mut s = WorkspaceState::default();
        s.promote("ws-c");
        s.promote("ws-b");
        s.promote("ws-a");
        save(&s).unwrap();
        std::fs::write(index_path(), bytes).unwrap();
        s
    }

    /// #140 — a damaged index must NOT read as "no workspaces".
    ///
    /// Absent and damaged were previously the same answer (`default()`), which
    /// is what made a recoverable file problem present as total data loss.
    #[test]
    fn load_fails_loudly_on_a_corrupt_index_instead_of_reading_empty() {
        let _env = TestEnv::new();
        seed_then_corrupt(b"{\"active_id\": \"ws-a\", \"mru\": [\"ws-a\",");

        let err = load().expect_err("a truncated index must not read as empty");
        assert!(
            matches!(err, StateError::Corrupt { .. }),
            "expected Corrupt, got {err:?}"
        );
    }

    /// The half that turns the problem from "alarming" into "unrecoverable":
    /// every mutating path is load → mutate → save, so a defaulted read used
    /// to overwrite the file that still held the real MRU. `save` refuses.
    #[test]
    fn save_refuses_to_overwrite_a_damaged_index() {
        let _env = TestEnv::new();
        let torn = b"{\"active_id\": \"ws-a\", \"mru\": [\"ws-a\",".to_vec();
        seed_then_corrupt(&torn);

        // Simulate the old fail-open sequence explicitly: a caller that
        // defaulted the read and is about to persist a near-empty state.
        let mut defaulted = WorkspaceState::default();
        defaulted.promote("ws-newly-activated");
        let err = save(&defaulted).expect_err("must refuse to clobber a damaged index");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        // The damaged CONTENT is still there and recoverable.
        //
        // Note this compares decrypted content, not raw bytes: the at-rest
        // layer transparently re-writes a plaintext file as encrypted when it
        // reads one, so the on-disk bytes legitimately change without the
        // content being touched. What must hold is that the user's data is
        // still there to recover — not that the file is byte-identical.
        let on_disk = wylde_shared::encryption::read_to_string_at_rest(&index_path()).unwrap();
        assert_eq!(
            on_disk.as_bytes(),
            torn.as_slice(),
            "the damaged index content must be preserved for recovery, not overwritten"
        );
    }

    /// Recovery: once the file is fixed (or removed), everything works again
    /// and the real MRU is intact — proving the data was never destroyed.
    #[test]
    fn a_repaired_index_loads_its_original_contents() {
        let _env = TestEnv::new();
        let real = seed_then_corrupt(b"not json at all");
        assert!(load().is_err());

        // "Repair" = restore the good bytes.
        save_forced(&real);
        let recovered = load().unwrap();
        assert_eq!(recovered, real);
        assert_eq!(recovered.mru, vec!["ws-a", "ws-b", "ws-c"]);
    }

    /// Test-only: write bypassing the damage guard, standing in for the user
    /// repairing or restoring `index.json` out of band.
    fn save_forced(state: &WorkspaceState) {
        let body = serde_json::to_string_pretty(state).unwrap();
        wylde_shared::encryption::write_at_rest(&index_path(), body.as_bytes()).unwrap();
    }

    /// `load_or_default` is the deliberate escape hatch for read-only
    /// consumers — it must still degrade quietly, so the watcher and symbol
    /// index don't panic on a damaged index.
    #[test]
    fn load_or_default_still_degrades_for_read_only_consumers() {
        let _env = TestEnv::new();
        seed_then_corrupt(b"{{{");
        assert_eq!(load_or_default(), WorkspaceState::default());
    }
}
