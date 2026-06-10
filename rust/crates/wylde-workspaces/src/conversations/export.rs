//! Conversation export (TBS Slice J) — wrap one workspace conversation in
//! the portable envelope (`wylde_shared::conversation_export`).
//!
//! The verb returns the envelope over the wire; persisting it to a file is
//! the caller's concern (the GUI offers a save dialog), which keeps this
//! service filesystem-agnostic about destinations. Export reads through the
//! store — so an at-rest-encrypted document exports as plaintext JSON
//! (portable across machines; DPAPI ciphertext is not).

use serde_json::Value;
use wylde_shared::conversation_export as envelope;

use super::store::{self, ReadError};

/// Export `conv_id` from `workspace_id` as a portable envelope.
pub fn export(workspace_id: &str, conv_id: &str) -> Result<Value, ReadError> {
    let doc = store::read_conversation(workspace_id, conv_id)?;
    Ok(envelope::wrap("workspace", doc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use serde_json::json;

    #[test]
    fn export_wraps_the_stored_document_verbatim() {
        let _env = TestEnv::new();
        let ws = "ws-export-000000";
        let doc = json!({
            "id": "c1", "title": "T", "updated_at": 7, "workspace_id": ws,
            "messages": [{"role": "user", "content": "hi"}],
            "unknown_sibling": [1, 2, 3],
        });
        store::save_conversation(ws, doc.as_object().unwrap()).unwrap();

        let env_v = export(ws, "c1").expect("exports");
        assert_eq!(env_v["format"], envelope::FORMAT);
        assert_eq!(env_v["scope"], "workspace");
        // Verbatim — including the field no struct knows about.
        assert_eq!(
            serde_json::to_string(&env_v["conversation"]).unwrap(),
            serde_json::to_string(&doc).unwrap()
        );
        // Deterministic: exporting again yields identical bytes.
        let again = export(ws, "c1").unwrap();
        assert_eq!(
            serde_json::to_string(&env_v).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
    }

    #[test]
    fn export_missing_and_invalid_map_to_read_errors() {
        let _env = TestEnv::new();
        assert!(matches!(
            export("ws-export-nf-000000", "ghost"),
            Err(ReadError::NotFound(_))
        ));
        assert!(matches!(
            export("ws-export-nf-000000", "bad/id"),
            Err(ReadError::InvalidId(_))
        ));
    }
}
