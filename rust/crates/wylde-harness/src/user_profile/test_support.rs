//! Test helper — per-test `WYLDE_DATA_DIR` tempdir, serialised against
//! the shared env lock.
//!
//! Same shape as `memory::short_term::test_support::TestEnv`: the
//! profile JSON resolves under `data_dir()`, which re-reads
//! `WYLDE_DATA_DIR` on every call, so each test gets an isolated store.
//! All env-mutating harness test suites share
//! [`crate::memory::common::TEST_ENV_LOCK`], so holding it for the test
//! body keeps the snapshot/restore atomic under cargo's parallel runner.

#![cfg(test)]

use std::sync::MutexGuard;

use tempfile::TempDir;

use crate::memory::common::TEST_ENV_LOCK as ENV_LOCK;

/// Per-test data-dir sandbox. Hold this for the body of any test that
/// reads/writes the user-profile store.
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
