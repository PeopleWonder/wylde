//! VRAM lease broker — Rust port of `Core/resource_monitor/broker/`.
//!
//! The broker is the fourth Core constituent service (alongside lifecycle /
//! harness / memgraph). It hands out priority-based GPU memory leases to
//! every Wylde service that loads a model, accounts for un-brokered Ollama
//! loads via a synthetic-lease reflection of `/api/ps`, and writes a JSON
//! state snapshot for the Tauri dashboard to consume.
//!
//! Each Python submodule of the broker maps to one Rust module here. The
//! granular split is intentional — the Wylde user asked for it so swapping pieces
//! during the strangler-fig phase stays surgical.
//!
//! Public entry points:
//!   * [`service::install`] — register the `vram.*` action surface and
//!     spawn the reaper / Ollama / manifest background tasks. Idempotent.
//!   * [`service::stop`] — signal the background tasks to drain.
//!   * [`service::reset_for_tests`] — clear singletons in place; for tests.

pub mod config;
pub mod estimate;
pub mod inventory;
pub mod model_cache;
pub mod policy;
pub mod registry;
pub mod service;
pub mod time;
pub mod workers;

pub use registry::Lease;
pub use service::{install, reset_for_tests, stop};

#[cfg(test)]
pub(crate) mod test_lock {
    //! Serial-test guard for cases that mutate the process-wide registry /
    //! model cache. Cargo runs test threads in parallel by default; without
    //! this, parallel mutations of the singletons race and produce
    //! non-deterministic failures.
    //!
    //! Uses a tokio async-aware Mutex so the guard can safely be held across
    //! `.await` points (the `await_holding_lock` clippy lint forbids that
    //! shape with a `std::sync::Mutex`).

    use tokio::sync::{Mutex, MutexGuard};

    pub async fn guard() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::const_new(());
        LOCK.lock().await
    }
}
