//! Service-wide error type for `wylde-workspaces`.
//!
//! Every fallible operation inside the service funnels through
//! [`WorkspacesError`]. It carries a stable `code()` string that maps onto
//! the shared IPC wire shape (`{code, message, details?}`) so a failing
//! action handler converts to a clean [`wylde_shared::ipc::Reply`] without
//! the caller having to know the internals.
//!
//! Slice 0a only needs a handful of variants; later slices (registry,
//! notes, graph, ingest) add their own. The `Other` catch-all keeps the
//! enum from churning while submodules are still being built out.

use wylde_shared::ipc::{IpcError, Reply};

/// The service-wide error type. New variants are added per slice as the
/// submodules land; each maps to a stable wire `code`.
#[derive(Debug, thiserror::Error)]
pub enum WorkspacesError {
    /// A requested action/verb has no registered handler. Mirrors the shared
    /// dispatcher's `no_action` code so callers see a uniform shape whether
    /// the rejection comes from the registry or from a submodule router.
    #[error("unknown action: {0}")]
    UnknownAction(String),

    /// Malformed request payload (missing/!typed fields).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// I/O failure touching the data dir or a backing store.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// (de)serialization failure on a stored or wire value.
    #[error("serde error: {0}")]
    Serde(String),

    /// Anything not yet given a first-class variant. Keeps the enum stable
    /// while submodules are scaffolded; replaced with specific variants as
    /// each slice lands.
    #[error("{0}")]
    Other(String),
}

impl WorkspacesError {
    /// Stable wire identifier for this error. Kept in sync with the shared
    /// IPC error codes (`no_action`, `bad_request`, …) so the GUI / harness
    /// classifier sees the same strings every service emits.
    pub fn code(&self) -> &'static str {
        match self {
            WorkspacesError::UnknownAction(_) => "no_action",
            WorkspacesError::BadRequest(_) => "bad_request",
            WorkspacesError::Io(_) => "io",
            WorkspacesError::Serde(_) => "serde",
            WorkspacesError::Other(_) => "internal",
        }
    }

    /// Render as a shared-IPC structured error.
    pub fn to_ipc(&self) -> IpcError {
        IpcError::new(self.code(), self.to_string())
    }

    /// Render as an `ok=false` [`Reply`] ready to hand back to the pipe
    /// server's action dispatch.
    pub fn to_reply(&self) -> Reply {
        Reply::err(self.to_ipc())
    }
}

impl From<serde_json::Error> for WorkspacesError {
    fn from(e: serde_json::Error) -> Self {
        WorkspacesError::Serde(e.to_string())
    }
}

/// Convenience alias used throughout the service.
pub type Result<T> = std::result::Result<T, WorkspacesError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_wire_convention() {
        assert_eq!(
            WorkspacesError::UnknownAction("x".into()).code(),
            "no_action"
        );
        assert_eq!(
            WorkspacesError::BadRequest("x".into()).code(),
            "bad_request"
        );
        assert_eq!(WorkspacesError::Other("x".into()).code(), "internal");
    }

    #[test]
    fn to_reply_is_not_ok() {
        let r = WorkspacesError::BadRequest("nope".into()).to_reply();
        assert!(!r.ok);
        let err = r.error.expect("error body");
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("nope"));
    }
}
