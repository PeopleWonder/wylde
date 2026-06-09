//! `api.rs` — the minimal IPC verb surface for workspaces.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/api`.
//!
//! ## Design stance: thin, write-mostly
//!
//! Workspaces are config files, so the harness owns *writes* (single
//! writer + validation) plus the *active-selection* pointer the turn
//! driver consumes, and exposes one *read* verb (`list_mru`) for the
//! InferenceBar dropdown. This is the deliberate opposite of the retired
//! `rag.workspaces.*` / `memory.workspaces.*` surfaces, which exposed a
//! full per-attribute CRUD API.
//!
//! Final verb set (design doc §5, Q1 — all five up front + one read):
//!
//! * `workspaces.set_active` — set the active workspace + bump MRU.
//! * `workspaces.create` — register a folder as a workspace.
//! * `workspaces.update` — rename / toggle `persona_enabled` / `rag_enabled`.
//! * `workspaces.delete` — remove a workspace + its `<workspace_id>/` dir.
//! * `workspaces.set_persona` — write `persona.md`.
//! * `workspaces.list_mru` — MRU-5 list (+ active id) for the dropdown.
//!
//! (`conversations.set_workspace`, the Q4 mutable-binding verb, lives in
//! the harness `memory::conversations::actions` alongside the other
//! conversation verbs.)
//!
//! Registration on the pipe lands in [`crate::action_dispatch`].

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::rag::indexer;
use super::{persona, prompt, registry};

fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// `workspaces.set_active` — set the active workspace and move it to the
/// head of the MRU list. Payload: `{ "workspace_id": string }`.
pub async fn handle_set_active(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    match registry::set_active(&id) {
        Ok(state) => {
            // Activating an existing workspace delta-reindexes its folder
            // in the background (mirrors the retired Python `activate` →
            // `_index_delta`); a never-indexed workspace gets a full pass.
            indexer::spawn_background_index(id.clone());
            // Slice I — follow the active pointer: tear down the previous
            // workspace's watcher and start one for this folder so its graph
            // stays fresh from now on. No-op until the live service arms it.
            crate::watcher::on_active_changed();
            Reply::ok(json!({
                "active_id": state.active_id,
                "mru": state.mru,
            }))
        }
        Err(registry::RegistryError::NotFound(_)) => {
            Reply::err_msg("not_found", format!("workspace {id:?} not found"))
        }
    }
}

/// `workspaces.create` — register a folder as a workspace (and activate
/// it). Payload: `{ "folder": string, "name"?: string }`.
pub async fn handle_create(payload: Value) -> Reply {
    let Some(folder) = require_string(&payload, "folder") else {
        return Reply::err_msg("bad_request", "folder is required");
    };
    match std::fs::metadata(&folder) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Reply::err_msg("bad_request", format!("not a directory: {folder:?}")),
        Err(_) => return Reply::err_msg("bad_request", format!("folder does not exist: {folder:?}")),
    }
    let name = payload.get("name").and_then(Value::as_str);
    let def = registry::create(&folder, name);
    // Index the folder in the background so create stays non-blocking;
    // first-time create has no index yet → a full pass.
    indexer::spawn_background_index(def.id.clone());
    Reply::ok(def.to_value())
}

/// `workspaces.update` — rename / toggle feature flags.
/// Payload: `{ "workspace_id": string, "name"?: string,
/// "persona_enabled"?: bool, "rag_enabled"?: bool }`.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let name = payload.get("name").and_then(Value::as_str);
    let persona_enabled = payload.get("persona_enabled").and_then(Value::as_bool);
    let rag_enabled = payload.get("rag_enabled").and_then(Value::as_bool);
    match registry::update(&id, name, persona_enabled, rag_enabled) {
        Some(def) => Reply::ok(def.to_value()),
        None => Reply::err_msg("not_found", format!("workspace {id:?} not found")),
    }
}

/// `workspaces.delete` — remove a workspace + its data dir.
/// Payload: `{ "workspace_id": string }`.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let ok = registry::delete(&id);
    if ok {
        // Re-evaluate the watcher: if the deleted workspace was active, the
        // registry cleared the active pointer, so this stops the watch.
        crate::watcher::on_active_changed();
        // Slice I — also clean up the workspace's Neo4j footprint (the Slice A
        // report flagged that `delete` left graph nodes behind). Fire-and-
        // forget: a Bolt connect can take seconds when the graph is down, and
        // `workspaces.delete` is a Fast/Medium verb — it must NOT block on the
        // graph. The registry delete already succeeded; the graph prune is
        // best-effort cleanup that can't fail the response.
        let ws = id.clone();
        tokio::spawn(async move {
            let cleanup = crate::graph::BoltClient::new().delete_workspace(&ws).await;
            if !cleanup.ok {
                tracing::warn!(
                    "workspaces.delete: graph cleanup degraded for {ws}: {:?}",
                    cleanup.error
                );
            }
        });
    }
    Reply::ok(json!({ "ok": ok, "workspace_id": id }))
}

/// `workspaces.set_persona` — write `persona.md` for a workspace and
/// enable/disable the persona slot based on whether text was supplied.
/// Payload: `{ "workspace_id": string, "text"?: string }`.
pub async fn handle_set_persona(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    if registry::get(&id).is_none() {
        return Reply::err_msg("not_found", format!("workspace {id:?} not found"));
    }
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    if let Err(e) = persona::save(&id, text) {
        return Reply::err_msg("io_error", format!("write persona.md: {e}"));
    }
    // Writing a persona implies enabling it; clearing it disables.
    registry::update(&id, None, Some(!text.trim().is_empty()), None);
    Reply::ok(json!({ "ok": true, "workspace_id": id }))
}

/// `workspaces.list_mru` — MRU-5 workspace list + active id for the
/// InferenceBar dropdown. No payload.
pub async fn handle_list_mru(_payload: Value) -> Reply {
    let (defs, active_id) = registry::list_mru();
    let workspaces: Vec<Value> = defs.iter().map(|d| d.to_value()).collect();
    Reply::ok(json!({ "workspaces": workspaces, "active_id": active_id }))
}

/// `workspaces.rag_query` — k-NN search over a workspace's file index.
/// Payload: `{ "workspace_id": string, "query": string, "k"?: number }`.
/// Returns `{ hits: [{file_path, line_range, content, score, chunk_idx}] }`.
///
/// Fail-soft: an unknown workspace, a missing/empty index, or an
/// unreachable embedder all return an empty `hits` list (never an error),
/// preserving the pointer-only fallback.
pub async fn handle_rag_query(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let query = payload.get("query").and_then(Value::as_str).unwrap_or("");
    let k = payload
        .get("k")
        .and_then(Value::as_u64)
        .map(|k| k as usize)
        .unwrap_or(super::rag::WorkspaceRagScope::DEFAULT_LIMIT);
    let hits = indexer::search::query(&id, query, k).await;
    let hits: Vec<Value> = hits.iter().map(|h| h.to_value()).collect();
    Reply::ok(json!({ "workspace_id": id, "hits": hits }))
}

/// `workspaces.reindex` — force a synchronous full reindex of a
/// workspace's folder (the GUI "Reindex" button). Payload:
/// `{ "workspace_id": string }`. Returns the resulting
/// `{ ok, file_count, chunk_count, indexing, last_error }` status.
pub async fn handle_reindex(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(def) = registry::get(&id) else {
        return Reply::err_msg("not_found", format!("workspace {id:?} not found"));
    };
    let outcome = indexer::reindex_full(&def).await;
    Reply::ok(json!({
        "ok": outcome.error.is_none(),
        "workspace_id": id,
        "file_count": outcome.file_count,
        "chunk_count": outcome.chunk_count,
        "last_error": outcome.error,
    }))
}

/// `workspaces.gather_prompt` — resolve a workspace's contribution to a
/// chat turn's system prompt (persona + notes + RAG), rendered into the
/// slot text the harness turn driver appends. Payload:
/// `{ "workspace_id": string, "user_message"?: string }`.
///
/// Returns `{ slots, persona, memory_snippets, rag_snippets }`. `slots`
/// is the ready-to-append rendered block (empty when the workspace
/// contributes nothing or the id is unknown/blank); the structured fields
/// are surfaced for future consumers. This is the read the chat turn
/// driver calls via the client once per turn; the client treats it as
/// best-effort enrichment and degrades to base context when the service
/// is unreachable.
pub async fn handle_gather_prompt(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let user_message = payload
        .get("user_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let ctx = prompt::gather(&id, user_message).await;
    let slots = prompt::render_slots(&ctx);
    Reply::ok(json!({
        "workspace_id": id,
        "slots": slots,
        "persona": ctx.persona,
        "memory_snippets": ctx.memory_snippets,
        "rag_snippets": ctx.rag_snippets,
    }))
}

/// `workspaces.watcher.status` — file-watcher observability snapshot. No
/// payload. Returns `{active_workspace, files_watched, last_event_at, paused}`.
pub async fn handle_watcher_status(_payload: Value) -> Reply {
    Reply::ok(crate::watcher::status().to_value())
}

/// `workspaces.watcher.pause` — pause the active workspace's watcher (e.g.
/// before a big checkout, so the user isn't flooded with delta-upserts). No
/// payload. Idempotent: a no-op (with `active_workspace: null`) when nothing
/// is watched.
pub async fn handle_watcher_pause(_payload: Value) -> Reply {
    let active = crate::watcher::pause();
    Reply::ok(json!({ "ok": true, "paused": true, "active_workspace": active }))
}

/// `workspaces.watcher.resume` — resume the watcher and re-walk the workspace
/// to catch up on edits missed while paused. No payload.
pub async fn handle_watcher_resume(_payload: Value) -> Reply {
    let active = crate::watcher::resume();
    Reply::ok(json!({ "ok": true, "paused": false, "active_workspace": active }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use tempfile::tempdir;

    #[tokio::test]
    async fn create_requires_existing_directory() {
        let _env = TestEnv::new();
        let reply = handle_create(json!({ "folder": "/no/such/dir/xyz-123" })).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn create_then_list_mru_then_set_active() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("proj");
        std::fs::create_dir(&p).unwrap();

        let created = handle_create(json!({ "folder": p.to_string_lossy(), "name": "Proj" })).await;
        assert!(created.ok, "create failed: {:?}", created.error);
        let id = created.data["id"].as_str().unwrap().to_owned();
        assert_eq!(created.data["name"], "Proj");

        let listed = handle_list_mru(Value::Null).await;
        assert!(listed.ok);
        assert_eq!(listed.data["workspaces"].as_array().unwrap().len(), 1);
        assert_eq!(listed.data["active_id"], id);

        let active = handle_set_active(json!({ "workspace_id": id })).await;
        assert!(active.ok);
        assert_eq!(active.data["active_id"], id);
    }

    #[tokio::test]
    async fn set_active_unknown_is_not_found() {
        let _env = TestEnv::new();
        let reply = handle_set_active(json!({ "workspace_id": "nope-000000" })).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn set_persona_enables_then_clears() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("persona-ws");
        std::fs::create_dir(&p).unwrap();
        let created = handle_create(json!({ "folder": p.to_string_lossy() })).await;
        let id = created.data["id"].as_str().unwrap().to_owned();

        let set = handle_set_persona(json!({ "workspace_id": id, "text": "Be brief." })).await;
        assert!(set.ok);
        assert!(registry::get(&id).unwrap().persona_enabled);
        assert_eq!(persona::load(&id).text, "Be brief.");

        let clear = handle_set_persona(json!({ "workspace_id": id, "text": "" })).await;
        assert!(clear.ok);
        assert!(!registry::get(&id).unwrap().persona_enabled);
    }

    #[tokio::test]
    async fn gather_prompt_blank_workspace_yields_empty_slots() {
        let _env = TestEnv::new();
        let reply = handle_gather_prompt(json!({ "workspace_id": "ghost-000000" })).await;
        assert!(reply.ok);
        assert_eq!(reply.data["slots"], "");
        assert_eq!(reply.data["persona"], "");
    }

    #[tokio::test]
    async fn gather_prompt_requires_workspace_id() {
        let _env = TestEnv::new();
        let reply = handle_gather_prompt(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn gather_prompt_renders_persona_slot() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("gather-ws");
        std::fs::create_dir(&p).unwrap();
        let id = handle_create(json!({ "folder": p.to_string_lossy() }))
            .await
            .data["id"]
            .as_str()
            .unwrap()
            .to_owned();
        handle_set_persona(json!({ "workspace_id": id, "text": "Be brief." })).await;

        let reply = handle_gather_prompt(json!({ "workspace_id": id, "user_message": "hi" })).await;
        assert!(reply.ok);
        assert_eq!(reply.data["persona"], "Be brief.");
        let slots = reply.data["slots"].as_str().unwrap();
        assert!(slots.contains("# Workspace context"));
        assert!(slots.contains("Be brief."));
    }

    #[tokio::test]
    async fn update_and_delete() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("upd-ws");
        std::fs::create_dir(&p).unwrap();
        let id = handle_create(json!({ "folder": p.to_string_lossy() }))
            .await
            .data["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let upd = handle_update(json!({ "workspace_id": id, "rag_enabled": false })).await;
        assert!(upd.ok);
        assert_eq!(upd.data["rag_enabled"], false);

        let del = handle_delete(json!({ "workspace_id": id })).await;
        assert!(del.ok);
        assert_eq!(del.data["ok"], true);
        assert!(registry::get(&id).is_none());
    }
}
