//! Embedding-host facade — Rust port of `device_gate/run.py`.
//!
//! The Python module exposes `start_device_gate` / `stop_device_gate` so a
//! future in-process daemon mode (or tests) can drive the service without
//! spawning a subprocess. The Rust equivalent mirrors that surface; the
//! long-lived `__main__` loop lives in `main.rs`.
//!
//! Most callers will use the `wylde-device-gate` binary directly. This
//! module exists so the per-module file-per-Python-module structure stays
//! 1:1 — the Wylde user's standing instruction.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::pipe;

pub const SERVICE_NAME: &str = "wylde-device-gate";

static STARTED: AtomicBool = AtomicBool::new(false);

/// Install the pipe action surface. Idempotent. Returns `true` if the surface
/// is now registered (or was already), `false` only if a future hard
/// dependency goes missing (today there are none — `register_action` is
/// in-process, so this always succeeds).
pub fn start_device_gate() -> bool {
    if STARTED.swap(true, Ordering::SeqCst) {
        return true;
    }
    pipe::install();
    true
}

/// Drop the pipe action surface. Reserved for future graceful shutdown;
/// today the pipe drains on process exit so this is a no-op outside tests.
pub fn stop_device_gate() {
    pipe::uninstall();
    STARTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lock::guard;
    use wylde_shared::ipc::list_actions;

    #[tokio::test(flavor = "current_thread")]
    async fn start_is_idempotent_and_registers_actions() {
        let _g = guard().await;
        stop_device_gate();
        assert!(start_device_gate());
        assert!(start_device_gate()); // idempotent
        let actions = list_actions();
        assert!(actions.iter().any(|n| n == "device_gate.verify"));
        assert!(actions.iter().any(|n| n == "device_gate.start_pairing"));
        stop_device_gate();
    }
}
