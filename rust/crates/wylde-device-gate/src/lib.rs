//! Device-gate service — Rust port of the `device_gate/` Python package.
//!
//! Owns per-device pairing, opaque session tokens, and the three-tier
//! permission model (`read_only`, `tool_use`, `destructive_tool_access`).
//! The Gateway calls `device_gate.verify` on every external request and
//! gates tool access against the returned tier.
//!
//! Each Python module of `device_gate/` maps to one Rust file here, mirroring
//! the granular split. Strangler-fig: the Lifecycle daemon picks Python or
//! this Rust binary via `WYLDE_WYLDE_DEVICE_GATE_IMPL`; both write to the
//! same on-disk `devices.json` + `htpasswd` files, so a live cutover doesn't
//! invalidate existing pairings.
//!
//! Public entry points:
//!   * [`pipe::install`] — register the `device_gate.*` action surface on
//!     the process-wide pipe registry. Idempotent.
//!   * [`pipe::uninstall`] — drop every action handler. Test-only.
//!   * [`core::reset_service`] — clear the singleton service. Test-only.

pub mod auth;
pub mod core;
pub mod pipe;
pub mod run;
pub mod store;

pub use crate::core::{
    devices_store_path, store_root_path, DeviceGateError, DeviceGateService, ALL_TIERS,
    PAIRING_CODE_LENGTH, PAIRING_CODE_TTL_SECONDS, TIER_DESTRUCTIVE, TIER_READ_ONLY, TIER_TOOL_USE,
};
pub use crate::store::{Device, DeviceStore};

#[cfg(test)]
pub(crate) mod test_lock {
    //! Serial-test guard for cases that mutate the process-wide service
    //! singleton or env vars (`DEVICE_GATE_DATA_DIR`, `DEVICE_GATE_HTPASSWD`).
    //! Without this, parallel cargo test threads race and produce
    //! non-deterministic failures.

    use tokio::sync::{Mutex, MutexGuard};

    pub async fn guard() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::const_new(());
        LOCK.lock().await
    }
}
