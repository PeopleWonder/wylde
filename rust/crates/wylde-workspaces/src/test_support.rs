//! Test helper — single shared lock + per-test `WYLDE_DATA_DIR` tempdir.
//!
//! Relocated from the harness `workspaces::test_support` (Slice 0b). The
//! registry / persona / rag test modules all touch the on-disk store under
//! `<data_dir>/workspaces/`; they run as threads in the same cargo-test
//! binary and `WYLDE_DATA_DIR` is process-wide, so they MUST serialize
//! against the shared [`crate::common::TEST_ENV_LOCK`].

#![cfg(test)]

use std::sync::MutexGuard;

use tempfile::TempDir;

use crate::common::TEST_ENV_LOCK as ENV_LOCK;

/// Per-test data-dir sandbox. Hold this for the body of any test that touches
/// the workspaces store on disk.
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

    /// A synthetic, **absolute**, per-test-unique workspace path under this
    /// env's tempdir. Keeps `slug_for` deterministic regardless of cwd.
    pub fn ws_path(&self, name: &str) -> String {
        self._tempdir.path().join(name).to_string_lossy().into_owned()
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
