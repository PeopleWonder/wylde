//! `api.rs` — the `workspaces.notes.*` IPC verb surface.
//!
//! **Conceptual path:** `Core/Workspaces/Notes/api`.
//!
//! The workspace-notes tier (the middle layer of the 3-layer memory model)
//! moved into this service in Slice 0c. These verbs expose CRUD + scoped
//! search + the reflection proposal primitive over a workspace's
//! `memory.jsonl` bucket. Registration on the pipe lands in
//! [`crate::action_dispatch`].
//!
//! Verb set (Build Order Appendix A, all Slice 0c):
//!
//! * `workspaces.notes.list`    — every note for a workspace.
//! * `workspaces.notes.add`     — append a note (embeds on write).
//! * `workspaces.notes.update`  — edit a note's text (re-embeds).
//! * `workspaces.notes.delete`  — remove a note by id.
//! * `workspaces.notes.search`  — recency+relevance ranked search.
//! * `workspaces.notes.propose` — reflection candidate (not persisted).

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::entry::{self, WorkspaceMemoryEntry};
use super::query::WorkspaceMemoryQuery;
use super::{query, reflection};

fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// `workspaces.notes.list` — every note for a workspace, in stored order.
/// Payload `{ workspace_id }`. Returns `{ notes, count }`.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let notes: Vec<Value> = entry::load(&ws).iter().map(|e| e.to_value()).collect();
    let count = notes.len();
    Reply::ok(json!({ "workspace_id": ws, "notes": notes, "count": count }))
}

/// `workspaces.notes.add` — append a note and embed it on write. Payload
/// `{ workspace_id, text, source? }`. The optional `source` records the
/// note's provenance — the C2b copy-in passes `"long-term-copy"` when the
/// user manually promotes a long-term memory into this workspace; omitted
/// for ordinary notes. Returns the new note.
pub async fn handle_add(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(text) = require_string(&payload, "text") else {
        return Reply::err_msg("bad_request", "text is required");
    };
    let mut e = WorkspaceMemoryEntry::new(entry::new_note_id(), &text);
    if let Some(source) = require_string(&payload, "source") {
        e.source = source;
    }
    // Embed on write so per-turn scoring stays a single dot product. Bounded
    // to stay inside the Medium verb budget: a down/slow embedder is
    // non-fatal — the note persists with an empty embedding (recency only).
    e.embedding = query::embed_text_bounded(&text, query::EMBED_WRITE_BUDGET).await;
    if let Err(err) = entry::append(&ws, e.clone()) {
        return Reply::err_msg("io_error", format!("append note: {err}"));
    }
    Reply::ok(e.to_value())
}

/// `workspaces.notes.update` — replace a note's text and re-embed. Payload
/// `{ workspace_id, id, text }`. `not_found` for an unknown id.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let Some(text) = require_string(&payload, "text") else {
        return Reply::err_msg("bad_request", "text is required");
    };
    match entry::update_text(&ws, &id, &text) {
        Ok(Some(mut updated)) => {
            // Re-embed the new text and persist it in place. Bounded +
            // non-fatal on embedder failure — the edit already stuck
            // (embedding cleared).
            let vec = query::embed_text_bounded(&text, query::EMBED_WRITE_BUDGET).await;
            if !vec.is_empty() {
                let _ = entry::set_embedding(&ws, &id, vec.clone());
                updated.embedding = vec;
            }
            Reply::ok(updated.to_value())
        }
        Ok(None) => Reply::err_msg("not_found", format!("note {id:?} not found")),
        Err(err) => Reply::err_msg("io_error", format!("update note: {err}")),
    }
}

/// `workspaces.notes.delete` — remove a note by id. Payload
/// `{ workspace_id, id }`. Returns `{ ok, id }` (`ok` false when absent).
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match entry::delete(&ws, &id) {
        Ok(deleted) => Reply::ok(json!({ "ok": deleted, "id": id })),
        Err(err) => Reply::err_msg("io_error", format!("delete note: {err}")),
    }
}

/// `workspaces.notes.search` — recency+relevance ranked search over a
/// workspace's notes. Payload `{ workspace_id, query, limit? }`. Fail-soft:
/// an empty bucket / unreachable embedder returns an empty list.
pub async fn handle_search(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let q = payload.get("query").and_then(Value::as_str).unwrap_or("");
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(WorkspaceMemoryQuery::DEFAULT_LIMIT);
    let req = WorkspaceMemoryQuery {
        workspace_id: ws.clone(),
        user_message: q.to_owned(),
        limit,
    };
    let notes: Vec<Value> = query::top_entries_bounded(&req, query::EMBED_WRITE_BUDGET)
        .await
        .iter()
        .map(|e| e.to_value())
        .collect();
    let count = notes.len();
    Reply::ok(json!({ "workspace_id": ws, "notes": notes, "count": count }))
}

/// `workspaces.notes.propose` — reflection candidate (NOT persisted; the
/// user accepts it via `workspaces.notes.add`). Payload
/// `{ workspace_id, text }`. Returns `{ candidate }` or `{ candidate: null }`
/// for blank text.
pub async fn handle_propose(payload: Value) -> Reply {
    let Some(ws) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    match reflection::propose(&ws, text).await {
        Some(candidate) => Reply::ok(json!({ "candidate": candidate.to_value() })),
        None => Reply::ok(json!({ "candidate": Value::Null })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[tokio::test]
    async fn add_list_update_delete_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-notes-api-000000";

        // add
        let added = handle_add(json!({ "workspace_id": ws, "text": "uses pytest" })).await;
        assert!(added.ok, "add failed: {:?}", added.error);
        let id = added.data["id"].as_str().unwrap().to_owned();
        assert_eq!(added.data["text"], "uses pytest");

        // list
        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["notes"][0]["id"], id);

        // update
        let upd =
            handle_update(json!({ "workspace_id": ws, "id": id, "text": "uses cargo" })).await;
        assert!(upd.ok);
        assert_eq!(upd.data["text"], "uses cargo");
        let updated_id_persists = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(updated_id_persists.data["notes"][0]["text"], "uses cargo");

        // delete
        let del = handle_delete(json!({ "workspace_id": ws, "id": id })).await;
        assert_eq!(del.data["ok"], true);
        let empty = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(empty.data["count"], 0);
    }

    #[tokio::test]
    async fn add_with_source_persists_provenance() {
        // The C2b copy-in real path: a long-term item promoted into a
        // workspace's notes carries its `source` provenance tag, and the
        // tag survives the on-disk round-trip exposed by `list`.
        let _env = TestEnv::new();
        let ws = "ws-notes-copyin-000000";
        let added = handle_add(json!({
            "workspace_id": ws,
            "text": "Aaron prefers Bash over PowerShell",
            "source": "long-term-copy",
        }))
        .await;
        assert!(added.ok, "add failed: {:?}", added.error);
        assert_eq!(added.data["source"], "long-term-copy");
        assert_eq!(added.data["text"], "Aaron prefers Bash over PowerShell");

        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["notes"][0]["source"], "long-term-copy");
    }

    #[tokio::test]
    async fn add_without_source_defaults_empty() {
        let _env = TestEnv::new();
        let ws = "ws-notes-nosource-000000";
        let added = handle_add(json!({ "workspace_id": ws, "text": "ambient" })).await;
        assert!(added.ok);
        assert_eq!(added.data["source"], "");
    }

    #[tokio::test]
    async fn add_requires_workspace_and_text() {
        let _env = TestEnv::new();
        let no_ws = handle_add(json!({ "text": "x" })).await;
        assert_eq!(no_ws.error.unwrap().code, "bad_request");
        let no_text = handle_add(json!({ "workspace_id": "ws" })).await;
        assert_eq!(no_text.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn update_unknown_id_is_not_found() {
        let _env = TestEnv::new();
        let r = handle_update(json!({ "workspace_id": "ws", "id": "ghost", "text": "x" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn delete_absent_reports_false() {
        let _env = TestEnv::new();
        let r = handle_delete(json!({ "workspace_id": "ws", "id": "ghost" })).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], false);
    }

    #[tokio::test]
    async fn search_is_fail_soft_empty() {
        let _env = TestEnv::new();
        // No notes + unreachable embedder → empty, never an error.
        let r = handle_search(json!({ "workspace_id": "ws-empty", "query": "anything" })).await;
        assert!(r.ok);
        assert_eq!(r.data["count"], 0);
    }

    #[tokio::test]
    async fn search_returns_added_notes() {
        let _env = TestEnv::new();
        let ws = "ws-notes-search-000000";
        handle_add(json!({ "workspace_id": ws, "text": "alpha" })).await;
        handle_add(json!({ "workspace_id": ws, "text": "beta" })).await;
        let r = handle_search(json!({ "workspace_id": ws, "query": "", "limit": 5 })).await;
        assert_eq!(r.data["count"], 2);
    }

    #[tokio::test]
    async fn propose_returns_candidate_without_persisting() {
        let _env = TestEnv::new();
        let ws = "ws-notes-propose-000000";
        let r = handle_propose(json!({ "workspace_id": ws, "text": "prefers Rust" })).await;
        assert!(r.ok);
        assert_eq!(r.data["candidate"]["text"], "prefers Rust");
        // Not written.
        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(listed.data["count"], 0);
        // Blank text → null candidate.
        let blank = handle_propose(json!({ "workspace_id": ws, "text": "  " })).await;
        assert_eq!(blank.data["candidate"], Value::Null);
    }
}
