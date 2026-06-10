//! Load + render a workspace's persona override.
//!
//! The override is plain markdown at
//! `<data_dir>/workspaces/<workspace_id>/persona.md` so a user can
//! hand-edit it. It supersedes the inline `persona` string the retired
//! verb-driven store carried (design doc migration §8).

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
/// when the file is absent or unreadable. Decrypts at rest (OI-14).
pub fn load(workspace_id: &str) -> PersonaOverride {
    let text = wylde_shared::encryption::read_to_string_at_rest(&persona_path(workspace_id))
        .unwrap_or_default();
    PersonaOverride { text }
}

/// Persist persona text for a workspace — encrypt-at-rest (OI-14) + atomic
/// write of `persona.md`, creating the bundle dir if needed.
pub fn save(workspace_id: &str, text: &str) -> std::io::Result<()> {
    let path = super::super::registry::persistence::workspace_dir(workspace_id).join("persona.md");
    wylde_shared::encryption::write_at_rest(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

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
