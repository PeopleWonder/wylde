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
            // Slice F-data — (re)build the active workspace's symbol index in
            // the background so `symbols.find` is warm. Same MRU model: one
            // workspace's index in memory at a time. No-op until armed.
            crate::graph::symbol_index::on_active_changed();
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
        Err(_) => {
            return Reply::err_msg("bad_request", format!("folder does not exist: {folder:?}"))
        }
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
        // Slice F-data — same re-evaluation for the symbol index: a deleted
        // active workspace clears the pointer, so this drops its index.
        crate::graph::symbol_index::on_active_changed();
        // C9 — Route 1 deletion sweep. The registry's bundle-dir removal
        // cascades the *legacy* per-workspace service-store conversations, but
        // under Route 1 a workspace's live **bound** conversations live in the
        // harness flat store (`<data_dir>/conversations/<id>.json` with a
        // matching `workspace_id`), which the bundle removal never touches.
        // Ask the harness — the canonical owner of that store — to sweep them,
        // or they orphan in the global list forever. Fire-and-forget for the
        // same reason as the graph prune below (a Fast/Medium verb must not
        // block on a peer service) and so a unit-test delete never stalls on a
        // pipe connect; best-effort, so an unreachable/slow harness only logs.
        let sweep_ws = id.clone();
        tokio::spawn(async move {
            let sweep = wylde_shared::ipc::send_action(
                "wylde-harness",
                "conversations.delete_by_workspace",
                json!({ "workspace_id": sweep_ws.clone() }),
            )
            .await;
            if !sweep.ok {
                tracing::warn!(
                    "workspaces.delete: flat-store conversation sweep degraded for {sweep_ws}: {:?}",
                    sweep.error
                );
            }
        });
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

/// Join a workspace definition with its live index state (F4). The
/// config-only [`registry::WorkspaceDefinition::to_value`] omits
/// `file_count` / `last_indexed_at` / `indexing` — those live in a separate
/// `rag_state.json` ([`indexer::status`]) that no read joined, which is why
/// the Registry showed "Last index: never" permanently even after a
/// successful index. Merge the [`store::RagState`] snapshot onto the
/// definition object so every list row carries live index state.
///
/// `last_indexed_at` is the raw epoch-seconds `f64` from `RagState`; the GUI
/// formats it for display (0.0 ⇒ "never"). Fields are additive, so the
/// InferenceBar / Memory dropdown consumers that read `list_mru` are unaffected.
fn def_with_index_state(def: &registry::WorkspaceDefinition) -> Value {
    let mut v = def.to_value();
    let st = indexer::status(&def.id);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("indexing".to_owned(), json!(st.indexing));
        obj.insert("last_indexed_at".to_owned(), json!(st.last_indexed_at));
        obj.insert("file_count".to_owned(), json!(st.file_count));
        obj.insert("chunk_count".to_owned(), json!(st.chunk_count));
        obj.insert("last_error".to_owned(), json!(st.last_error));
    }
    v
}

/// `workspaces.list_mru` — MRU-5 workspace list + active id for the
/// InferenceBar dropdown and the Registry list. No payload.
///
/// Each row is the workspace definition joined with its live index state
/// (F4) so `file_count` / `last_indexed_at` / `indexing` survive a reload —
/// previously they were carried only in the one-shot `reindex` reply and
/// lost on refresh.
pub async fn handle_list_mru(_payload: Value) -> Reply {
    let (defs, active_id) = registry::list_mru();
    let workspaces: Vec<Value> = defs.iter().map(def_with_index_state).collect();
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

/// `workspaces.reindex_purge` — drop already-indexed chunks whose path the
/// current exclusion matcher now excludes (the index-hygiene one-time purge),
/// filter-only (no re-embed). Payload: `{ "workspace_id": string }`. Returns
/// the [`indexer::purge::PurgeOutcome`] `{ before, dropped, kept, files_dropped,
/// excluded_remaining, graph_cleaned, graph_error }`. Idempotent — a clean
/// index drops nothing. Re-cluster the concepts afterward
/// (`workspaces.concepts.build_semantic`) so they re-derive from real source.
pub async fn handle_reindex_purge(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(def) = registry::get(&id) else {
        return Reply::err_msg("not_found", format!("workspace {id:?} not found"));
    };
    let outcome = indexer::purge::purge_excluded(&def).await;
    let mut v = outcome.to_value();
    if let Value::Object(ref mut map) = v {
        map.insert("workspace_id".to_owned(), json!(id));
        map.insert("ok".to_owned(), json!(true));
    }
    Reply::ok(v)
}

/// `workspaces.rag.walk_preview` — read-only dry-run of the walk-time exclusion
/// over a workspace folder. Payload: `{ "workspace_id": string, "sample"?: n }`.
/// Returns `{ workspace_id, would_index, would_exclude, sample_excluded:[paths] }`
/// so the matcher's effect can be confirmed before committing to a purge. Walks
/// the raw tree (no embed, no persist); `sample` caps the excluded-path sample
/// (default 20).
pub async fn handle_walk_preview(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(def) = registry::get(&id) else {
        return Reply::err_msg("not_found", format!("workspace {id:?} not found"));
    };
    let sample_cap = payload
        .get("sample")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(20);
    let preview = indexer::walk::walk_preview(&def.folder, sample_cap);
    Reply::ok(json!({
        "workspace_id": id,
        "would_index": preview.would_index,
        "would_exclude": preview.would_exclude,
        "sample_excluded": preview.sample_excluded,
    }))
}

// ── settings.lexical.* (the lexical/BM25 + RRF master toggle, lexical-bm25
//    plan L0) — the GUI's write facade over the service-owned `LexicalConfig`
//    store, so there is ONE source of truth read in-process by the RAG search
//    hot path (no TCP↔pipe drift). Mirrors `settings.concept_routing.*`,
//    relocated to this service because *its* consumer (`search.rs`) lives here.

/// `settings.lexical.get {}` — the full lexical config. Reply: the serialized
/// [`LexicalConfig`](super::rag::LexicalConfig) (`{enabled, rrf_k, w_dense,
/// w_lex, min_bm25, fused_relative_floor, active_file_focus_boost,
/// active_file_dir_focus_boost}`). Default-off on a fresh install.
pub async fn handle_lexical_get(_payload: Value) -> Reply {
    Reply::ok(super::rag::LexicalConfig::current().to_value())
}

/// `settings.lexical.set {...}` — persist the lexical config. Every field is
/// optional; an omitted field keeps its current value (a partial patch), so the
/// GUI can flip just `enabled` without resending the knobs. Reply: the persisted
/// config. The master toggle defaults off and only ever turns on by an explicit,
/// persisted opt-in here.
pub async fn handle_lexical_set(payload: Value) -> Reply {
    if !payload.is_object() {
        return Reply::err_msg("bad_request", "payload must be an object");
    }
    // Merge the incoming patch over the current config so callers can send only
    // the keys they're changing, then re-parse through the tolerant loader
    // (unknown/garbage keys fall back to current values, never fail open).
    let mut merged = super::rag::LexicalConfig::current().to_value();
    if let (Some(base), Some(patch)) = (merged.as_object_mut(), payload.as_object()) {
        for (k, v) in patch {
            base.insert(k.clone(), v.clone());
        }
    }
    let next = super::rag::LexicalConfig::from_value(&merged);
    match super::rag::LexicalConfig::persist(next) {
        Ok(()) => Reply::ok(next.to_value()),
        Err(e) => Reply::err_msg("io_error", format!("persist lexical: {e}")),
    }
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
    // Concept-routing master toggle, forwarded by the harness (default false ⇒
    // the pre-routing path; the field is absent on every non-routing caller).
    let route = payload
        .get("route")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Concept-routing R2: the user-curated concept ids (plan §4). `Some` (even
    // empty) ⇒ the curate-before-inject menu ran and these are the concepts to
    // Augment-inject; absent ⇒ no injection (R1 behaviour). Parsed as an
    // Option so "field absent" and "explicitly curated to nothing" stay
    // distinguishable — the latter must inject nothing without re-routing.
    let curated_concepts: Option<Vec<String>> = payload.get("curated_concepts").map(|v| {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    });
    let ctx = prompt::gather(&id, user_message, route, curated_concepts.as_deref()).await;
    let slots = prompt::render_slots(&ctx);
    // R1: surface the candidate set (logged server-side) so the harness can log
    // it from its single gather site too. `null` when routing was off or found
    // nothing to route against — never injected (injection is R2).
    let route_candidates = ctx
        .route_candidates
        .as_ref()
        .and_then(|c| serde_json::to_value(c).ok())
        .unwrap_or(Value::Null);
    Reply::ok(json!({
        "workspace_id": id,
        "slots": slots,
        "persona": ctx.persona,
        "memory_snippets": ctx.memory_snippets,
        "rag_snippets": ctx.rag_snippets,
        "route_candidates": route_candidates,
        // R2: the Augment-injection blocks (boundary blurb + member snippets)
        // the harness renders into its dedicated `### Concepts` slot. Empty
        // unless a non-empty curated set was injected.
        "concept_context": ctx.concept_context,
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
    async fn list_mru_joins_index_state() {
        // F4: list_mru must carry the RagState fields (file_count /
        // last_indexed_at / indexing) joined from rag_state.json, so the
        // Registry stops showing "Last index: never" after an index.
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("idx-ws");
        std::fs::create_dir(&p).unwrap();
        let created = handle_create(json!({ "folder": p.to_string_lossy() })).await;
        let id = created.data["id"].as_str().unwrap().to_owned();

        // Before any index: fields are present and zero/false (not absent).
        let listed = handle_list_mru(Value::Null).await;
        let row = &listed.data["workspaces"][0];
        assert_eq!(row["file_count"], json!(0));
        assert_eq!(row["last_indexed_at"], json!(0.0));
        assert_eq!(row["indexing"], json!(false));

        // Simulate a completed index by writing RagState directly.
        indexer::store::save_state(
            &id,
            &indexer::store::RagState {
                indexing: false,
                last_indexed_at: 1_781_470_631.0,
                file_count: 7,
                chunk_count: 42,
                last_error: None,
            },
        )
        .unwrap();

        let listed = handle_list_mru(Value::Null).await;
        let row = &listed.data["workspaces"][0];
        assert_eq!(row["file_count"], json!(7), "count must survive reload");
        assert_eq!(row["chunk_count"], json!(42));
        assert_eq!(row["last_indexed_at"], json!(1_781_470_631.0));
        assert_eq!(row["indexing"], json!(false));
        // Definition fields are still present (join, not replace).
        assert_eq!(row["id"], json!(id));
        assert_eq!(row["rag_enabled"], json!(true));
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
