//! Per-extension MCP spec-version compatibility policy (Q-E1).
//!
//! Wylde host accepts wire-spec versions N (current) and N-1 (prior),
//! rejects N+1 (future) with a clear log line. Anything else is also
//! rejected, with the same log line shape.
//!
//! "Acceptable" here means: the host completes the `initialize`
//! handshake and proceeds with tool calls. The host always **sends**
//! the current spec version; the rule below is about what the host
//! tolerates in the server's reply.

use crate::config::{MCP_SPEC_VERSION, MCP_SPEC_VERSION_PREV};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDecision {
    /// Server reported the same spec version we sent.
    Current,
    /// Server reported the prior spec version — accept (N-1 policy).
    Prior,
    /// Server reported a newer spec version than us — reject.
    Future,
    /// Server reported something unrecognised — reject.
    Unknown,
}

impl VersionDecision {
    pub fn accepted(self) -> bool {
        matches!(self, VersionDecision::Current | VersionDecision::Prior)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VersionDecision::Current => "current",
            VersionDecision::Prior => "prior",
            VersionDecision::Future => "future",
            VersionDecision::Unknown => "unknown",
        }
    }
}

/// Decide whether to accept the server-reported spec version.
///
/// `server_reported` is what came back in the `initialize` response's
/// `protocolVersion` field. The N/N-1/N+1 policy is hard-coded against
/// [`MCP_SPEC_VERSION`] / [`MCP_SPEC_VERSION_PREV`].
pub fn classify(server_reported: &str) -> VersionDecision {
    if server_reported == MCP_SPEC_VERSION {
        VersionDecision::Current
    } else if server_reported == MCP_SPEC_VERSION_PREV {
        VersionDecision::Prior
    } else if version_gt(server_reported, MCP_SPEC_VERSION) {
        VersionDecision::Future
    } else {
        VersionDecision::Unknown
    }
}

/// Date-string compare. MCP spec versions are `YYYY-MM-DD`; lexical
/// ordering matches chronological for that format. Anything that
/// doesn't parse as that shape is treated as "not greater" so it
/// falls into the [`VersionDecision::Unknown`] bucket, not the
/// `Future` bucket (which would be a soft-fail with a misleading
/// message).
fn version_gt(a: &str, b: &str) -> bool {
    if !is_iso_date(a) || !is_iso_date(b) {
        return false;
    }
    a > b
}

fn is_iso_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(|c| c.is_ascii_digit())
        && bytes[5..7].iter().all(|c| c.is_ascii_digit())
        && bytes[8..10].iter().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_accepts() {
        assert_eq!(classify(MCP_SPEC_VERSION), VersionDecision::Current);
        assert!(classify(MCP_SPEC_VERSION).accepted());
    }

    #[test]
    fn prior_accepts() {
        assert_eq!(classify(MCP_SPEC_VERSION_PREV), VersionDecision::Prior);
        assert!(classify(MCP_SPEC_VERSION_PREV).accepted());
    }

    #[test]
    fn future_rejects() {
        // A date strictly after MCP_SPEC_VERSION.
        assert_eq!(classify("2099-01-01"), VersionDecision::Future);
        assert!(!classify("2099-01-01").accepted());
    }

    #[test]
    fn unknown_rejects() {
        assert_eq!(classify("not-a-version"), VersionDecision::Unknown);
        assert_eq!(classify("1999-01-01"), VersionDecision::Unknown);
        assert!(!classify("garbage").accepted());
    }
}
