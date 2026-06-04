//! Test helper — single shared lock + per-test `WYLDE_DATA_DIR`
//! tempdir, scoped to the conversation document store.
//!
//! Identical in shape to `short_term::test_support::TestEnv`: conversation
//! JSON files resolve under `conversations_dir()` = `data_dir()/conversations`,
//! and the active-conversation pointer under `data_dir()` directly. Both
//! re-read `WYLDE_DATA_DIR` on every call, so the per-test tempdir fully
//! isolates this suite's reads/writes. Every suite that mutates the
//! process-wide `WYLDE_DATA_DIR` serialises against the shared
//! [`TEST_ENV_LOCK`]; holding it for the test body is enough.

#![cfg(test)]

use std::sync::MutexGuard;

use tempfile::TempDir;

use crate::memory::common::TEST_ENV_LOCK as ENV_LOCK;

/// Per-test data-dir sandbox. Hold this for the body of any test that
/// reads/writes conversation JSON files.
pub struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    _tempdir: TempDir,
    prior: Option<std::ffi::OsString>,
    prior_conv: Option<std::ffi::OsString>,
}

impl TestEnv {
    pub fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tempdir = TempDir::new().expect("create test tempdir");
        let prior = std::env::var_os("WYLDE_DATA_DIR");
        // A stray CONVERSATIONS_DIR override on the runner would bypass
        // the tempdir and leak into the user's real conversations — clear
        // it for the test body and restore on drop.
        let prior_conv = std::env::var_os("CONVERSATIONS_DIR");
        std::env::remove_var("CONVERSATIONS_DIR");
        std::env::set_var("WYLDE_DATA_DIR", tempdir.path());
        Self {
            _guard: guard,
            _tempdir: tempdir,
            prior,
            prior_conv,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        if let Some(v) = self.prior_conv.take() {
            std::env::set_var("CONVERSATIONS_DIR", v);
        }
    }
}
