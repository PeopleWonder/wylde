//! Load + render a workspace's persona override.
//!
//! The override is plain markdown at
//! `<data_dir>/workspaces/<workspace_id>/persona.md` so a user can
//! hand-edit it. It supersedes the inline `persona` string the retired
//! verb-driven store carried (design doc migration §8).

use crate::memory::common::ensure_dir;

/// A rendered persona ready to fold into the system prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersonaOverride {
    /// The persona text (markdown body of `persona.md`). Empty means
    /// "no override" — the prompt builder skips the slot.
    pub text: String,
}

impl PersonaOverride {
    /// True when there is no persona to inject.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// `<data_dir>/workspaces/<workspace_id>/persona.md`.
pub fn persona_path(workspace_id: &str) -> std::path::PathBuf {
    super::super::registry::persistence::workspace_dir(workspace_id).join("persona.md")
}

/// Load the persona override for a workspace. Returns an empty override
/// when the file is absent or unreadable.
pub fn load(workspace_id: &str) -> PersonaOverride {
    let text = std::fs::read_to_string(persona_path(workspace_id)).unwrap_or_default();
    PersonaOverride { text }
}

/// Persist persona text for a workspace (writes `persona.md` atomically,
/// creating the bundle dir if needed).
pub fn save(workspace_id: &str, text: &str) -> std::io::Result<()> {
    let dir = super::super::registry::persistence::workspace_dir(workspace_id);
    ensure_dir(&dir)?;
    let path = dir.join("persona.md");
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;

    #[test]
    fn save_then_load_round_trips() {
        let _env = TestEnv::new();
        let ws = "ws-persona-000000";
        save(ws, "You are a terse Rust reviewer.").unwrap();
        let p = load(ws);
        assert_eq!(p.text, "You are a terse Rust reviewer.");
        assert!(!p.is_empty());
    }

    #[test]
    fn load_is_empty_when_absent() {
        let _env = TestEnv::new();
        let p = load("nope-000000");
        assert!(p.is_empty());
        assert_eq!(p, PersonaOverride::default());
    }

    #[test]
    fn save_overwrites_prior_text() {
        let _env = TestEnv::new();
        let ws = "ws-persona-overwrite";
        save(ws, "first").unwrap();
        save(ws, "second").unwrap();
        assert_eq!(load(ws).text, "second");
    }
}
