//! Reflection — propose candidate workspace notes (auto-extract facts).
//!
//! The reflection *logic* lives with the notes tier (Slice 0c moved it into
//! the `wylde-workspaces` service), but the *trigger* — when reflection runs
//! over a finished turn — is harness-driven and lands in a later slice (Phase
//! 2, Slice G). (The GUI's cross-panel `conversation_bus` is a separate,
//! GUI-only nav mechanism, not this trigger.) Here we expose the proposal
//! primitive the trigger will call: take candidate text, embed it, and return
//! a non-persisted [`WorkspaceMemoryEntry`]. **Proposals are never written**
//! — per the plan (§4.5) reflection is always user-accept, so the GUI shows
//! the candidate and only `workspaces.notes.add` actually persists it.

use super::entry::{self, WorkspaceMemoryEntry};
use super::query;

/// Build a proposed (not-yet-persisted) note from candidate `text` for
/// `workspace_id`. The returned entry carries a freshly minted id and the
/// embedded vector (empty if the embedder was unreachable), ready to hand to
/// `workspaces.notes.add` on user-accept. Returns `None` for blank text.
pub async fn propose(_workspace_id: &str, text: &str) -> Option<WorkspaceMemoryEntry> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut candidate = WorkspaceMemoryEntry::new(entry::new_note_id(), trimmed);
    candidate.embedding = query::embed_text_bounded(trimmed, query::EMBED_WRITE_BUDGET).await;
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn propose_blank_is_none() {
        assert!(propose("ws", "   ").await.is_none());
        assert!(propose("ws", "").await.is_none());
    }

    #[tokio::test]
    async fn propose_mints_candidate_without_persisting() {
        use crate::test_support::TestEnv;
        let _env = TestEnv::new();
        let ws = "ws-propose-000000";
        // Embedder is unreachable in unit tests → empty embedding, but the
        // candidate is still well-formed and is NOT written to disk.
        let candidate = propose(ws, "uses tokio").await.expect("candidate");
        assert_eq!(candidate.text, "uses tokio");
        assert!(candidate.id.starts_with("note-"));
        assert!(entry::load(ws).is_empty(), "propose must not persist");
    }
}
