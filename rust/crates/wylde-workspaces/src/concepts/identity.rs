//! Persisted **never-reused** `sem:` ordinal allocator (Phase-B §4.1).
//!
//! `concept_identity.json` holds the high-water mark for minting semantic
//! concept ids. Stable-id carry-over ([`super::semantic::build_semantic_concepts_stable`])
//! reuses a prior id when a new cluster matches an old centroid; a genuinely-new
//! cluster mints `sem:<next>` and bumps this counter. Persisting the counter
//! across recomputes guarantees a **dropped** theme's number is never recycled
//! onto a different theme — so a relation that briefly dangled can never silently
//! re-point at an unrelated concept that inherited its id.
//!
//! Encrypted at rest (OI-14) + atomic-write, matching [`super::store`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::registry::persistence::workspace_dir;

/// The per-workspace concept-id allocator state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConceptIdentity {
    /// The next `sem:` ordinal to mint — monotonically non-decreasing across
    /// recomputes (never reused).
    #[serde(default)]
    pub next_sem_ordinal: u32,
}

/// `<data_dir>/workspaces/<workspace_id>/concept_identity.json`.
fn identity_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("concept_identity.json")
}

/// Load the allocator. Fail-soft: default (`next_sem_ordinal: 0`) on a
/// missing/torn file. Decrypts at rest.
pub fn load(workspace_id: &str) -> ConceptIdentity {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&identity_path(workspace_id))
    else {
        return ConceptIdentity::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest + atomically replace `concept_identity.json`.
pub fn save(workspace_id: &str, identity: &ConceptIdentity) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(identity).unwrap();
    wylde_shared::encryption::write_at_rest(&identity_path(workspace_id), body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn round_trips_and_defaults() {
        let _env = TestEnv::new();
        let ws = "ws-ident-0000";
        assert_eq!(load(ws), ConceptIdentity::default());
        assert_eq!(load(ws).next_sem_ordinal, 0);
        save(ws, &ConceptIdentity { next_sem_ordinal: 42 }).unwrap();
        assert_eq!(load(ws).next_sem_ordinal, 42);
    }
}
