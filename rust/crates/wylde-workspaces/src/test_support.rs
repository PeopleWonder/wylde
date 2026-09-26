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

/// Env vars [`TestEnv`] pins for the test body and restores on drop.
///
/// The graph pair keeps unit tests hermetic: the concept-build path projects
/// into Neo4j (fail-soft), so without a pin a test either dials the rig's REAL
/// graph (writing into it) or, with Neo4j down as on CI, waits on the connect.
/// A dead port plus a short connect timeout exercises the same fail-soft path
/// in well under a second. Real-graph tests live in `tests/` (live-graph job).
const PINNED: [(&str, &str); 3] = [
    ("WYLDE_DATA_DIR", ""), // set per-test to the tempdir
    ("GRAPH_BOLT_URL", "bolt://127.0.0.1:1"),
    ("WYLDE_BOLT_CONNECT_TIMEOUT_SECS", "0.25"),
];

/// Per-test data-dir sandbox. Hold this for the body of any test that touches
/// the workspaces store on disk.
pub struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    _tempdir: TempDir,
    prior: Vec<Option<std::ffi::OsString>>,
}

impl TestEnv {
    pub fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tempdir = TempDir::new().expect("create test tempdir");
        let prior = PINNED.iter().map(|(k, _)| std::env::var_os(k)).collect();
        for (k, v) in PINNED {
            if k == "WYLDE_DATA_DIR" {
                std::env::set_var(k, tempdir.path());
            } else {
                std::env::set_var(k, v);
            }
        }
        Self {
            _guard: guard,
            _tempdir: tempdir,
            prior,
        }
    }

    /// A synthetic, **absolute**, per-test-unique workspace path under this
    /// env's tempdir. Keeps `slug_for` deterministic regardless of cwd.
    pub fn ws_path(&self, name: &str) -> String {
        self._tempdir
            .path()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for ((k, _), prior) in PINNED.iter().zip(self.prior.drain(..)) {
            match prior {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
