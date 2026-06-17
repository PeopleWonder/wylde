//! The `workspaces.concepts.*` verb handlers (TBS concept-system Phase 0).
//!
//! Read/write/curate the per-workspace concept [`store`]:
//!
//!   * `list` / `get` / `update` / `delete` — CRUD over `concepts.json`.
//!   * `build` — the Phase-0 cheap-concept pass: read the workspace code graph
//!     ([`crate::graph::api::graph`]), label its directory clusters
//!     ([`crate::concepts::cheap`]), and replace the concept set. Idempotent.
//!   * `reverse_lookup` — from a symbol/file → the concepts (and vocabulary
//!     anchors) it belongs to (thesis §4.2). A pure store query; no Neo4j.
//!
//! Every concept-bearing reply uses the [`Concept`] serde shape directly.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::cheap;
use super::concept::Concept;
use super::store::{self, ConceptPatch, CreateOutcome, UpdateOutcome};
use crate::anchors::store as anchor_store;

fn require_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn opt_str_array(payload: &Value, key: &str) -> Option<Vec<String>> {
    payload.get(key).and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn concepts_reply(workspace_id: &str, extra: &[(&str, Value)], concepts: &[Concept]) -> Reply {
    let mut obj = json!({
        "workspace_id": workspace_id,
        "count": concepts.len(),
        "concepts": concepts,
    });
    for (k, v) in extra {
        obj[*k] = v.clone();
    }
    Reply::ok(obj)
}

/// `workspaces.concepts.list` — every concept for a workspace.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    concepts_reply(&ws, &[], &store::load(&ws))
}

/// `workspaces.concepts.get` — one concept by id (with members + files).
pub async fn handle_get(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::get(&ws, &id) {
        Some(c) => Reply::ok(json!(c)),
        None => Reply::err_msg("not_found", format!("no concept {id:?} in this workspace")),
    }
}

/// `workspaces.concepts.build` — the Phase-0 cheap-concept pass. Reads the
/// workspace's live code graph, labels its directory clusters into stand-in
/// concepts, and replaces `concepts.json` with the result. Idempotent: a
/// re-run reproduces the same set (modulo timestamps). Returns
/// `{workspace_id, built, source: "directory_cluster"}`.
///
/// Surfaces the `bolt_*` code unchanged when the graph backend is unreachable
/// — building cheap concepts requires the directory clusters the graph verb
/// produces, so a down Neo4j fails the build (the same dependency the graph
/// tab has). The browse surface degrades to the last-built set on disk.
pub async fn handle_build(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let graph = match crate::graph::api::graph(&ws).await {
        Ok(g) => g,
        Err(e) => return e.to_reply(),
    };
    let concepts = cheap::build_concepts(&graph);

    // Build the graph-projection rows before the store swap moves `concepts`.
    let rows: Vec<Value> = concepts
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "description": c.description,
                "source": c.source.as_str(),
                "members": c.members,
                "parents": c.parent_concepts,
            })
        })
        .collect();

    let built = match store::replace_all(&ws, concepts) {
        Ok(n) => n,
        Err(e) => return Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    };

    // Additively project into the graph so the panel can render concept nodes.
    // Best-effort: the JSON store is authoritative, so a projection failure
    // (Neo4j hiccup) is logged, never fatal to the build.
    let projected = match crate::graph::BoltClient::new()
        .project_concepts(&ws, rows)
        .await
    {
        reply if reply.ok => reply.data.get("projected").cloned().unwrap_or(json!(0)),
        reply => {
            tracing::warn!(
                workspace = %ws,
                error = ?reply.error,
                "concept graph projection failed (non-fatal; JSON store is authoritative)"
            );
            json!(0)
        }
    };

    Reply::ok(json!({
        "workspace_id": ws,
        "built": built,
        "projected": projected,
        "source": "directory_cluster",
    }))
}

/// `workspaces.concepts.update` — patch a concept's
/// label/description/members/parents/described_by. `not_found` for an unknown id.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let patch = ConceptPatch {
        label: require_str(&payload, "label"),
        description: payload
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        members: opt_str_array(&payload, "members"),
        member_files: opt_str_array(&payload, "member_files"),
        parent_concepts: opt_str_array(&payload, "parent_concepts"),
        described_by: opt_str_array(&payload, "described_by"),
    };
    match store::update(&ws, &id, patch) {
        Ok(UpdateOutcome::Updated(c)) => Reply::ok(json!(c)),
        Ok(UpdateOutcome::NotFound) => {
            Reply::err_msg("not_found", format!("no concept {id:?} in this workspace"))
        }
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.create` — hand-author one concept (curation).
/// `already_exists` on a duplicate id.
pub async fn handle_create(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let label = require_str(&payload, "label").unwrap_or_else(|| id.clone());
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut concept = Concept::new(id, label, description, super::concept::ConceptSource::Manual);
    if let Some(m) = opt_str_array(&payload, "members") {
        concept.members = m;
    }
    if let Some(f) = opt_str_array(&payload, "member_files") {
        concept.member_files = f;
    }
    if let Some(p) = opt_str_array(&payload, "parent_concepts") {
        concept.parent_concepts = p;
    }
    match store::create(&ws, concept) {
        Ok(CreateOutcome::Created(c)) => Reply::ok(json!(c)),
        Ok(CreateOutcome::AlreadyExists(c)) => Reply::err_msg(
            "already_exists",
            format!("concept {:?} already exists in this workspace", c.id),
        ),
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.delete` — remove a concept by id.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::delete(&ws, &id) {
        Ok(removed) => Reply::ok(json!({ "ok": true, "removed": removed, "id": id })),
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.list_under` — concepts whose parent set contains
/// `parent_id` (DAG child traversal).
pub async fn handle_list_under(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(parent_id) = require_str(&payload, "parent_id") else {
        return Reply::err_msg("bad_request", "parent_id is required");
    };
    let kids = store::list_under(&ws, &parent_id);
    concepts_reply(&ws, &[("parent_id", json!(parent_id))], &kids)
}

/// `workspaces.concepts.reverse_lookup` — from a `symbol_id` (and/or `file`) to
/// the concepts and vocabulary it belongs to (thesis §4.2). Pure store query;
/// no Neo4j. Reply: `{workspace_id, symbol_id?, file?, concepts, vocabulary}`
/// where `vocabulary` is the anchors targeting that symbol (the curated terms).
pub async fn handle_reverse_lookup(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let symbol_id = require_str(&payload, "symbol_id");
    let file = require_str(&payload, "file");
    if symbol_id.is_none() && file.is_none() {
        return Reply::err_msg("bad_request", "one of symbol_id or file is required");
    }

    // Concepts: union of member-match (by symbol) and file-match (by file).
    let mut concepts: Vec<Concept> = Vec::new();
    if let Some(sym) = &symbol_id {
        concepts.extend(store::find_by_member(&ws, sym));
    }
    if let Some(f) = &file {
        for c in store::find_by_file(&ws, f) {
            if !concepts.iter().any(|e| e.id == c.id) {
                concepts.push(c);
            }
        }
    }
    concepts.sort_by(|a, b| a.id.cmp(&b.id));

    // Vocabulary: the anchors targeting that symbol (curated terms naming it).
    let vocabulary: Vec<Value> = symbol_id
        .as_deref()
        .map(|s| {
            anchor_store::find_by_target(&ws, s)
                .iter()
                .map(wylde_shared::anchor::Anchor::to_value)
                .collect()
        })
        .unwrap_or_default();

    Reply::ok(json!({
        "workspace_id": ws,
        "symbol_id": symbol_id,
        "file": file,
        "concepts": concepts,
        "vocabulary": vocabulary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;
    use crate::test_support::TestEnv;

    fn seed(ws: &str) {
        let mut a = Concept::new("dir:src/graph", "Graph", "the graph", ConceptSource::DirectoryCluster);
        a.members = vec!["alpha".into(), "shared".into()];
        a.member_files = vec!["src/graph/api.rs".into()];
        let mut b = Concept::new("dir:src/rag", "Rag", "retrieval", ConceptSource::DirectoryCluster);
        b.members = vec!["shared".into()];
        b.member_files = vec!["src/rag/search.rs".into()];
        b.parent_concepts = vec!["dir:src/graph".into()];
        store::replace_all(ws, vec![a, b]).unwrap();
    }

    #[tokio::test]
    async fn list_requires_workspace_id() {
        let r = handle_list(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn list_and_get_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-api-con-0000";
        seed(ws);
        let list = handle_list(json!({ "workspace_id": ws })).await;
        assert!(list.ok);
        assert_eq!(list.data["count"], 2);

        let got = handle_get(json!({ "workspace_id": ws, "id": "dir:src/rag" })).await;
        assert!(got.ok);
        assert_eq!(got.data["label"], "Rag");

        let miss = handle_get(json!({ "workspace_id": ws, "id": "nope" })).await;
        assert!(!miss.ok);
        assert_eq!(miss.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn update_and_delete() {
        let _env = TestEnv::new();
        let ws = "ws-api-upd-0000";
        seed(ws);
        let upd = handle_update(json!({
            "workspace_id": ws, "id": "dir:src/graph",
            "label": "Graph Layer", "described_by": ["graph_term"]
        }))
        .await;
        assert!(upd.ok);
        assert_eq!(upd.data["label"], "Graph Layer");
        assert_eq!(upd.data["described_by"][0], "graph_term");

        let del = handle_delete(json!({ "workspace_id": ws, "id": "dir:src/rag" })).await;
        assert!(del.ok);
        assert_eq!(del.data["removed"], true);
        assert_eq!(store::load(ws).len(), 1);
    }

    #[tokio::test]
    async fn list_under_returns_children() {
        let _env = TestEnv::new();
        let ws = "ws-api-under-000";
        seed(ws);
        let r = handle_list_under(json!({ "workspace_id": ws, "parent_id": "dir:src/graph" })).await;
        assert!(r.ok);
        assert_eq!(r.data["count"], 1);
        assert_eq!(r.data["concepts"][0]["id"], "dir:src/rag");
    }

    #[tokio::test]
    async fn reverse_lookup_unions_member_and_file() {
        let _env = TestEnv::new();
        let ws = "ws-api-rev-0000";
        seed(ws);
        // "shared" is a member of both concepts.
        let by_sym = handle_reverse_lookup(json!({ "workspace_id": ws, "symbol_id": "shared" })).await;
        assert!(by_sym.ok);
        assert_eq!(by_sym.data["concepts"].as_array().unwrap().len(), 2);

        // file match narrows to one.
        let by_file =
            handle_reverse_lookup(json!({ "workspace_id": ws, "file": "src/rag/search.rs" })).await;
        assert!(by_file.ok);
        assert_eq!(by_file.data["concepts"].as_array().unwrap().len(), 1);
        assert_eq!(by_file.data["concepts"][0]["id"], "dir:src/rag");
    }

    #[tokio::test]
    async fn reverse_lookup_requires_a_key() {
        let _env = TestEnv::new();
        let ws = "ws-api-rev2-000";
        let r = handle_reverse_lookup(json!({ "workspace_id": ws })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let _env = TestEnv::new();
        let ws = "ws-api-cr-0000";
        let mk = || json!({ "workspace_id": ws, "id": "manual:x", "label": "X" });
        assert!(handle_create(mk()).await.ok);
        let dup = handle_create(mk()).await;
        assert!(!dup.ok);
        assert_eq!(dup.error.unwrap().code, "already_exists");
    }
}
