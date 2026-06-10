//! Short-term ("working memory") store — Rust port of the
//! working-memory half of `Core/harness/memory/conversation.py`.
//!
//! ## Storage layout
//!
//! Short-term memory is NOT a tier of its own on disk. It lives as the
//! `working_memory` array INSIDE each conversation document at
//! `<conversations_dir>/<id>.json` — the exact same file chat history
//! lives in. So a working-memory entry survives a normal app
//! close/reopen (the JSON file is the durable state) but dies with the
//! conversation when the file is deleted. This mirrors Python's
//! `conversation.py` semantics, which the user explicitly confirmed.
//!
//! ## Why this module only ports the working-memory surface
//!
//! `conversation.py` owns a broader surface — `list_conversations`,
//! `save_conversation`, `delete_conversation`, `set_workspace`, … — that
//! backs the `conversations.*` pipe verbs (ported in
//! [`crate::memory::conversations`]) and the `memory.reflect`
//! consolidation cycle (ported in [`crate::memory::reflection`],
//! which drives [`replace_working_memory`] below for its supersession
//! rewrite). This module ports the three `memory.short_term.*` verbs
//! (`get` / `append` / `clear`), so it implements just enough of the
//! conversation-document read/merge/write path to mutate `working_memory`
//! WITHOUT clobbering the sibling fields (`messages`, `title`,
//! `created_at`, `model`, `workspace_id`) that the Python side still
//! reads and writes through the same file. The merge-save below preserves
//! every field verbatim and only rewrites `working_memory` + `updated_at`,
//! matching `save_conversation`'s field-preservation contract exactly.
//!
//! ## Concurrency
//!
//! Python guards nothing here beyond the atomic-rename write; reads and
//! writes are plain file IO. The Rust port adds a process-wide `Mutex`
//! so a `get` racing an `append` in the same process can't observe a
//! torn read — holds are short (one small JSON file). Cross-process
//! safety still rests on the atomic temp-write + rename, same as Python.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::memory::common::conversations_dir;

/// Serialises in-process working-memory reads/writes. Cross-process
/// torn-write protection still comes from the atomic temp + rename.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Mirrors Python's `_MAX_ID_LEN`.
const MAX_ID_LEN: usize = 128;

/// Conversation id wasn't safe to use as a filename. Mirrors Python's
/// `InvalidConversationId` (a `ValueError` there). The action layer maps
/// this to a `bad_request` reply.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidConversationId(pub String);

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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the conversation document as a JSON object. Returns:
/// * `Ok(Some(map))` — the file exists and parsed to an object.
/// * `Ok(None)` — the file doesn't exist (caller decides stub-vs-empty).
/// * `Err(..)` — the id is invalid.
///
/// Reads through the at-rest encryption layer (OI-14): conversation
/// documents are written encrypted by [`crate::memory::conversations`]
/// and by [`merge_save`] below, and pre-encryption plaintext files stay
/// readable (and lazily migrate) via the same helper. A torn /
/// non-object / undecryptable file is treated as `None` (matches
/// Python's `ConversationNotFound`-on-unreadable, which `get`/`clear`
/// swallow as "no conversation").
fn read_doc(conv_id: &str) -> Result<Option<Map<String, Value>>, InvalidConversationId> {
    validate_id(conv_id)?;
    let path = path_for(conv_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = match wylde_shared::encryption::read_to_string_at_rest(&path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        _ => Ok(None),
    }
}

/// Drop `role == "system"` entries. Mirrors Python's
/// `strip_system_messages` — they're regenerated each turn and would
/// only bloat the file. Idempotent (stored docs are already stripped).
fn strip_system_messages(messages: &Value) -> Vec<Value> {
    messages
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|m| {
                    m.as_object()
                        .and_then(|o| o.get("role"))
                        .and_then(Value::as_str)
                        != Some("system")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Persist `working` as the conversation's working memory, preserving
/// every sibling field. `existing` is the prior document (or `None` to
/// mint a stub). Mirrors the field-preservation half of
/// `save_conversation`: `created_at` is preserved (minted to now for a
/// new doc), `updated_at` bumped, system messages stripped, `model`
/// written only when non-empty.
fn merge_save(
    conv_id: &str,
    existing: Option<Map<String, Value>>,
    working: Vec<Value>,
) -> std::io::Result<()> {
    let now = now_secs();
    let prior = existing.unwrap_or_default();

    let created_at = prior
        .get("created_at")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(now);
    let title = prior
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("Untitled")
        .to_owned();
    let messages = strip_system_messages(prior.get("messages").unwrap_or(&Value::Null));
    let workspace_id = prior
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let mut doc = Map::new();
    doc.insert("id".into(), Value::String(conv_id.to_owned()));
    doc.insert("title".into(), Value::String(title));
    doc.insert("created_at".into(), json!(created_at));
    doc.insert("updated_at".into(), json!(now));
    doc.insert("messages".into(), Value::Array(messages));
    doc.insert("workspace_id".into(), Value::String(workspace_id));
    doc.insert("working_memory".into(), Value::Array(working));
    // `save_conversation` only writes `model` when it resolves to a
    // non-empty value — keep that so the JSON shape stays byte-faithful.
    if let Some(model) = prior
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        doc.insert("model".into(), Value::String(model.to_owned()));
    }

    let body = serde_json::to_string_pretty(&Value::Object(doc))
        .expect("conversation doc serialises to JSON");
    // One write path for every conversation document: `write_at_rest`
    // encrypts (OI-14), writes atomically (temp + rename), and hardens
    // the file owner-only — the same call `conversations::store::
    // write_doc` routes through. (Run-005 fix: writing plaintext here
    // while the conversations store encrypts let a lazy-migration read
    // flip the file to ciphertext mid-flow, after which this module's
    // plain reads saw an unreadable doc and minted a stub over live
    // data — caught by the R2b conversation-reflection tests.)
    wylde_shared::encryption::write_at_rest(&path_for(conv_id), body.as_bytes())
}

/// Working-memory entries for `conv_id`, or `[]` when the conversation
/// doesn't exist / has none. Mirrors Python's `get_working_memory`
/// (which swallows `ConversationNotFound` as `[]`).
pub fn get_working_memory(conv_id: &str) -> Result<Vec<Value>, InvalidConversationId> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(doc) = read_doc(conv_id)? else {
        return Ok(Vec::new());
    };
    Ok(doc
        .get("working_memory")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Append one short-term entry and return the full working-memory list
/// after the append. Mirrors Python's `append_working_memory`: creates a
/// stub conversation if none exists, coerces a non-object entry to
/// `{kind: "raw", at, data: <str>}`, and `setdefault`s the `at`
/// timestamp on object entries.
pub fn append_working_memory(conv_id: &str, entry: Value) -> Result<Vec<Value>, AppendError> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let existing = read_doc(conv_id).map_err(AppendError::InvalidId)?;
    let mut working: Vec<Value> = existing
        .as_ref()
        .and_then(|d| d.get("working_memory"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let stamped = match entry {
        Value::Object(mut map) => {
            // `setdefault("at", now)` — only stamp when absent.
            map.entry("at".to_owned())
                .or_insert_with(|| json!(now_secs()));
            Value::Object(map)
        }
        other => {
            // Non-object entry → the raw-coercion shape Python falls back
            // to. The action layer rejects non-maps before reaching here,
            // so this only fires for direct in-process callers.
            json!({
                "kind": "raw",
                "at": now_secs(),
                "data": value_to_string(&other),
            })
        }
    };
    working.push(stamped);

    merge_save(conv_id, existing, working.clone()).map_err(AppendError::Io)?;
    Ok(working)
}

/// Replace the entire working-memory list, preserving every sibling
/// field and bumping `updated_at`. This is the supersession-rewrite
/// path conversation-scope reflection uses — the Rust analogue of
/// Python `reflection.py` calling
/// `save_conversation(..., working_memory=updated)`. A missing
/// conversation is an upsert-to-stub, same as the append path (the
/// reflection caller never hits that branch — it only rewrites lists
/// it just read).
pub fn replace_working_memory(conv_id: &str, working: Vec<Value>) -> Result<(), AppendError> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let existing = read_doc(conv_id).map_err(AppendError::InvalidId)?;
    merge_save(conv_id, existing, working).map_err(AppendError::Io)
}

/// Drop the short-term entries. Returns `true` iff something was
/// cleared. Mirrors Python's `clear_working_memory`: `false` when the
/// conversation doesn't exist OR already has an empty buffer.
pub fn clear_working_memory(conv_id: &str) -> Result<bool, InvalidConversationId> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(doc) = read_doc(conv_id)? else {
        return Ok(false);
    };
    let has_entries = doc
        .get("working_memory")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_entries {
        return Ok(false);
    }
    // `merge_save` can't fail validation here (read_doc already passed);
    // an IO error is swallowed to match Python's best-effort save, but
    // we still report `true` only when the write lands.
    match merge_save(conv_id, Some(doc), Vec::new()) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Render a non-object JSON value the way Python's `str(entry)` would
/// for the raw-coercion fallback. Strings pass through unquoted.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Error from [`append_working_memory`].
#[derive(Debug)]
pub enum AppendError {
    InvalidId(InvalidConversationId),
    Io(std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::short_term::test_support::TestEnv;

    #[test]
    fn validate_id_accepts_minted_shape() {
        assert!(validate_id("2026-06-04T12-00-00-000000Z-abc123").is_ok());
        assert!(validate_id("default").is_ok());
    }

    #[test]
    fn validate_id_rejects_empty_and_bad_charset() {
        assert!(validate_id("").is_err());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn get_returns_empty_for_unknown_conversation() {
        let _env = TestEnv::new();
        assert_eq!(
            get_working_memory("never-seen").unwrap(),
            Vec::<Value>::new()
        );
    }

    #[test]
    fn get_propagates_invalid_id() {
        let _env = TestEnv::new();
        assert!(get_working_memory("bad/id").is_err());
    }

    #[test]
    fn append_then_get_round_trips_in_order() {
        let _env = TestEnv::new();
        let cid = "round_trip_1";
        append_working_memory(cid, json!({"kind": "tool", "data": {"name": "git_status"}}))
            .unwrap();
        let after =
            append_working_memory(cid, json!({"kind": "decision", "data": "use SQLite"})).unwrap();
        assert_eq!(after.len(), 2);
        let entries = get_working_memory(cid).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["kind"], "tool");
        assert_eq!(entries[1]["kind"], "decision");
    }

    #[test]
    fn append_stamps_at_when_absent_and_preserves_when_present() {
        let _env = TestEnv::new();
        let cid = "stamp_1";
        append_working_memory(cid, json!({"kind": "tool", "data": {}})).unwrap();
        let entries = get_working_memory(cid).unwrap();
        assert!(entries[0].get("at").and_then(Value::as_i64).is_some());

        append_working_memory(cid, json!({"kind": "tool", "at": 42, "data": {}})).unwrap();
        let entries = get_working_memory(cid).unwrap();
        assert_eq!(entries[1]["at"], 42);
    }

    #[test]
    fn append_coerces_non_object_entry() {
        let _env = TestEnv::new();
        let cid = "coerce_1";
        append_working_memory(cid, json!("just a string")).unwrap();
        let entries = get_working_memory(cid).unwrap();
        assert_eq!(entries[0]["kind"], "raw");
        assert_eq!(entries[0]["data"], "just a string");
        assert!(entries[0].get("at").is_some());
    }

    #[test]
    fn clear_returns_false_when_absent_then_true_after_append() {
        let _env = TestEnv::new();
        let cid = "clear_1";
        assert!(!clear_working_memory(cid).unwrap(), "nothing to clear yet");
        append_working_memory(cid, json!({"kind": "tool", "data": {}})).unwrap();
        assert!(clear_working_memory(cid).unwrap(), "had one entry");
        assert_eq!(get_working_memory(cid).unwrap(), Vec::<Value>::new());
        assert!(!clear_working_memory(cid).unwrap(), "already empty");
    }

    #[test]
    fn working_memory_survives_reopen_and_preserves_sibling_fields() {
        let _env = TestEnv::new();
        let cid = "persist_1";
        // Seed a conversation file with messages + title + workspace +
        // model the way the Python `save_conversation` would, so we can
        // prove the merge-save doesn't clobber them.
        let path = path_for(cid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "id": cid,
                "title": "My Chat",
                "created_at": 1000,
                "updated_at": 1000,
                "model": "qwen2.5",
                "workspace_id": "ws-1",
                "messages": [
                    {"role": "system", "content": "stripme"},
                    {"role": "user", "content": "hi"}
                ],
                "working_memory": []
            }))
            .unwrap(),
        )
        .unwrap();

        append_working_memory(cid, json!({"kind": "summary", "data": "found it"})).unwrap();

        // Re-read the raw file: working_memory landed, siblings intact,
        // system message stripped, created_at preserved.
        let raw: Value =
            serde_json::from_str(&wylde_shared::encryption::read_to_string_at_rest(&path).unwrap())
                .unwrap();
        assert_eq!(raw["title"], "My Chat");
        assert_eq!(raw["created_at"], 1000);
        assert_eq!(raw["model"], "qwen2.5");
        assert_eq!(raw["workspace_id"], "ws-1");
        assert_eq!(raw["working_memory"].as_array().unwrap().len(), 1);
        let msgs = raw["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "system message should be stripped");
        assert_eq!(msgs[0]["role"], "user");
        // updated_at advanced past created_at.
        assert!(raw["updated_at"].as_i64().unwrap() >= 1000);
    }

    #[test]
    fn replace_working_memory_swaps_list_and_preserves_siblings() {
        let _env = TestEnv::new();
        let cid = "replace_1";
        append_working_memory(cid, json!({"kind": "tool", "at": 1, "data": {}})).unwrap();
        append_working_memory(cid, json!({"kind": "decision", "at": 2, "data": "x"})).unwrap();

        let mut entries = get_working_memory(cid).unwrap();
        entries[0]
            .as_object_mut()
            .unwrap()
            .insert("superseded_by".into(), json!("ref-1"));
        replace_working_memory(cid, entries.clone()).unwrap();

        let after = get_working_memory(cid).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0]["superseded_by"], "ref-1");
        assert!(after[1].get("superseded_by").is_none());
        // Sibling fields intact.
        let raw: Value = serde_json::from_str(
            &wylde_shared::encryption::read_to_string_at_rest(&path_for(cid)).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["id"], cid);
        assert_eq!(raw["title"], "Untitled");
        // Invalid ids still rejected.
        assert!(replace_working_memory("bad/id", vec![]).is_err());
    }

    #[test]
    fn append_mints_stub_for_unknown_conversation() {
        let _env = TestEnv::new();
        let cid = "stub_1";
        append_working_memory(cid, json!({"kind": "tool", "data": {}})).unwrap();
        let raw: Value = serde_json::from_str(
            &wylde_shared::encryption::read_to_string_at_rest(&path_for(cid)).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["id"], cid);
        assert_eq!(raw["title"], "Untitled");
        assert_eq!(raw["workspace_id"], "");
        assert_eq!(raw["messages"].as_array().unwrap().len(), 0);
        assert!(raw.get("model").is_none(), "no model key when empty");
    }
}
