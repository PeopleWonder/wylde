//! Test helper — single shared lock + per-test `WYLDE_DATA_DIR`
//! tempdir.
//!
//! Three test modules (`store::tests`, `mru::tests`, `actions::tests`)
//! all touch the JSON registry. They run as threads inside the same
//! cargo-test binary; the `WYLDE_DATA_DIR` env var is process-wide, so
//! they MUST hold a shared mutex while reading/writing it.
//!
//! Each test asks for a [`TestEnv`] which:
//! 1. Takes the process-wide lock (single-writer over the env var).
//! 2. Builds a unique `tempdir()`.
//! 3. Points `WYLDE_DATA_DIR` at it.
//! 4. Restores the prior value on drop.
//!
//! The lock is dropped along with the [`TestEnv`], so two tests can't
//! interleave their env-var manipulation. With `data_dir()` re-reading
//! on every call, the cost is "lock held for the test body" — cheap
//! and matches what serial_test would give us without the extra dep.

#![cfg(test)]

use std::sync::MutexGuard;

use tempfile::TempDir;

use crate::memory::common::TEST_ENV_LOCK as ENV_LOCK;

/// Per-test data-dir sandbox. Hold this for the body of any test that
/// touches the workspace registry / MRU cap / persona on disk.
pub struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    _tempdir: TempDir,
    prior: Option<std::ffi::OsString>,
}

impl TestEnv {
    pub fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tempdir = TempDir::new().expect("create test tempdir");
        let prior = std::env::var_os("WYLDE_DATA_DIR");
        std::env::set_var("WYLDE_DATA_DIR", tempdir.path());
        Self {
            _guard: guard,
            _tempdir: tempdir,
            prior,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
    }
}
