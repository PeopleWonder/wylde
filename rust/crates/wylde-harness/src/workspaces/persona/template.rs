//! Load + render a workspace's persona override.
//!
//! Scaffold only — no logic yet.

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
pub fn persona_path(_workspace_id: &str) -> std::path::PathBuf {
    todo!("workspaces redesign: persona_path")
}

/// Load the persona override for a workspace. Returns an empty override
/// when the file is absent.
pub fn load(_workspace_id: &str) -> PersonaOverride {
    todo!("workspaces redesign: load persona")
}

/// Persist persona text for a workspace (writes `persona.md`).
pub fn save(_workspace_id: &str, _text: &str) -> std::io::Result<()> {
    todo!("workspaces redesign: save persona")
}
