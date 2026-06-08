//! Inbound IPC for `wylde-workspaces`.
//!
//! Thin wrapper over the shared named-pipe server in
//! [`wylde_shared::ipc`]. The wire format is the stack-wide
//! `[u32 BE length][rmp-serde body]` framing with the v1 handshake — we do
//! NOT reinvent it. `serve()` binds `\\.\pipe\wylde-<service>` (the name
//! comes from [`crate::config`]), performs the handshake, and dispatches
//! `/__action__` frames into the registry populated by
//! [`crate::action_dispatch::install`].
//!
//! The shared server already answers the built-in control methods
//! (`/__ping__`, `/__handshake__`, `/health`); our domain `ping` verb is a
//! registered action on top of that, reached via the action envelope.

use wylde_shared::ipc;

/// The pipe path this service binds for a given service name, e.g.
/// `\\.\pipe\wylde-workspaces`. Delegates to the shared name helper so the
/// `wylde-` prefix convention stays in one place.
pub fn pipe_path(service_name: &str) -> String {
    ipc::pipe_name(service_name)
}

/// Bind the pipe and run the accept loop until the future is dropped
/// (cancelled by the caller's shutdown `select!`) or an irrecoverable bind
/// error occurs.
///
/// `service_name` is normally [`crate::config::Config::service_name`]; tests
/// pass an isolated name so they never touch a running prod pipe.
pub async fn serve(service_name: &str) -> anyhow::Result<()> {
    ipc::serve(service_name, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_path_uses_wylde_prefix() {
        assert_eq!(pipe_path("wylde-workspaces"), r"\\.\pipe\wylde-workspaces");
        // Bare name is normalised to the same path.
        assert_eq!(pipe_path("workspaces"), r"\\.\pipe\wylde-workspaces");
    }
}
