//! Per-test `WYLDE_DATA_DIR` tempdir + the shared `TEST_ENV_LOCK` mutex.
//!
//! Reuses the same lock that `workspaces::test_support` and
//! `long_term::test_support` hold. `WYLDE_DATA_DIR` is process-wide, so
//! every test that mutates it MUST serialise through exactly one mutex.
//! See `memory/wylde_phase7b_long_term_shipped.md` for the rationale —
//! independent per-module locks would race when both modules are
//! exercised in the same `cargo test` binary.

#![cfg(test)]

use std::sync::MutexGuard;

use tempfile::TempDir;

use crate::memory::common::TEST_ENV_LOCK as ENV_LOCK;

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
