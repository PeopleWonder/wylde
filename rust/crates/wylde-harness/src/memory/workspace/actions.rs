//! `memory.workspace.*` IPC action handlers.
//!
//! The workspace-memory verbs the gateway / GUI consume:
//!
//! * `memory.workspace.list`   → `{ memories, count, workspace_id }`
//! * `memory.workspace.search` → `{ hits: [...] }`
//! * `memory.workspace.save`   → the record object
//! * `memory.workspace.update` → the replacement record (revision)
//! * `memory.workspace.delete` → `{ ok, workspace_id, id }`
//! * `memory.workspace.delete_all` → `{ ok, workspace_id, removed }`
//! * `memory.workspace.curate` → the skipped `CurationResult` shape
//!
//! Reply shapes + error codes/messages match the Python `_memory.py`
//! handlers they replace exactly (the gateway depends on them). The
//! storage work lives in [`super::store`]; entity → graph edges are
//! fire-and-forget (see [`record_entities_best_effort`]) so Memgraph
//! being down can never block or fail a save.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::record::WorkspaceMemory;
use super::store::{self, SaveError};
use crate::api::require_string;
use crate::memory::memgraph::BoltClient;

/// Default search hit count when neither `k` nor `limit` is supplied.
/// Mirrors Python's `int(p.get("k") or p.get("limit") or 5)`.
pub const SEARCH_LIMIT_DEFAULT: usize = 5;

/// Hard cap on requested search hits — mirrors Python's
/// `max(1, min(50, ...))` clamp.
pub const SEARCH_LIMIT_MAX: usize = 50;

/// `memory.workspace.list` — every record for a workspace, importance
/// desc then recency desc. Payload `{ workspace_id, include_superseded? }`.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let include = payload
        .get("include_superseded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let records: Vec<Value> = store::list_records(&wsid, include)
        .iter()
        .map(WorkspaceMemory::to_value)
        .collect();
    let count = records.len();
    Reply::ok(json!({
        "memories": records,
        "count": count,
        "workspace_id": wsid,
    }))
}

/// `memory.workspace.search` — scored text retrieval. Payload
/// `{ workspace_id, query, k?|limit? }`; the hit count clamps to
/// `1..=50`, default 5 (`k` wins over `limit`, zero values fall
/// through — Python's `or` chain).
pub async fn handle_search(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(query) = require_string(&payload, "query").filter(|q| !q.trim().is_empty()) else {
        return Reply::err_msg("bad_request", "query is required");
    };
    let limit = search_limit(&payload);
    // Text-overlap baseline always computed — it's the safe fallback and
    // it preserves recall for records the vector mirror doesn't cover.
    let text_hits = store::search_records(&wsid, &query, limit, None);
    // Upgrade to semantic ranking when the mirror is populated AND the
    // query embeds; otherwise stay on text (dev / embedder-down path).
    let ranked = if store::vector_mirror_is_empty(&wsid) {
        text_hits
    } else if let Some(query_vector) = crate::memory::embed_write::embed_for_write(&query).await {
        let vector_hits = store::search_records_vector(&wsid, query_vector, limit, None);
        store::merge_hits(vector_hits, text_hits, limit)
    } else {
        text_hits
    };
    let hits: Vec<Value> = ranked.iter().map(|h| h.to_value()).collect();
    Reply::ok(json!({ "hits": hits }))
}

/// `memory.workspace.save` — write a new workspace memory. Payload
/// `{ workspace_id, body, source?, importance?, entities?[] }`.
/// Returns the record object. Entity → graph edges are written
/// best-effort after the JSON store wins.
pub async fn handle_save(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(body) = require_string(&payload, "body").filter(|b| !b.trim().is_empty()) else {
        return Reply::err_msg("bad_request", "body is required");
    };
    let source = payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let importance = payload.get("importance").and_then(Value::as_f64);
    let entities = entities_from(&payload).unwrap_or_default();

    match store::save_new(&wsid, &body, &source, importance, entities) {
        Ok(record) => {
            // Populate the per-workspace vector mirror so search can rank
            // this record semantically (budgeted, fail-soft — an absent
            // embedder just leaves it to text search).
            let vector = crate::memory::embed_write::embed_for_write(&record.body).await;
            store::vector_upsert(&wsid, &record.id, vector);
            record_entities_best_effort(&record);
            Reply::ok(record.to_value())
        }
        Err(SaveError::EmptyBody) => Reply::err_msg("bad_request", "body is required"),
        Err(SaveError::EmptyWorkspaceId) => {
            Reply::err_msg("bad_request", "workspace_id is required")
        }
        Err(SaveError::Io(e)) => Reply::err_msg("io_error", e.to_string()),
    }
}

/// `memory.workspace.update` — revision-not-deletion. Payload
/// `{ workspace_id, id, body?, importance?, entities? }`. Returns the
/// replacement record, or `not_found` with the Python-format message
/// `memory '<id>' not in '<workspace_id>'`.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(rid) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let body = payload.get("body").and_then(Value::as_str);
    let importance = payload.get("importance").and_then(Value::as_f64);
    let entities = entities_from(&payload);

    match store::update(&wsid, &rid, body, importance, entities) {
        Some(record) => {
            // The revision is a NEW record id — mirror its (possibly
            // unchanged) body so the vector store tracks the live text.
            let vector = crate::memory::embed_write::embed_for_write(&record.body).await;
            store::vector_upsert(&wsid, &record.id, vector);
            record_entities_best_effort(&record);
            Reply::ok(record.to_value())
        }
        None => Reply::err_msg("not_found", format!("memory '{rid}' not in '{wsid}'")),
    }
}

/// `memory.workspace.delete` — remove a record (and its superseded
/// predecessors). Payload `{ workspace_id, id }`. Returns
/// `{ ok, workspace_id, id }`; `ok` is `false` for an unknown id.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(rid) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let ok = store::delete(&wsid, &rid);
    Reply::ok(json!({ "ok": ok, "workspace_id": wsid, "id": rid }))
}

/// `memory.workspace.delete_all` — remove a workspace's ENTIRE durable
/// memory directory: every record, the vector mirror, and the folder itself.
/// Payload `{ workspace_id }`. Returns `{ ok: true, workspace_id, removed }`
/// where `removed` is whether a folder was actually there to delete.
///
/// The teardown complement to the workspaces service's bundle removal (#135).
/// `workspace_memories/<id>/` lives OUTSIDE the workspace bundle on purpose —
/// so MRU eviction of a file index never takes the curated memories with it —
/// but that also put it outside the reach of every removal path, so an
/// explicitly deleted workspace left its memories on disk forever. Since a
/// workspace id is derived from its folder (#28), re-registering the same
/// folder re-derived the same id and silently re-attached memories the user
/// believed they had deleted: a privacy consequence, not just a disk one.
///
/// **Only the explicit-delete path may call this.** MRU eviction must not —
/// surviving eviction is the whole point of the durable tier. That asymmetry
/// is enforced at the caller: the workspaces service invokes this from
/// `handle_delete`, never from the shared `teardown_bundle` primitive that
/// eviction also funnels through.
///
/// A blank id is rejected rather than treated as "no workspace": the store
/// path for an empty id is the tier ROOT, so obeying it would wipe every
/// workspace's memories (the store guards this too — defence in depth).
pub async fn handle_delete_all(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    if wsid.trim().is_empty() {
        return Reply::err_msg("bad_request", "workspace_id must not be blank");
    }
    let removed = store::delete_memory_dir(&wsid);
    Reply::ok(json!({ "ok": true, "workspace_id": wsid, "removed": removed }))
}

/// `memory.workspace.curate` — trigger LLM-driven curation. Payload
/// `{ workspace_id }`. Always returns the skipped `CurationResult`
/// shape (`skipped: true, skip_reason: "no chat_fn supplied"`) because
/// a chat function isn't injectable across the wire — exactly what the
/// Python action returned. The scheduler runs real passes via
/// [`super::curate_with_chat`].
pub async fn handle_curate(payload: Value) -> Reply {
    let Some(wsid) = require_string(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let result = super::curate_with_chat(&wsid, None).await;
    Reply::ok(result.to_value())
}

// ── Helpers ───────────────────────────────────────────────────────────

/// `k` then `limit`, skipping missing / non-numeric / zero values
/// (Python's falsy `or` chain), truncated to int and clamped to
/// `1..=SEARCH_LIMIT_MAX`. Default [`SEARCH_LIMIT_DEFAULT`].
fn search_limit(payload: &Value) -> usize {
    let raw = nonzero_f64(payload.get("k")).or_else(|| nonzero_f64(payload.get("limit")));
    let n = raw.unwrap_or(SEARCH_LIMIT_DEFAULT as f64) as i64;
    n.clamp(1, SEARCH_LIMIT_MAX as i64) as usize
}

fn nonzero_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| *n != 0.0)
}

/// `entities` as a string list when the payload carries a list;
/// `None` otherwise (the update path distinguishes "not supplied"
/// from "supplied empty" — Python's `isinstance(..., list)` check).
fn entities_from(payload: &Value) -> Option<Vec<String>> {
    payload
        .get("entities")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
}

/// Best-effort write of entity → memory edges into the graph. Rust
/// port of Python's `_record_entities`: each entity is upserted as an
/// `:Entity` node, the memory becomes a `:Chunk` tagged with the
/// workspace id, and `MENTIONED_IN` edges connect them.
///
/// Fire-and-forget on a spawned task — if Neo4j is down or slow the
/// save has already won and we only log at debug. The text search is
/// enough for retrieval without the graph layer. (No runtime → the
/// write is skipped entirely; direct sync callers of the store don't
/// get graph edges, same as Python callers that bypassed `save`.)
fn record_entities_best_effort(record: &WorkspaceMemory) {
    if record.entities.is_empty() {
        return;
    }
    let chunk = json!({
        "id": record.id,
        "path": format!("workspace:{}:memory", record.workspace_id),
        "symbol": "memory",
        "language": "memory",
        "workspace": record.workspace_id,
        "entities": record.entities,
    });
    let record_id = record.id.clone();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("workspace_memory: graph write skipped for {record_id} (no async runtime)");
        return;
    };
    handle.spawn(async move {
        let reply = BoltClient::new().upsert(vec![chunk]).await;
        if !reply.ok {
            tracing::debug!(
                "workspace_memory: graph write skipped for {} (memgraph unreachable: {})",
                record_id,
                reply
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "unknown".to_owned())
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;

    // ── validation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn every_verb_requires_workspace_id() {
        let _env = TestEnv::new();
        for reply in [
            handle_list(json!({})).await,
            handle_search(json!({"query": "x"})).await,
            handle_save(json!({"body": "x"})).await,
            handle_update(json!({"id": "x"})).await,
            handle_delete(json!({"id": "x"})).await,
            handle_curate(json!({})).await,
        ] {
            assert!(!reply.ok);
            let err = reply.error.unwrap();
            assert_eq!(err.code, "bad_request");
            assert_eq!(err.message, "workspace_id is required");
        }
    }

    #[tokio::test]
    async fn search_requires_non_blank_query() {
        let _env = TestEnv::new();
        for payload in [
            json!({"workspace_id": "ws1"}),
            json!({"workspace_id": "ws1", "query": ""}),
            json!({"workspace_id": "ws1", "query": "   "}),
        ] {
            let reply = handle_search(payload).await;
            assert!(!reply.ok);
            let err = reply.error.unwrap();
            assert_eq!(err.code, "bad_request");
            assert_eq!(err.message, "query is required");
        }
    }

    #[tokio::test]
    async fn save_requires_non_blank_body() {
        let _env = TestEnv::new();
        for payload in [
            json!({"workspace_id": "ws1"}),
            json!({"workspace_id": "ws1", "body": "   "}),
        ] {
            let reply = handle_save(payload).await;
            assert!(!reply.ok);
            let err = reply.error.unwrap();
            assert_eq!(err.code, "bad_request");
            assert_eq!(err.message, "body is required");
        }
    }

    #[tokio::test]
    async fn update_and_delete_require_id() {
        let _env = TestEnv::new();
        for reply in [
            handle_update(json!({"workspace_id": "ws1"})).await,
            handle_delete(json!({"workspace_id": "ws1"})).await,
        ] {
            assert!(!reply.ok);
            let err = reply.error.unwrap();
            assert_eq!(err.code, "bad_request");
            assert_eq!(err.message, "id is required");
        }
    }

    // ── wire shapes ──────────────────────────────────────────────────

    #[tokio::test]
    async fn list_empty_store_returns_zero_count_and_echoes_workspace() {
        let _env = TestEnv::new();
        let reply = handle_list(json!({"workspace_id": "ws_empty"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["count"], 0);
        assert_eq!(reply.data["workspace_id"], "ws_empty");
        assert!(reply.data["memories"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_returns_full_record_object() {
        let _env = TestEnv::new();
        let reply = handle_save(json!({
            "workspace_id": "ws1",
            "body": "the harness owns the pipe surface",
            "source": "chat",
            "importance": 7,
        }))
        .await;
        assert!(reply.ok);
        let id = reply.data["id"].as_str().unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(reply.data["workspace_id"], "ws1");
        assert_eq!(reply.data["body"], "the harness owns the pipe surface");
        assert_eq!(reply.data["source"], "chat");
        assert_eq!(reply.data["importance"], 7);
        assert!(reply.data["created_at"].as_f64().unwrap() > 0.0);
        assert_eq!(reply.data["superseded_by"], "");
        assert!(reply.data["entities"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_with_entities_echoes_them_and_survives_no_graph() {
        // Neo4j isn't running in tests — the graph write must be
        // fire-and-forget and the save must still succeed.
        let _env = TestEnv::new();
        let reply = handle_save(json!({
            "workspace_id": "ws1",
            "body": "watcher polls outputs",
            "entities": ["watcher", "outputs"],
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["entities"], json!(["watcher", "outputs"]));
    }

    #[tokio::test]
    async fn save_heuristic_importance_when_missing() {
        let _env = TestEnv::new();
        let reply = handle_save(json!({"workspace_id": "ws1", "body": "tiny"})).await;
        assert!(reply.ok);
        // length_pts=0, entity_pts=0 → 3.
        assert_eq!(reply.data["importance"], 3);
    }

    #[tokio::test]
    async fn list_then_save_round_trip_with_superseded_flag() {
        let _env = TestEnv::new();
        let saved = handle_save(json!({"workspace_id": "ws1", "body": "v1"})).await;
        let id = saved.data["id"].as_str().unwrap().to_owned();
        let updated = handle_update(json!({
            "workspace_id": "ws1",
            "id": id,
            "body": "v2",
        }))
        .await;
        assert!(updated.ok);

        let visible = handle_list(json!({"workspace_id": "ws1"})).await;
        assert_eq!(visible.data["count"], 1);
        assert_eq!(visible.data["memories"][0]["body"], "v2");

        let all = handle_list(json!({
            "workspace_id": "ws1",
            "include_superseded": true,
        }))
        .await;
        assert_eq!(all.data["count"], 2);
    }

    #[tokio::test]
    async fn update_unknown_id_uses_python_not_found_message() {
        let _env = TestEnv::new();
        let reply = handle_update(json!({
            "workspace_id": "ws1",
            "id": "deadbeefdeadbeef",
            "body": "x",
        }))
        .await;
        assert!(!reply.ok);
        let err = reply.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert_eq!(err.message, "memory 'deadbeefdeadbeef' not in 'ws1'");
    }

    #[tokio::test]
    async fn delete_returns_ok_envelope_for_known_and_unknown_ids() {
        let _env = TestEnv::new();
        let saved = handle_save(json!({"workspace_id": "ws1", "body": "to delete"})).await;
        let id = saved.data["id"].as_str().unwrap().to_owned();

        let deleted = handle_delete(json!({"workspace_id": "ws1", "id": id})).await;
        assert!(deleted.ok);
        assert_eq!(deleted.data["ok"], true);
        assert_eq!(deleted.data["workspace_id"], "ws1");
        assert_eq!(deleted.data["id"], id);

        let again = handle_delete(json!({"workspace_id": "ws1", "id": id})).await;
        assert!(again.ok);
        assert_eq!(again.data["ok"], false);
    }

    #[tokio::test]
    async fn search_returns_hits_with_python_candidate_shape() {
        let _env = TestEnv::new();
        handle_save(json!({
            "workspace_id": "ws1",
            "body": "gateway wire shapes",
            "importance": 8,
        }))
        .await;
        handle_save(json!({"workspace_id": "ws1", "body": "unrelated note"})).await;

        let reply = handle_search(json!({"workspace_id": "ws1", "query": "gateway"})).await;
        assert!(reply.ok);
        let hits = reply.data["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit["body"], "gateway wire shapes");
        assert_eq!(hit["workspace_id"], "ws1");
        assert_eq!(hit["importance"], 8);
        assert!(hit["similarity"].as_f64().unwrap() > 0.0);
        assert!(hit["score"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn search_limit_accepts_k_or_limit_and_clamps() {
        let _env = TestEnv::new();
        for i in 0..4 {
            handle_save(json!({"workspace_id": "ws1", "body": format!("token row{i}")})).await;
        }
        // k wins.
        let r = handle_search(json!({"workspace_id": "ws1", "query": "token", "k": 2})).await;
        assert_eq!(r.data["hits"].as_array().unwrap().len(), 2);
        // limit used when k absent.
        let r = handle_search(json!({"workspace_id": "ws1", "query": "token", "limit": 3})).await;
        assert_eq!(r.data["hits"].as_array().unwrap().len(), 3);
        // k=0 is falsy → falls through to limit (Python `or` chain).
        let r = handle_search(json!({
            "workspace_id": "ws1", "query": "token", "k": 0, "limit": 1,
        }))
        .await;
        assert_eq!(r.data["hits"].as_array().unwrap().len(), 1);
        // Default 5 (only 4 rows exist).
        let r = handle_search(json!({"workspace_id": "ws1", "query": "token"})).await;
        assert_eq!(r.data["hits"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn search_limit_clamps_to_one_through_fifty() {
        assert_eq!(search_limit(&json!({"k": -3})), 1);
        assert_eq!(search_limit(&json!({"k": 999})), 50);
        assert_eq!(search_limit(&json!({})), 5);
        assert_eq!(search_limit(&json!({"limit": 7})), 7);
        assert_eq!(search_limit(&json!({"k": "weird"})), 5);
    }

    #[tokio::test]
    async fn curate_returns_python_skipped_shape() {
        let _env = TestEnv::new();
        let reply = handle_curate(json!({"workspace_id": "ws1"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["workspace_id"], "ws1");
        assert_eq!(reply.data["inputs_considered"], 0);
        assert_eq!(reply.data["skipped"], true);
        assert_eq!(reply.data["skip_reason"], "no chat_fn supplied");
        assert!(reply.data["kept"].as_array().unwrap().is_empty());
        assert!(reply.data["superseded"].as_array().unwrap().is_empty());
        assert!(reply.data["merged"].as_array().unwrap().is_empty());
    }
}
