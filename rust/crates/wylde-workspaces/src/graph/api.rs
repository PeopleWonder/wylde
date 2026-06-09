//! `workspaces.graph` — the Slice B read verb.
//!
//! Returns the active workspace's code graph (`{nodes, edges, clusters}`)
//! read live from Neo4j. The read path is:
//!
//!   payload → [`graph`] → [`BoltClient::fetch_workspace_graph`] (Cypher in
//!   [`super::query`]) → [`super::projection::project`] → [`WorkspaceGraph`].
//!
//! It is the foundation for Phase 3 (visual rendering) and Phase 4 (composer
//! integration). Read-only — it never mutates the graph; the write surface
//! ([`super::bolt`] upsert/relate) is untouched.

use serde_json::Value;
use wylde_shared::ipc::Reply;

use crate::error::{Result, WorkspacesError};
use crate::graph::projection::{self, WorkspaceGraph};
use crate::graph::BoltClient;

/// Read `workspace_id`'s full code graph from Neo4j.
///
/// `Ok(WorkspaceGraph)` even when the workspace has no graph data yet (an
/// empty graph). `Err` only on a bad request (blank id) or a graph-backend
/// failure (Neo4j unreachable / query error) — the latter preserves the
/// underlying `bolt_*` wire code so the client classifier sees it unchanged.
pub async fn graph(workspace_id: &str) -> Result<WorkspaceGraph> {
    let ws = workspace_id.trim();
    if ws.is_empty() {
        return Err(WorkspacesError::BadRequest("workspace_id is required".into()));
    }
    let rows = BoltClient::new()
        .fetch_workspace_graph(ws)
        .await
        .map_err(WorkspacesError::backend)?;
    Ok(projection::project(rows))
}

/// `workspaces.graph` action handler. Payload: `{ "workspace_id": string }`.
/// Reply data is the [`WorkspaceGraph`] (`{nodes, edges, clusters}`).
pub async fn handle_graph(payload: Value) -> Reply {
    let id = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(id) = id else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    match graph(id).await {
        Ok(g) => match serde_json::to_value(&g) {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err_msg("serde", format!("serialize graph: {e}")),
        },
        Err(e) => e.to_reply(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn handle_graph_requires_workspace_id() {
        let r = handle_graph(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn handle_graph_rejects_blank_workspace_id() {
        let r = handle_graph(json!({ "workspace_id": "   " })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn graph_fn_rejects_blank_id_without_touching_neo4j() {
        let err = graph("").await.unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }
}
