//! Per-consumer fallback hooks (scope v2 §7.5).
//!
//! When the service is `Degraded`/`Broken`, each consumer runs a
//! degraded-mode behaviour instead of hanging or erroring out: the chat
//! turn driver answers with harness-only context, the composer falls back
//! to plain text, the graph tab shows the last cached render, and so on.
//!
//! This module is the *seam* for that policy — the [`FallbackOutcome`] enum
//! and the [`FallbackHook`] trait. Slice 0a wires the seam without any real
//! consumer behaviour; the concrete hooks land with their consumers in later
//! phases. [`NoFallback`] is the default (propagate the error unchanged).

use serde_json::Value;

use crate::error::WorkspacesClientError;

/// What a fallback decided to do for a failed verb call.
#[derive(Debug, Clone)]
pub enum FallbackOutcome {
    /// The consumer produced a degraded-but-usable value (e.g. a cached
    /// render, harness-only context). The client returns this in place of
    /// the failed call.
    Used(Value),
    /// No usable fallback exists, but the failure should be swallowed into a
    /// neutral "unavailable" state (e.g. an empty list) rather than surfaced
    /// as an error.
    Unavailable,
    /// No fallback — propagate the original error to the caller.
    Propagate,
}

/// A consumer-supplied degraded-mode behaviour. Implementors decide what to
/// do when a verb fails because the service is slow/down.
pub trait FallbackHook: Send + Sync {
    /// Called when a verb call fails after retries (or the breaker is open).
    /// Return how to degrade for this `verb`.
    fn on_failure(&self, verb: &str, err: &WorkspacesClientError) -> FallbackOutcome;
}

/// The default hook: never degrades, always propagates the error. The Slice
/// 0a client uses this; consumers swap in their own hook later.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFallback;

impl FallbackHook for NoFallback {
    fn on_failure(&self, _verb: &str, _err: &WorkspacesClientError) -> FallbackOutcome {
        FallbackOutcome::Propagate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WorkspacesClientError;

    #[test]
    fn no_fallback_propagates() {
        let hook = NoFallback;
        let err = WorkspacesClientError::breaker_open("ping");
        assert!(matches!(
            hook.on_failure("ping", &err),
            FallbackOutcome::Propagate
        ));
    }
}
