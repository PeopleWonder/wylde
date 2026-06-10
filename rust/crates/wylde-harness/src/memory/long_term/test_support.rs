//! Test helper — per-test `WYLDE_DATA_DIR` tempdir + shared mutex.
//!
//! Mirrors `workspaces::test_support::TestEnv` exactly. Two test
//! modules under `long_term/` (`entries`, future `vector`) and a few
//! more under the workspace tree all touch `WYLDE_DATA_DIR`; since
//! that's process-wide they must hold a shared mutex.
//!
//! We keep this lock *distinct* from `workspaces::test_support::ENV_LOCK`
//! because the two modules write to non-overlapping subpaths under the
//! same env-var, but the env var itself MUST be guarded by exactly one
//! mutex for the whole process. If we ever exercise both in the same
//! cargo-test binary at the same time, they'd race.
//!
//! Resolution: route the long_term env guard through the *same* mutex
//! the workspaces helper uses, via a shared module-private static. See
//! `crate::memory::env_guard` if a shared helper lands later; for now
//! we depend on `workspaces::test_support::TestEnv` to gate other
//! tests too, and the long_term tests hold the same lock via this
//! module.

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
