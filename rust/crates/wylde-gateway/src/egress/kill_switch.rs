//! Egress kill switch — process-wide flag that severs every outbound call.
//!
//! Rust port of `Gateway/egress/kill_switch.py`. Two ways to engage it:
//!
//! * **Boot env var.** `WYLDE_GATEWAY_EGRESS_KILL_SWITCH_INIT=true` flips
//!   the flag at process start. Used during incident response so the
//!   Gateway boots already-blocked.
//! * **Runtime API.** `POST /api/egress/kill` toggles the flag without a
//!   restart.
//!
//! The flag is a single [`AtomicBool`] — per-destination disable is
//! [`super::destinations::resolve`]'s job. The kill switch is the bigger
//! hammer: stop all outbound traffic until someone investigates.

use std::sync::atomic::{AtomicBool, Ordering};

static RUNTIME_FLAG: AtomicBool = AtomicBool::new(false);

/// Crate-wide serialization lock for tests that mutate the process-wide
/// kill switch and/or destinations registry. Exposed here so every test
/// module touching that state takes the **same** lock — per-module locks
/// don't compose across `cargo test`'s parallel scheduler. Async because
/// many callers hold it across `.await` (clippy forbids
/// `std::sync::Mutex` in that position).
#[cfg(test)]
pub(crate) static EGRESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Apply the `WYLDE_GATEWAY_EGRESS_KILL_SWITCH_INIT` env var. Called from
/// the lifespan startup so a hot-reload of settings doesn't double-flip.
/// Idempotent — calling twice with the same env value is a no-op.
pub fn apply_env_bootstrap() {
    let raw = std::env::var("WYLDE_GATEWAY_EGRESS_KILL_SWITCH_INIT").unwrap_or_default();
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ) && !RUNTIME_FLAG.swap(true, Ordering::SeqCst)
    {
        tracing::warn!("egress kill switch ENGAGED at startup via env var");
    }
}

/// Read the kill-switch state.
pub fn is_blocked() -> bool {
    RUNTIME_FLAG.load(Ordering::SeqCst)
}

/// Toggle the runtime kill switch. Returns the new state.
pub fn set_blocked(enabled: bool) -> bool {
    let prev = RUNTIME_FLAG.swap(enabled, Ordering::SeqCst);
    if enabled && !prev {
        tracing::warn!("egress kill switch ENGAGED — all outbound calls blocked");
    } else if !enabled && prev {
        tracing::warn!("egress kill switch released — outbound calls re-enabled");
    }
    enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_is_unblocked() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        set_blocked(false);
        assert!(!is_blocked());
    }

    #[tokio::test]
    async fn set_then_clear_round_trip() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        set_blocked(true);
        assert!(is_blocked());
        set_blocked(false);
        assert!(!is_blocked());
    }

    #[tokio::test]
    async fn set_blocked_returns_new_state() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        let prev = is_blocked();
        assert!(set_blocked(true));
        assert!(!set_blocked(false));
        set_blocked(prev);
    }
}
