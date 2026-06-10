//! Conversation import (TBS Slice J) — land a portable envelope in a
//! workspace's conversation store.
//!
//! The imported document is written **verbatim** except for one field:
//! `workspace_id` is set to the destination workspace (the store's listing
//! reads it back, and a stale source id would lie). A same-workspace
//! round-trip therefore stays byte-exact; a cross-workspace import differs
//! from its source only in that one field — both pinned by tests.
//!
//! Collisions are explicit: an existing conversation with the same id is
//! `already_exists` unless the caller passes `overwrite: true` (the OI-18
//! user-decides spirit — nothing is silently replaced).

use serde_json::Value;
use wylde_shared::conversation_export as envelope;

use super::store;

/// Why an import failed. The api layer maps these onto reply codes.
#[derive(Debug)]
pub enum ImportError {
    /// Bad envelope (wrong format / version / no conversation / no id).
    Format(envelope::UnwrapError),
    /// The embedded id isn't a safe filename.
    InvalidId(String),
    /// A conversation with this id already exists and `overwrite` was false.
    AlreadyExists(String),
    /// The store write failed.
    Io(String),
}

/// Import `envelope_value` into `workspace_id`. Returns the conversation id
/// on success.
pub fn import(
    workspace_id: &str,
    envelope_value: &Value,
    overwrite: bool,
) -> Result<String, ImportError> {
    let (id, mut doc) = envelope::unwrap(envelope_value).map_err(ImportError::Format)?;
    store::validate_id(&id).map_err(|e| ImportError::InvalidId(e.0))?;
    if !overwrite && store::path_for(workspace_id, &id).exists() {
        return Err(ImportError::AlreadyExists(id));
    }
    // The one rewritten field — the destination owns the conversation now.
    doc.insert(
        "workspace_id".to_owned(),
        Value::String(workspace_id.to_owned()),
    );
    store::save_conversation(workspace_id, &doc).map_err(|e| ImportError::Io(e.to_string()))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::export::export;
    use crate::test_support::TestEnv;
    use serde_json::json;

    fn seed(ws: &str, doc: Value) {
        store::save_conversation(ws, doc.as_object().unwrap()).unwrap();
    }

    #[test]
    fn same_workspace_round_trip_is_byte_exact() {
        // The Phase 5 acceptance: export → import → export, identical bytes.
        let _env = TestEnv::new();
        let ws = "ws-rt-000000";
        seed(
            ws,
            json!({
                "id": "c1", "title": "Round trip", "updated_at": 42, "workspace_id": ws,
                "messages": [{"role": "user", "content": "hello"},
                             {"role": "assistant", "content": "hi"}],
                "working_memory": [{"note": "kept"}],
                "embedding": [0.25, 0.5],
                "field_from_the_future": {"survives": true},
            }),
        );

        let first = export(ws, "c1").unwrap();
        store::delete_conversation(ws, "c1").unwrap();
        let id = import(ws, &first, false).expect("imports");
        assert_eq!(id, "c1");
        let second = export(ws, "c1").unwrap();
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "export → import → export must be byte-exact"
        );
    }

    #[test]
    fn cross_workspace_import_differs_only_in_workspace_id() {
        let _env = TestEnv::new();
        let (src, dst) = ("ws-src-000000", "ws-dst-000000");
        seed(
            src,
            json!({"id": "c1", "title": "Move me", "workspace_id": src,
                   "messages": [{"role": "user", "content": "x"}]}),
        );
        let exported = export(src, "c1").unwrap();
        import(dst, &exported, false).expect("imports into dst");

        let mut moved = store::read_conversation(dst, "c1").unwrap();
        assert_eq!(moved["workspace_id"], dst, "destination owns it");
        // Normalise the one expected difference; the rest is identical.
        moved["workspace_id"] = json!(src);
        assert_eq!(
            serde_json::to_string(&moved).unwrap(),
            serde_json::to_string(&exported["conversation"]).unwrap()
        );
        // The source copy is untouched.
        assert!(store::read_conversation(src, "c1").is_ok());
    }

    #[test]
    fn collision_requires_explicit_overwrite() {
        let _env = TestEnv::new();
        let ws = "ws-coll-000000";
        seed(
            ws,
            json!({"id": "c1", "title": "Original", "workspace_id": ws, "messages": []}),
        );
        let envelope_v = envelope::wrap(
            "workspace",
            json!({"id": "c1", "title": "Incoming", "messages": []}),
        );

        match import(ws, &envelope_v, false) {
            Err(ImportError::AlreadyExists(id)) => assert_eq!(id, "c1"),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        // Untouched after the refusal.
        assert_eq!(
            store::read_conversation(ws, "c1").unwrap()["title"],
            "Original"
        );

        // Explicit overwrite replaces it.
        import(ws, &envelope_v, true).expect("overwrites");
        assert_eq!(
            store::read_conversation(ws, "c1").unwrap()["title"],
            "Incoming"
        );
    }

    #[test]
    fn bad_envelopes_and_ids_are_rejected() {
        let _env = TestEnv::new();
        let ws = "ws-bad-000000";
        assert!(matches!(
            import(ws, &json!({"format": "junk"}), false),
            Err(ImportError::Format(_))
        ));
        let traversal = envelope::wrap("workspace", json!({"id": "../escape", "messages": []}));
        assert!(matches!(
            import(ws, &traversal, false),
            Err(ImportError::InvalidId(_))
        ));
    }
}
