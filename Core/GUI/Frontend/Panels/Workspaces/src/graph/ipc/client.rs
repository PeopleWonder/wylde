//! Connection + graceful-degrade for the graph IPC (OI-1 / Plan v2 §7.3).
//!
//! When the `wylde-workspaces` service is down, unlaunched, or its breaker is
//! open, the panel must show a "Workspaces service unavailable" message with
//! a Retry button — never crash, never render an empty graph silently. This
//! module classifies a raw pipe-error string into [`GraphFetchError`] so the
//! view can pick the right fallback, and wraps the two-step "find active
//! workspace → fetch its graph" flow into one [`fetch_active_graph`] call.

use crate::graph::model::WorkspaceGraph;

use super::graph_query;

/// The outcome of loading the active workspace's graph.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphLoad {
    /// The active workspace id, or `None` when no workspace exists yet (the
    /// panel shows an "add a workspace" hint, not a degrade banner).
    pub workspace_id: Option<String>,
    pub graph: WorkspaceGraph,
}

/// Why a graph load failed — distinguishes the service-down degrade path
/// (OI-1) from a logical/application error so the view renders the right thing.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphFetchError {
    /// The service is unreachable (down / not launched / slow / breaker open).
    /// → "service unavailable + Retry" fallback; keep last-known data.
    ServiceUnavailable(String),
    /// A logical error (bad request, backend/Neo4j error). Shown verbatim.
    Logical(String),
}

impl GraphFetchError {
    pub fn message(&self) -> &str {
        match self {
            GraphFetchError::ServiceUnavailable(m) | GraphFetchError::Logical(m) => m,
        }
    }

    pub fn is_service_unavailable(&self) -> bool {
        matches!(self, GraphFetchError::ServiceUnavailable(_))
    }
}

/// Classify a raw `wylde_gui_pipe::call` error string. Mirrors the registry
/// tab's `is_service_unavailable` heuristic: a `pipe_*` transport failure or a
/// `no_action` (service reachable but verb unknown — e.g. running an old build)
/// is a service-availability problem; everything else is logical.
pub fn classify(err: String) -> GraphFetchError {
    let unavailable = err.contains("pipe_unavailable")
        || err.contains("pipe_connect")
        || err.contains("pipe_timeout")
        || err.contains("pipe_io")
        || err.contains("not running")
        || err.contains("no_action");
    if unavailable {
        GraphFetchError::ServiceUnavailable(err)
    } else {
        GraphFetchError::Logical(err)
    }
}

/// Load the active workspace's graph: resolve the active workspace (MRU head),
/// then fetch its graph. Either step's transport failure degrades to
/// [`GraphFetchError::ServiceUnavailable`].
pub async fn fetch_active_graph() -> Result<GraphLoad, GraphFetchError> {
    let active = graph_query::active_workspace_id().await.map_err(classify)?;
    let Some(id) = active else {
        // No workspace yet — not an error; an empty, id-less load.
        return Ok(GraphLoad {
            workspace_id: None,
            graph: WorkspaceGraph::default(),
        });
    };
    let graph = graph_query::fetch_graph(&id).await.map_err(classify)?;
    Ok(GraphLoad {
        workspace_id: Some(id),
        graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_transport_errors_as_unavailable() {
        for e in [
            "pipe_unavailable: service 'wylde-workspaces' is not running (pipe not found)",
            "pipe_connect: wylde-workspaces: oops",
            "pipe_timeout: no response from 'wylde-workspaces' within 30s",
            "no_action: workspaces.graph",
        ] {
            assert!(
                classify(e.to_owned()).is_service_unavailable(),
                "{e} should degrade"
            );
        }
    }

    #[test]
    fn classifies_logical_errors_verbatim() {
        let e = classify("bad_request: workspace_id is required".to_owned());
        assert!(!e.is_service_unavailable());
        assert_eq!(e.message(), "bad_request: workspace_id is required");
        // A backend/Neo4j error is logical, not a degrade.
        assert!(!classify("bolt_unavailable: neo4j down".to_owned()).is_service_unavailable());
    }

    #[test]
    fn graph_load_distinguishes_no_workspace() {
        let load = GraphLoad {
            workspace_id: None,
            graph: WorkspaceGraph::default(),
        };
        assert!(load.workspace_id.is_none());
        assert!(load.graph.nodes.is_empty());
    }
}
