//! Three-tier error classification for the workspaces client.
//!
//! Implements scope v2 §7.7: every failure the client surfaces is tagged
//! with an [`ErrorTier`] so consumers render the right affordance —
//! `Transient` (inline spinner, will retry), `Degraded` (yellow badge,
//! breaker tripped / fallbacks active), `Broken` (red banner + Reconnect).
//!
//! The tier is derived from the shared IPC error `code` ([`classify`]).
//! A separate `transport` flag marks failures that originate at the
//! transport layer (timeout / connect) — only those are retried and only
//! those count toward the circuit breaker. Server-side application errors
//! (`no_action`, `bad_request`, …) mean the service is healthy but the
//! request was wrong: no retry, no breaker hit.

use wylde_shared::ipc::IpcError;

/// The UI/health tier of a client error (scope v2 §7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorTier {
    /// A single timeout that will be retried. Inline spinner, no user action.
    Transient,
    /// Breaker tripped or partial results. Yellow badge + fallbacks active.
    Degraded,
    /// Service down / pipe missing / hard failure. Red banner + Reconnect.
    Broken,
}

impl ErrorTier {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorTier::Transient => "transient",
            ErrorTier::Degraded => "degraded",
            ErrorTier::Broken => "broken",
        }
    }
}

/// Error returned by every [`crate::WorkspacesClient`] verb call.
///
/// Named to avoid shadowing [`wylde_shared::ipc::IpcError`] (the wire type
/// it is often derived from). Carries the stable wire `code`, a
/// human-readable `message`, the [`ErrorTier`], and whether the failure was
/// transport-level (retry / breaker eligible).
#[derive(Debug, Clone, thiserror::Error)]
#[error("[{tier}] {code}: {message}", tier = self.tier.as_str())]
pub struct WorkspacesClientError {
    pub tier: ErrorTier,
    pub code: String,
    pub message: String,
    /// True iff the failure originated at the transport layer (timeout,
    /// connect, pipe I/O). Transport failures are retryable and count toward
    /// the circuit breaker; application errors do not.
    pub transport: bool,
}

impl WorkspacesClientError {
    /// Build from a shared-IPC wire error, classifying the tier + transport
    /// flag from its `code`.
    pub fn from_ipc(err: IpcError) -> Self {
        let (tier, transport) = classify(&err.code);
        Self {
            tier,
            code: err.code,
            message: err.message,
            transport,
        }
    }

    /// The circuit breaker is open for this verb — fail fast into the
    /// consumer's fallback rather than hanging. Tier `Degraded`, not a
    /// transport failure (so it never re-trips the breaker it came from).
    pub fn breaker_open(verb: &str) -> Self {
        Self {
            tier: ErrorTier::Degraded,
            code: "breaker_open".to_string(),
            message: format!("circuit breaker open for verb {verb:?}; failing fast"),
            transport: false,
        }
    }

    /// The reply arrived but didn't match the expected shape. The service is
    /// reachable, so this is `Broken` for this call but not a transport hit.
    pub fn decode(message: impl Into<String>) -> Self {
        Self {
            tier: ErrorTier::Broken,
            code: "decode".to_string(),
            message: message.into(),
            transport: false,
        }
    }

    /// No [`crate::verbs::VerbDef`] is registered for the requested verb —
    /// a client-side programming error.
    pub fn unknown_verb(verb: &str) -> Self {
        Self {
            tier: ErrorTier::Broken,
            code: "unknown_verb".to_string(),
            message: format!("no client verb definition for {verb:?}"),
            transport: false,
        }
    }

    /// Should this failure count toward the breaker / be retried?
    pub fn counts_toward_breaker(&self) -> bool {
        self.transport
    }
}

/// Map a shared-IPC error `code` → (tier, is_transport).
///
/// Timeouts are `Transient` (retry). Connect / pipe-down / handshake
/// failures are `Broken` (service unreachable). Everything else is an
/// application-level error: `Broken` for this call, but not a transport
/// failure (no retry, no breaker hit).
pub fn classify(code: &str) -> (ErrorTier, bool) {
    match code {
        // Transport timeouts — retryable.
        "pipe_timeout" | "read_timeout" | "handshake_timeout" => (ErrorTier::Transient, true),
        // Transport / connection down — service unreachable.
        "pipe_connect" | "pipe_unavailable" | "pipe_io" | "handshake_io" | "handshake_rejected"
        | "version_mismatch" | "ipc_disabled" | "no_http_backend" => (ErrorTier::Broken, true),
        // Application-level errors: service answered with a logical failure.
        _ => (ErrorTier::Broken, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_transient_and_transport() {
        let (tier, transport) = classify("pipe_timeout");
        assert_eq!(tier, ErrorTier::Transient);
        assert!(transport);
    }

    #[test]
    fn connect_failures_are_broken_transport() {
        let (tier, transport) = classify("pipe_connect");
        assert_eq!(tier, ErrorTier::Broken);
        assert!(transport);
    }

    #[test]
    fn application_errors_are_broken_not_transport() {
        let (tier, transport) = classify("no_action");
        assert_eq!(tier, ErrorTier::Broken);
        assert!(!transport);
    }

    #[test]
    fn breaker_open_is_degraded_non_transport() {
        let e = WorkspacesClientError::breaker_open("ping");
        assert_eq!(e.tier, ErrorTier::Degraded);
        assert!(!e.counts_toward_breaker());
    }

    #[test]
    fn from_ipc_preserves_code_and_message() {
        let e = WorkspacesClientError::from_ipc(IpcError::new("no_action", "unknown action"));
        assert_eq!(e.code, "no_action");
        assert!(e.message.contains("unknown action"));
        assert_eq!(e.tier, ErrorTier::Broken);
        assert!(!e.transport);
    }
}
