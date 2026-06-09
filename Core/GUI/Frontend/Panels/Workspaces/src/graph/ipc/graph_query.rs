//! `workspaces.graph` (Slice B) over the wire — returns a deserialised
//! [`WorkspaceGraph`].
//!
//! Per the Slice 0d precedent, the GUI talks to the `wylde-workspaces` service
//! through `wylde_gui_pipe::call` (the established gpui transport) rather than
//! the async `wylde-workspaces-client` crate directly — that client crate
//! guards the harness hot path and assumes a tokio context the gpui dispatch
//! threads don't have. `client.rs` layers the graceful-degrade classification
//! (OI-1 / Plan v2 §7.3) on top of these raw calls.

use serde_json::json;

use crate::graph::model::WorkspaceGraph;

/// The service the graph verbs live on (Slice 0d moved `workspaces.*` off the
/// harness pipe onto the dedicated service).
const SERVICE: &str = "wylde-workspaces";

/// Call `workspaces.graph(workspace_id)` and deserialise the reply into a
/// [`WorkspaceGraph`]. `Err(String)` is the raw pipe-error string (a
/// `pipe_*` transport error, or a `code: message` application error) — the
/// caller in `client.rs` classifies it into degrade vs. logical.
pub async fn fetch_graph(workspace_id: &str) -> Result<WorkspaceGraph, String> {
    let data = wylde_gui_pipe::call(
        SERVICE,
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.graph",
            "payload": { "workspace_id": workspace_id },
        })),
    )
    .await?;
    WorkspaceGraph::from_value(data).map_err(|e| format!("decode graph reply: {e}"))
}

/// The active workspace id, read as the head of `workspaces.list_mru`
/// (`set_active` bumps the active workspace to the MRU head — the same source
/// the registry tab and InferenceBar use). `Ok(None)` when no workspaces
/// exist yet.
pub async fn active_workspace_id() -> Result<Option<String>, String> {
    let data = wylde_gui_pipe::call(
        SERVICE,
        "POST",
        "/__action__",
        Some(json!({ "action": "workspaces.list_mru", "payload": {} })),
    )
    .await?;
    Ok(data
        .get("workspaces")
        .and_then(|w| w.as_array())
        .and_then(|arr| arr.first())
        .and_then(|w| w.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_compile_and_target_the_service() {
        // Build-time witness — same pattern the registry ipc tests use. The
        // verb strings and service name are exercised here without a live pipe.
        let _ = fetch_graph;
        let _ = active_workspace_id;
        assert_eq!(SERVICE, "wylde-workspaces");
    }
}
