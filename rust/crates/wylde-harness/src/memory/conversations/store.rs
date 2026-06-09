//! Conversation-document store — Rust port of the conversation-listing
//! half of `Core/harness/memory/conversation.py`.
//!
//! ## What this module ports
//!
//! The four `conversations.*` pipe verbs the gpui Chat panel's switcher
//! consumes:
//!
//! * `conversations.new`    → mint a fresh, sortable, filename-safe id.
//! * `conversations.list`   → lightweight metadata for every saved chat.
//! * `conversations.get`    → the full conversation document by id.
//! * `conversations.delete` → remove a conversation file.
//!
//! plus a small **active-conversation persistence** pair the switcher
//! uses to remember the user's selection across app restarts:
//!
//! * `conversations.get_active` / `conversations.set_active` →
//!   `<data_dir>/active_conversation.json` (`{"id": "..."}`), mirroring
//!   `model_registry::model_state`'s `active_model.json` exactly.
//!
//! ## Relationship to `short_term`
//!
//! [`super::super::short_term`] already ports the *working-memory* half
//! of the same `conversation.py` file (the `working_memory` array inside
//! each `<conversations_dir>/<id>.json` document). This module ports the
//! *document-lifecycle* half — minting / listing / reading / deleting the
//! same files. The two share the on-disk schema and the same id-validation
//! rules; neither writes a field the other owns, so a `conversations.get`
//! and a `memory.short_term.append` interleave safely on one file. The
//! Python `conversation.py` module stays load-bearing for
//! `memory.reflect`; this port only moves the four pipe verbs off Python.
//!
//! ## Why no in-process cache
//!
//! Like [`super::super::common`], every path lookup re-reads the env so
//! tests can swap `WYLDE_DATA_DIR` per-test. List/read are plain file IO;
//! they don't take a process-wide lock because the only mutation that can
//! race them — `short_term`'s merge-save — already serialises through its
//! own atomic temp + rename, so a concurrent reader sees either the old
//! or the new file, never a torn one.

use std::path::PathBuf;

use rand::RngCore;
use serde_json::{json, Map, Value};

use crate::memory::common::{conversations_dir, data_dir, ensure_dir};

/// Mirrors Python's `_MAX_ID_LEN`.
const MAX_ID_LEN: usize = 128;

/// Caller-supplied id wasn't safe to use as a filename. Mirrors Python's
/// `InvalidConversationId` (a `ValueError`); the action layer maps this to
/// a `bad_request` reply.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidConversationId(pub String);

/// Requested conversation file does not exist / isn't readable. Mirrors
/// Python's `ConversationNotFound`; the action layer maps this to a
/// `not_found` reply.
#[derive(Debug, PartialEq, Eq)]
pub struct ConversationNotFound(pub String);

/// Validate a caller-supplied conversation id. Matches Python's
/// `_validate_id`: non-empty, `<= 128` chars, charset `[A-Za-z0-9_-]`.
fn validate_id(conv_id: &str) -> Result<(), InvalidConversationId> {
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

/// `<conversations_dir>/<id>.json`. Caller has already validated `id`.
fn path_for(conv_id: &str) -> PathBuf {
    conversations_dir().join(format!("{conv_id}.json"))
}

/// `<data_dir>/active_conversation.json`. The persisted "which chat was
/// the user last looking at" pointer. Sits beside the `conversations/`
/// folder (same `data_dir()` root as the documents themselves), unlike
/// `model_state`'s `active_model.json` which keys off the legacy
/// `DATA_DIR` env — conversations have always resolved through
/// `WYLDE_DATA_DIR`, so we stay on that root for consistency.
fn active_path() -> PathBuf {
    data_dir().join("active_conversation.json")
}

/// Mint a sortable, filename-safe id with a short random suffix. Mirrors
/// Python's `new_conversation_id`:
/// `datetime.now(utc).strftime("%Y-%m-%dT%H-%M-%S-%fZ")` + `-` +
/// `secrets.token_hex(3)`. `%f` carries no dot, so Python's defensive
/// `.replace(".", "-")` is a no-op we omit.
pub fn new_conversation_id() -> String {
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%6fZ");
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    let suffix: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("{stamp}-{suffix}")
}

/// Read + parse one conversation document. `Ok(map)` on success,
/// `Err(NotFound)` when the file is absent / unreadable / not an object —
/// matching Python's `read_conversation` raise-on-missing semantics.
fn read_doc(conv_id: &str) -> Result<Map<String, Value>, ConversationNotFound> {
    let path = path_for(conv_id);
    if !path.exists() {
        return Err(ConversationNotFound(format!(
            "conversation '{conv_id}' not found"
        )));
    }
    let raw = wylde_shared::encryption::read_to_string_at_rest(&path).map_err(|e| {
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

/// The full conversation document for `conv_id`. Mirrors Python's
/// `read_conversation`: validates the id, then raises not-found for a
/// missing / malformed file.
pub fn read_conversation(conv_id: &str) -> Result<Value, ReadError> {
    validate_id(conv_id).map_err(ReadError::InvalidId)?;
    read_doc(conv_id).map(Value::Object).map_err(ReadError::NotFound)
}

/// Epoch seconds as `i64`, matching the `created_at` / `updated_at`
/// convention used across the conversation document.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomically write a conversation document (temp + rename), preserving
/// every field the caller hands in. Used by [`set_workspace`].
fn write_doc(conv_id: &str, doc: &Map<String, Value>) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(&Value::Object(doc.clone()))
        .unwrap_or_else(|_| "{}".to_owned());
    // Encrypt-at-rest (OI-14) + atomic temp-write + rename + owner-only.
    wylde_shared::encryption::write_at_rest(&path_for(conv_id), body.as_bytes())
}

/// Re-assign a conversation's `workspace_id` (Q4 — mutable binding).
///
/// Upserts: if the conversation document doesn't exist yet (a freshly
/// minted id the user is about to chat in), a minimal document is
/// created so the binding persists. An empty / `None` `workspace_id`
/// clears the binding. Returns the updated document. The write is
/// best-effort (swallowed like the active-pointer write); the GUI
/// re-reads afterward.
pub fn set_workspace(
    conv_id: &str,
    workspace_id: Option<&str>,
) -> Result<Value, InvalidConversationId> {
    validate_id(conv_id)?;
    let now = now_secs();
    let mut doc = read_doc(conv_id).unwrap_or_else(|_| {
        let mut m = Map::new();
        m.insert("id".into(), Value::String(conv_id.to_owned()));
        m.insert("title".into(), Value::String("Untitled".to_owned()));
        m.insert("created_at".into(), json!(now));
        m.insert("messages".into(), Value::Array(Vec::new()));
        m.insert("working_memory".into(), Value::Array(Vec::new()));
        m
    });
    let cleaned = workspace_id.map(str::trim).filter(|s| !s.is_empty());
    doc.insert(
        "workspace_id".into(),
        Value::String(cleaned.unwrap_or("").to_owned()),
    );
    doc.insert("updated_at".into(), json!(now));
    let _ = write_doc(conv_id, &doc); // best-effort, like set_active_conversation
    Ok(Value::Object(doc))
}

/// Every standalone conversation document in full (not just metadata),
/// newest-first by `updated_at`. The Slice E search backend needs the whole
/// document — `auto_summary`, `topic_tags`, `embedding` — which
/// [`list_conversations`] projects away. Skips the active-pointer file and
/// any malformed / id-less object, exactly like the listing path.
pub fn read_all_conversations() -> Vec<Value> {
    let dir = conversations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut docs: Vec<Map<String, Value>> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&path) else {
            continue;
        };
        let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        // Same id guard the listing path uses — drops the active pointer.
        match doc.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => {}
            _ => continue,
        }
        docs.push(doc);
    }
    docs.sort_by(|a, b| {
        let av = a.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        let bv = b.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        bv.cmp(&av)
    });
    docs.into_iter().map(Value::Object).collect()
}

/// Merge `fields` into conversation `conv_id`'s document and persist it
/// atomically, preserving every sibling field. The Slice E summary pipeline
/// uses this to attach `auto_summary` / `topic_tags` / `embedding` /
/// `summary_msg_count` without disturbing `messages`, `working_memory`, or
/// the activity timestamps — a summary refresh is **not** user activity, so
/// `updated_at` is deliberately left untouched (re-summarising must not
/// reorder the conversation list). Errors if the conversation is absent
/// (this is a refresh, not an upsert) or the id is invalid.
pub fn merge_fields(conv_id: &str, fields: Map<String, Value>) -> Result<Value, ReadError> {
    validate_id(conv_id).map_err(ReadError::InvalidId)?;
    let mut doc = read_doc(conv_id).map_err(ReadError::NotFound)?;
    for (k, v) in fields {
        doc.insert(k, v);
    }
    let _ = write_doc(conv_id, &doc); // best-effort, like set_workspace
    Ok(Value::Object(doc))
}

/// Remove a conversation file. Returns `true` iff a file was deleted.
/// Mirrors Python's `delete_conversation`. Validates the id first so a
/// caller can't aim the unlink outside the conversations dir.
pub fn delete_conversation(conv_id: &str) -> Result<bool, InvalidConversationId> {
    validate_id(conv_id)?;
    let path = path_for(conv_id);
    if !path.exists() {
        return Ok(false);
    }
    // A delete failure (perms, race with another process) maps to "not
    // deleted" rather than an error — matches Python's best-effort unlink
    // shape closely enough for the GUI, which re-lists afterward anyway.
    Ok(std::fs::remove_file(&path).is_ok())
}

/// Lightweight per-conversation metadata, newest-first. Mirrors Python's
/// `list_conversations` field-for-field, plus one additive field the
/// Slice B switcher renders: `working_memory_count` (the size of the
/// working-memory badge). Adding a field is safe — the Python side became
/// a forwarder, and existing readers ignore unknown keys.
pub fn list_conversations() -> Vec<Value> {
    let dir = conversations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // Missing dir → no conversations yet. Mirrors Python's
        // `if not CONVERSATIONS_DIR.exists(): return []`.
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
        let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&path) else {
            continue;
        };
        let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        // Skip the active-conversation pointer or any other stray object
        // that lacks a usable `id` — mirrors Python's `id` guard.
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
        // Additive (Q4): the switcher restores the workspace selection
        // from this. Existing readers ignore the unknown key.
        let workspace_id = doc.get("workspace_id").and_then(Value::as_str).unwrap_or("");
        metas.push(json!({
            "id": cid,
            "title": title,
            "created_at": created_at,
            "updated_at": updated_at,
            "message_count": msg_count,
            "working_memory_count": wm_count,
            "model": model,
            "workspace_id": workspace_id,
        }));
    }
    // Newest-first by updated_at, matching Python's reverse sort.
    metas.sort_by(|a, b| {
        let av = a.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        let bv = b.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        bv.cmp(&av)
    });
    metas
}

/// The persisted active-conversation id, or `None` if none chosen yet /
/// the file is missing or malformed. Any read error folds to `None`
/// (matches `model_state::read_disk`).
pub fn get_active_conversation() -> Option<String> {
    let text = std::fs::read_to_string(active_path()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let id = value.get("id").and_then(Value::as_str)?;
    let trimmed = id.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Persist `id` as the active conversation (empty / `None` clears it).
/// Returns the persisted value. Write errors are swallowed (best-effort,
/// like `model_state::write_disk`); a failed write just means the
/// selection isn't remembered, never a hard failure.
pub fn set_active_conversation(id: Option<&str>) -> Option<String> {
    let cleaned = id.map(str::trim).filter(|s| !s.is_empty());
    let path = active_path();
    if let Some(parent) = path.parent() {
        let _ = ensure_dir(parent); // wylde-check: discard-result-ok
    }
    let body = json!({ "id": cleaned.unwrap_or("") });
    let serialised = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_owned());
    let _ = std::fs::write(&path, serialised); // wylde-check: discard-result-ok
    cleaned.map(str::to_owned)
}

/// Error from [`read_conversation`].
#[derive(Debug)]
pub enum ReadError {
    InvalidId(InvalidConversationId),
    NotFound(ConversationNotFound),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::conversations::test_support::TestEnv;

    fn seed_conversation(cid: &str, doc: Value) {
        let path = path_for(cid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }

    #[test]
    fn new_id_is_sortable_and_filename_safe() {
        let a = new_conversation_id();
        // Charset matches the validator (so a freshly minted id is a
        // valid filename) and carries the timestamp + 6-hex suffix shape.
        assert!(validate_id(&a).is_ok(), "minted id should validate: {a}");
        assert!(a.starts_with("20"), "starts with a year: {a}");
        assert!(a.len() >= 7 + 6, "has a stamp + suffix: {a}");
    }

    #[test]
    fn new_ids_are_unique() {
        let a = new_conversation_id();
        let b = new_conversation_id();
        assert_ne!(a, b, "random suffix should disambiguate same-microsecond ids");
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
    fn list_empty_when_dir_absent() {
        let _env = TestEnv::new();
        assert!(list_conversations().is_empty());
    }

    #[test]
    fn list_returns_metadata_newest_first() {
        let _env = TestEnv::new();
        seed_conversation(
            "old",
            json!({
                "id": "old", "title": "First chat", "created_at": 100,
                "updated_at": 100, "messages": [{"role": "user", "content": "hi"}],
                "working_memory": [], "model": "qwen2.5",
            }),
        );
        seed_conversation(
            "new",
            json!({
                "id": "new", "title": "Second chat", "created_at": 200,
                "updated_at": 300,
                "messages": [
                    {"role": "user", "content": "a"},
                    {"role": "assistant", "content": "b"},
                ],
                "working_memory": [{"kind": "tool", "data": {}}],
            }),
        );
        let metas = list_conversations();
        assert_eq!(metas.len(), 2);
        // Newest (updated_at=300) first.
        assert_eq!(metas[0]["id"], "new");
        assert_eq!(metas[0]["message_count"], 2);
        assert_eq!(metas[0]["working_memory_count"], 1);
        assert_eq!(metas[0]["model"], "");
        assert_eq!(metas[1]["id"], "old");
        assert_eq!(metas[1]["title"], "First chat");
        assert_eq!(metas[1]["model"], "qwen2.5");
    }

    #[test]
    fn list_skips_unreadable_and_non_object_and_idless() {
        let _env = TestEnv::new();
        seed_conversation("good", json!({"id": "good", "updated_at": 5, "messages": []}));
        // Non-object JSON.
        let dir = conversations_dir();
        std::fs::write(dir.join("array.json"), "[1,2,3]").unwrap();
        // Object without an id.
        std::fs::write(dir.join("noid.json"), r#"{"title":"x"}"#).unwrap();
        // Garbage.
        std::fs::write(dir.join("torn.json"), "{not json").unwrap();
        // Non-json file.
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();
        let metas = list_conversations();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0]["id"], "good");
    }

    #[test]
    fn read_returns_full_document() {
        let _env = TestEnv::new();
        seed_conversation(
            "doc1",
            json!({
                "id": "doc1", "title": "T", "created_at": 1, "updated_at": 2,
                "messages": [{"role": "user", "content": "hi"}],
                "working_memory": [{"kind": "decision", "data": "x"}],
            }),
        );
        let doc = read_conversation("doc1").expect("found");
        assert_eq!(doc["title"], "T");
        assert_eq!(doc["messages"].as_array().unwrap().len(), 1);
        assert_eq!(doc["working_memory"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn read_not_found_for_missing() {
        let _env = TestEnv::new();
        match read_conversation("ghost") {
            Err(ReadError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn read_invalid_id_is_distinct_from_not_found() {
        let _env = TestEnv::new();
        match read_conversation("bad/id") {
            Err(ReadError::InvalidId(_)) => {}
            other => panic!("expected InvalidId, got {other:?}"),
        }
    }

    #[test]
    fn delete_removes_file_and_reports_truthfully() {
        let _env = TestEnv::new();
        seed_conversation("del1", json!({"id": "del1", "messages": []}));
        assert!(delete_conversation("del1").unwrap(), "first delete removes it");
        assert!(read_conversation("del1").is_err(), "gone after delete");
        assert!(
            !delete_conversation("del1").unwrap(),
            "second delete is a no-op false"
        );
    }

    #[test]
    fn delete_rejects_invalid_id() {
        let _env = TestEnv::new();
        assert!(delete_conversation("../escape").is_err());
    }

    #[test]
    fn active_conversation_round_trips_and_clears() {
        let _env = TestEnv::new();
        assert_eq!(get_active_conversation(), None, "unset on fresh install");
        assert_eq!(
            set_active_conversation(Some("conv-abc")),
            Some("conv-abc".to_owned())
        );
        assert_eq!(get_active_conversation(), Some("conv-abc".to_owned()));
        // Empty / whitespace clears.
        assert_eq!(set_active_conversation(Some("   ")), None);
        assert_eq!(get_active_conversation(), None);
        set_active_conversation(Some("conv-def"));
        assert_eq!(set_active_conversation(None), None);
        assert_eq!(get_active_conversation(), None);
    }

    #[test]
    fn set_workspace_is_mutable_upsert_and_clear() {
        let _env = TestEnv::new();
        // Upsert onto a not-yet-saved conversation id.
        let doc = set_workspace("conv-ws-1", Some("proj-abc123")).unwrap();
        assert_eq!(doc["workspace_id"], "proj-abc123");
        assert_eq!(doc["id"], "conv-ws-1");
        // Re-assign (mutable).
        let doc2 = set_workspace("conv-ws-1", Some("other-def456")).unwrap();
        assert_eq!(doc2["workspace_id"], "other-def456");
        // Persisted + visible to read + list.
        let read = read_conversation("conv-ws-1").unwrap();
        assert_eq!(read["workspace_id"], "other-def456");
        let metas = list_conversations();
        assert_eq!(metas[0]["workspace_id"], "other-def456");
        // Clear.
        let cleared = set_workspace("conv-ws-1", None).unwrap();
        assert_eq!(cleared["workspace_id"], "");
    }

    #[test]
    fn set_workspace_rejects_invalid_id() {
        let _env = TestEnv::new();
        assert!(set_workspace("../escape", Some("x")).is_err());
    }

    #[test]
    fn set_workspace_preserves_existing_fields() {
        let _env = TestEnv::new();
        seed_conversation(
            "keepme",
            json!({
                "id": "keepme", "title": "Keep", "created_at": 5, "updated_at": 5,
                "messages": [{"role": "user", "content": "hi"}],
                "working_memory": [{"kind": "x"}], "model": "qwen2.5",
            }),
        );
        set_workspace("keepme", Some("ws-1")).unwrap();
        let doc = read_conversation("keepme").unwrap();
        assert_eq!(doc["title"], "Keep");
        assert_eq!(doc["model"], "qwen2.5");
        assert_eq!(doc["messages"].as_array().unwrap().len(), 1);
        assert_eq!(doc["working_memory"].as_array().unwrap().len(), 1);
        assert_eq!(doc["workspace_id"], "ws-1");
    }

    #[test]
    fn active_pointer_is_not_listed_as_a_conversation() {
        let _env = TestEnv::new();
        // The pointer lives in data_dir, not conversations_dir, so even
        // though it's a JSON object with no `id` it can't pollute the
        // list. Prove the list stays empty after writing it.
        set_active_conversation(Some("conv-xyz"));
        assert!(list_conversations().is_empty());
    }
}
