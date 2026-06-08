//! Named-pipe transport: connect / framing / msgpack.
//!
//! The wire mechanics are owned by [`wylde_shared::ipc`] (the stack-wide
//! `[u32 BE length][rmp-serde body]` framing + v1 handshake). This module is
//! the thin seam that lets the client issue a single action call with a
//! caller-chosen per-attempt timeout — `wylde_shared::ipc::send_action`
//! always uses the env default, but the workspaces client needs the
//! per-verb budget from [`crate::timeouts`], so it builds the `/__action__`
//! envelope and calls [`wylde_shared::ipc::send`] with the explicit timeout.

use std::time::Duration;

use serde_json::Value;
use wylde_shared::ipc::{self, Reply, ACTION_DISPATCH_PATH};

/// Derive the bare service name the shared transport expects from a pipe
/// path. The shared layer re-applies the `wylde-` prefix, so passing either
/// `\\.\pipe\wylde-workspaces`, `wylde-workspaces`, or `workspaces` all
/// resolve to the same pipe.
pub fn service_name_from_pipe_path(pipe_path: &std::path::Path) -> String {
    let raw = pipe_path.to_string_lossy();
    // Take the final `\`- or `/`-separated component.
    raw.rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or(&raw)
        .to_string()
}

/// Fire one action at `service` with an explicit per-attempt `timeout` and
/// return the raw [`Reply`]. Never panics — transport failures come back as
/// `ok=false` with a structured error code (the shared client guarantees
/// this).
pub async fn call_action(service: &str, action: &str, payload: Value, timeout: Duration) -> Reply {
    let body = serde_json::json!({
        "action": action,
        "payload": payload,
    });
    ipc::send(service, ACTION_DISPATCH_PATH, body, timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extracts_service_from_full_pipe_path() {
        let p = PathBuf::from(r"\\.\pipe\wylde-workspaces");
        assert_eq!(service_name_from_pipe_path(&p), "wylde-workspaces");
    }

    #[test]
    fn passes_through_bare_name() {
        let p = PathBuf::from("wylde-workspaces");
        assert_eq!(service_name_from_pipe_path(&p), "wylde-workspaces");
    }
}
