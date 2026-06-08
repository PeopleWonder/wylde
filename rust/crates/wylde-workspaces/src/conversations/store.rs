//! Workspace-scoped conversation store.
//!
//! **Conceptual path:** `Core/Workspaces/Conversations/`.
//!
//! Slice 0c split the conversation tier: **standalone** conversations
//! (`workspace_id == None`) stay in the harness flat store at
//! `<data_dir>/conversations/`; **workspace** conversations
//! (`workspace_id != None`) live here, one file per conversation under each
//! workspace's bundle:
//!
//! ```text
//! <data_dir>/workspaces/<workspace_id>/conversations/<conv_id>.json
//! ```
//!
//! Because the bundle dir is removed wholesale by
//! [`crate::registry::delete`], deleting a workspace deletes its
//! conversations (plan §3 — "workspace deletion deletes its conversations").
//!
//! ## Why `Value`/`Map` rather than a typed `Conversation` struct
//!
//! The on-disk document is the same free-form shape the harness flat store
//! uses (`id, title, created_at, updated_at, messages, working_memory,
//! model, workspace_id, …`). Operating on the raw map keeps the relocated
//! files **byte-identical** and never drops a sibling field a typed struct
//! wouldn't know about — the same "proven layout > canonical churn" stance
//! Slice 0b took. The Build Order's typed `Conversation` lands when a later
//! slice (E/J) needs it.
//!
//! Atomic-write discipline matches the rest of the store: reads tolerate a
//! torn/missing file, writes go to `<path>.tmp` then rename.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::common::ensure_dir;
use crate::registry::persistence::workspace_dir;

/// Mirrors the harness `_MAX_ID_LEN`.
const MAX_ID_LEN: usize = 128;

/// Caller-supplied id wasn't safe to use as a filename. The action layer
/// maps this to a `bad_request` reply.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidConversationId(pub String);

/// Requested conversation file does not exist / isn't readable. The action
/// layer maps this to a `not_found` reply.
#[derive(Debug, PartialEq, Eq)]
pub struct ConversationNotFound(pub String);

/// Error from [`read_conversation`].
#[derive(Debug)]
pub enum ReadError {
    InvalidId(InvalidConversationId),
    NotFound(ConversationNotFound),
}

/// Validate a caller-supplied conversation id: non-empty, `<= 128` chars,
/// charset `[A-Za-z0-9_-]`. Identical rules to the harness store so a
/// migrated id always validates here.
pub fn validate_id(conv_id: &str) -> Result<(), InvalidConversationId> {
    if conv_id.is_empty() {
        return Err(InvalidConversationId(
            "conversation id must be a non-empty string".to_owned(),
        ));
    }
    if conv_id.len() > MAX_ID_LEN {
        return Err(InvalidConversationId(format!(
            "conversation id is too long (>{MAX_ID_LEN} chars)"
        )));
    }
    if !conv_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(InvalidConversationId(
            "conversation id may only contain [A-Za-z0-9_-]".to_owned(),
        ));
    }
    Ok(())
}

/// `<data_dir>/workspaces/<workspace_id>/conversations/`.
pub fn conversations_dir(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("conversations")
}

/// `<data_dir>/workspaces/<workspace_id>/conversations/<conv_id>.json`.
/// Caller has already validated `conv_id`.
pub fn path_for(workspace_id: &str, conv_id: &str) -> PathBuf {
    conversations_dir(workspace_id).join(format!("{conv_id}.json"))
}

/// Read + parse one conversation document. `Err(NotFound)` when the file is
/// absent / unreadable / not an object.
fn read_doc(workspace_id: &str, conv_id: &str) -> Result<Map<String, Value>, ConversationNotFound> {
    let path = path_for(workspace_id, conv_id);
    if !path.exists() {
        return Err(ConversationNotFound(format!(
            "conversation '{conv_id}' not found in workspace '{workspace_id}'"
        )));
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        ConversationNotFound(format!("conversation '{conv_id}' is unreadable: {e}"))
    })?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ConversationNotFound(format!(
            "conversation '{conv_id}' is malformed"
        ))),
        Err(e) => Err(ConversationNotFound(format!(
            "conversation '{conv_id}' is unreadable: {e}"
        ))),
    }
}

/// The full conversation document for `conv_id` within `workspace_id`.
pub fn read_conversation(workspace_id: &str, conv_id: &str) -> Result<Value, ReadError> {
    validate_id(conv_id).map_err(ReadError::InvalidId)?;
    read_doc(workspace_id, conv_id)
        .map(Value::Object)
        .map_err(ReadError::NotFound)
}

/// Atomically write a conversation document (temp + rename), preserving
/// every field. Used by the migration and the tests. The doc's own `id`
/// (validated) decides the filename; an absent/empty/invalid id is an error.
pub fn save_conversation(workspace_id: &str, doc: &Map<String, Value>) -> std::io::Result<()> {
    let conv_id = doc
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "conversation document missing a usable `id`",
            )
        })?;
    validate_id(conv_id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.0))?;
    let dir = conversations_dir(workspace_id);
    ensure_dir(&dir)?;
    let path = path_for(workspace_id, conv_id);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(&Value::Object(doc.clone()))
        .unwrap_or_else(|_| "{}".to_owned());
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Remove a conversation file. Returns `true` iff a file was deleted.
/// Validates the id first so a caller can't aim the unlink outside the dir.
pub fn delete_conversation(
    workspace_id: &str,
    conv_id: &str,
) -> Result<bool, InvalidConversationId> {
    validate_id(conv_id)?;
    let path = path_for(workspace_id, conv_id);
    if !path.exists() {
        return Ok(false);
    }
    Ok(std::fs::remove_file(&path).is_ok())
}

/// Lightweight per-conversation metadata for one workspace, newest-first by
/// `updated_at`. Mirrors the harness flat `list_conversations` field set so a
/// repointed GUI sees an identical shape.
pub fn list_conversations(workspace_id: &str) -> Vec<Value> {
    let dir = conversations_dir(workspace_id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut metas: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let cid = match doc.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => continue,
        };
        let created_at = doc.get("created_at").and_then(Value::as_i64).unwrap_or(0);
        let updated_at = doc
            .get("updated_at")
            .and_then(Value::as_i64)
            .unwrap_or(created_at);
        let msg_count = doc
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let wm_count = doc
            .get("working_memory")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let title = doc
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled");
        let model = doc.get("model").and_then(Value::as_str).unwrap_or("");
        let ws = doc.get("workspace_id").and_then(Value::as_str).unwrap_or("");
        metas.push(serde_json::json!({
            "id": cid,
            "title": title,
            "created_at": created_at,
            "updated_at": updated_at,
            "message_count": msg_count,
            "working_memory_count": wm_count,
            "model": model,
            "workspace_id": ws,
        }));
    }
    metas.sort_by(|a, b| {
        let av = a.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        let bv = b.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        bv.cmp(&av)
    });
    metas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use serde_json::json;

    fn seed(ws: &str, cid: &str, doc: Value) {
        let Value::Object(map) = doc else { panic!("doc must be object") };
        save_conversation(ws, &map).unwrap();
        // Sanity: filename matches the doc id.
        assert!(path_for(ws, cid).exists());
    }

    #[test]
    fn validate_rejects_bad_ids() {
        assert!(validate_id("").is_err());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id(&"x".repeat(129)).is_err());
        assert!(validate_id("ok-default_1").is_ok());
    }

    #[test]
    fn save_then_read_round_trips() {
        let _env = TestEnv::new();
        let ws = "ws-conv-000000";
        seed(
            ws,
            "c1",
            json!({"id": "c1", "title": "T", "workspace_id": ws,
                   "messages": [{"role": "user", "content": "hi"}], "working_memory": []}),
        );
        let doc = read_conversation(ws, "c1").expect("found");
        assert_eq!(doc["title"], "T");
        assert_eq!(doc["workspace_id"], ws);
        assert_eq!(doc["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn read_not_found_and_invalid_are_distinct() {
        let _env = TestEnv::new();
        let ws = "ws-conv-nf-000000";
        match read_conversation(ws, "ghost") {
            Err(ReadError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        match read_conversation(ws, "bad/id") {
            Err(ReadError::InvalidId(_)) => {}
            other => panic!("expected InvalidId, got {other:?}"),
        }
    }

    #[test]
    fn list_is_scoped_to_one_workspace_newest_first() {
        let _env = TestEnv::new();
        let ws_a = "ws-a-000000";
        let ws_b = "ws-b-000000";
        seed(ws_a, "old", json!({"id": "old", "updated_at": 100, "messages": [], "workspace_id": ws_a}));
        seed(ws_a, "new", json!({"id": "new", "updated_at": 300, "messages": [{"role":"user","content":"x"}], "workspace_id": ws_a}));
        seed(ws_b, "other", json!({"id": "other", "updated_at": 200, "messages": [], "workspace_id": ws_b}));

        let a = list_conversations(ws_a);
        assert_eq!(a.len(), 2, "only ws_a's conversations");
        assert_eq!(a[0]["id"], "new", "newest first");
        assert_eq!(a[1]["id"], "old");

        let b = list_conversations(ws_b);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0]["id"], "other");
    }

    #[test]
    fn list_empty_when_dir_absent() {
        let _env = TestEnv::new();
        assert!(list_conversations("ws-none-000000").is_empty());
    }

    #[test]
    fn delete_removes_and_reports_truthfully() {
        let _env = TestEnv::new();
        let ws = "ws-conv-del-000000";
        seed(ws, "d1", json!({"id": "d1", "messages": [], "workspace_id": ws}));
        assert!(delete_conversation(ws, "d1").unwrap());
        assert!(read_conversation(ws, "d1").is_err());
        assert!(!delete_conversation(ws, "d1").unwrap());
    }

    #[test]
    fn delete_rejects_invalid_id() {
        let _env = TestEnv::new();
        assert!(delete_conversation("ws", "../escape").is_err());
    }

    #[test]
    fn save_rejects_doc_without_id() {
        let _env = TestEnv::new();
        let map = json!({"title": "no id"}).as_object().unwrap().clone();
        assert!(save_conversation("ws", &map).is_err());
    }
}
