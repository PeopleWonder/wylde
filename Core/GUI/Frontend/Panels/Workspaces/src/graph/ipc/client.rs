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

/// Why a graph load failed — distinguishes three states the view renders
/// differently (OI-1 / F2): the service is *down*, the service is *up but its
/// binary is too old to know the verb*, and a plain logical error.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphFetchError {
    /// The pipe is unreachable (down / not launched / slow / breaker open).
    /// → "service unavailable, Start it + Retry" fallback; keep last-known data.
    ServiceUnavailable(String),
    /// The service answered but doesn't know the verb (`no_action`) — its binary
    /// predates the feature. → "service out of date, Update/Restart + Retry".
    /// Telling the user to *start* an already-running service was the F2 bug.
    OutOfDate(String),
    /// The workspaces service is up, but the **graph database** (Memgraph, the
    /// Bolt `:7687` backend) is down — a `bolt_*`/connect error from the graph
    /// query. Distinct from the service being down: the fix is "Start graph
    /// database", not "Start the workspaces service" (decision 7 / design §7.1).
    GraphDbDown(String),
    /// A logical error (bad request, backend/Neo4j error). Shown verbatim.
    Logical(String),
}

impl GraphFetchError {
    pub fn message(&self) -> &str {
        match self {
            GraphFetchError::ServiceUnavailable(m)
            | GraphFetchError::OutOfDate(m)
            | GraphFetchError::GraphDbDown(m)
            | GraphFetchError::Logical(m) => m,
        }
    }

    pub fn is_service_unavailable(&self) -> bool {
        matches!(self, GraphFetchError::ServiceUnavailable(_))
    }

    /// The service is reachable but its build lacks the verb (stale binary).
    pub fn is_out_of_date(&self) -> bool {
        matches!(self, GraphFetchError::OutOfDate(_))
    }

    /// The graph database (Bolt/Memgraph) is down — offers "Start graph database".
    pub fn is_graph_db_down(&self) -> bool {
        matches!(self, GraphFetchError::GraphDbDown(_))
    }

    /// Any state with a one-click recovery affordance: service down (Start),
    /// out of date (Restart), or graph-db down (Start graph database). All
    /// three also offer click-to-retry once the underlying fix lands.
    pub fn is_recoverable(&self) -> bool {
        self.is_service_unavailable() || self.is_out_of_date() || self.is_graph_db_down()
    }
}

/// Classify a raw `wylde_gui_pipe::call` error string. A `pipe_*` transport
/// failure (or "not running") means the service is genuinely *down*; a
/// `no_action` means the service is *up* but its binary predates the verb — a
/// distinct "out of date" state (F2). Everything else is a logical error.
pub fn classify(err: String) -> GraphFetchError {
    let down = err.contains("pipe_unavailable")
        || err.contains("pipe_connect")
        || err.contains("pipe_timeout")
        || err.contains("pipe_io")
        || err.contains("not running");
    // The graph DB being down surfaces as a bolt connect error from the graph
    // query (graph/bolt.rs) — a logical-looking error, but with a *different*
    // one-click fix than the workspaces service being down.
    let graph_db_down = err.contains("bolt_")
        || err.contains("bolt connect")
        || err.contains("memgraph")
        || err.contains(":7687");
    if down {
        GraphFetchError::ServiceUnavailable(err)
    } else if err.contains("no_action") {
        GraphFetchError::OutOfDate(err)
    } else if graph_db_down {
        GraphFetchError::GraphDbDown(err)
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
        ] {
            let c = classify(e.to_owned());
            assert!(c.is_service_unavailable(), "{e} should be 'down'");
            assert!(!c.is_out_of_date(), "{e} is not out-of-date");
            assert!(c.is_recoverable());
        }
    }

    #[test]
    fn classifies_no_action_as_out_of_date_not_down() {
        // F2: a running service that lacks the verb is OUT OF DATE, not down.
        // Telling the user to "start" a running service was the bug.
        let c = classify("no_action: unknown action workspaces.graph".to_owned());
        assert!(c.is_out_of_date(), "no_action should be out-of-date");
        assert!(!c.is_service_unavailable(), "no_action is not 'down'");
        assert!(c.is_recoverable());
    }

    #[test]
    fn classifies_logical_errors_verbatim() {
        let e = classify("bad_request: workspace_id is required".to_owned());
        assert!(!e.is_recoverable());
        assert_eq!(e.message(), "bad_request: workspace_id is required");
    }

    #[test]
    fn classifies_bolt_errors_as_graph_db_down() {
        // The graph DB (Bolt/Memgraph) being down is its own recoverable state
        // with a distinct fix — "Start graph database" — not a plain logical
        // error and not the workspaces service being down (decision 7).
        for e in [
            "bolt_connect: connection refused 127.0.0.1:7687",
            "bolt_unavailable: neo4j down",
            "memgraph not reachable",
        ] {
            let c = classify(e.to_owned());
            assert!(c.is_graph_db_down(), "{e} should be graph-db-down");
            assert!(c.is_recoverable(), "{e} offers Start graph database");
            assert!(!c.is_service_unavailable(), "{e} is not the workspaces pipe being down");
        }
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
