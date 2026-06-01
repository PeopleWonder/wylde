//! Filesystem locations the harness needs: the Wylde repo root, the Python
//! interpreter inside `.venv`, and the release Rust service binaries.
//!
//! Everything is anchored on `CARGO_MANIFEST_DIR` (this package lives at
//! `<repo>/rust/tests/parity`), so the harness works regardless of the
//! caller's current directory.

use std::path::{Path, PathBuf};

/// Absolute path to the Wylde repo root (`<repo>`).
///
/// This package sits three directories deep: `rust/tests/parity`.
pub fn repo_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("..")
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root from CARGO_MANIFEST_DIR")
}

/// The Python interpreter inside the repo's virtualenv.
///
/// Wylde's `.venv` is the only interpreter with the service dependencies
/// installed; the system `py -3` resolves to a bare 3.14 that lacks them.
pub fn venv_python() -> PathBuf {
    repo_root().join(".venv").join("Scripts").join("python.exe")
}

/// A release Rust service binary, e.g. `rust_release_bin("wylde-gateway")`.
///
/// The lookup order is:
///
/// 1. `WYLDE_PARITY_BIN_DIR` — explicit override, treated as the directory
///    that holds the `.exe`s directly. Set this when the live stack's
///    `rust/target/release/` is locked and the test was built into a
///    sibling tree (e.g. `target-fresh/release`).
/// 2. `CARGO_TARGET_DIR` — cargo's own target-tree override. Honoured as
///    `<CARGO_TARGET_DIR>/release/<name>.exe` so a project-wide retarget
///    "just works" without an extra parity-specific knob.
/// 3. `<repo>/rust/target/release/` — the default cargo output, used when
///    neither override is set.
///
/// Each candidate is checked for existence in order; the first hit wins.
/// If none exist the default path is returned anyway so the caller's
/// `require_artifact` assertion surfaces it with the usual hint.
pub fn rust_release_bin(name: &str) -> PathBuf {
    let exe = format!("{name}.exe");
    let candidates = bin_dir_candidates();
    for dir in &candidates {
        let path = dir.join(&exe);
        if path.exists() {
            return path;
        }
    }
    // Default path so the operator-facing error message matches what they'd
    // expect from a stock build. `require_artifact` will surface it with
    // the usual remediation hint.
    candidates
        .last()
        .expect("default release dir is always present")
        .join(&exe)
}

/// Directories to search for a release Rust service binary, in priority
/// order. Resolves the live process environment, then delegates to the
/// pure [`bin_dir_candidates_from`] for the actual policy — env vars are
/// process-global, so isolating the read makes the policy unit-testable.
fn bin_dir_candidates() -> Vec<PathBuf> {
    bin_dir_candidates_from(
        std::env::var_os("WYLDE_PARITY_BIN_DIR"),
        std::env::var_os("CARGO_TARGET_DIR"),
        repo_root(),
    )
}

/// Pure variant of [`bin_dir_candidates`] — takes its inputs explicitly so
/// the override policy can be exercised in unit tests without mutating
/// process env (which races other parallel tests in the same binary).
fn bin_dir_candidates_from(
    parity_bin_dir: Option<std::ffi::OsString>,
    cargo_target_dir: Option<std::ffi::OsString>,
    repo_root: PathBuf,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(3);
    if let Some(dir) = parity_bin_dir {
        out.push(PathBuf::from(dir));
    }
    if let Some(target) = cargo_target_dir {
        out.push(PathBuf::from(target).join("release"));
    }
    out.push(repo_root.join("rust").join("target").join("release"));
    out
}

/// Panic with an actionable message if a required artifact is missing.
///
/// Parity tests are a cutover gate, not a unit test — a missing binary or
/// interpreter is an operator error, so fail loud and tell them how to fix
/// it rather than silently skipping.
pub fn require_artifact(path: &Path, hint: &str) {
    assert!(
        path.exists(),
        "parity prerequisite missing: {}\n  fix: {hint}",
        path.display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn no_overrides_returns_only_default() {
        let c = bin_dir_candidates_from(None, None, root());
        assert_eq!(c, vec![PathBuf::from("/repo/rust/target/release")]);
    }

    #[test]
    fn parity_override_takes_priority() {
        let c = bin_dir_candidates_from(Some(OsString::from("/tmp/fresh")), None, root());
        assert_eq!(c[0], PathBuf::from("/tmp/fresh"));
        assert_eq!(c.last().unwrap(), &PathBuf::from("/repo/rust/target/release"));
    }

    #[test]
    fn cargo_target_dir_appends_release_segment() {
        let c = bin_dir_candidates_from(None, Some(OsString::from("/build/wylde")), root());
        // CARGO_TARGET_DIR is the *target* root, the .exe lives under release/.
        assert_eq!(c[0], PathBuf::from("/build/wylde/release"));
    }

    #[test]
    fn both_overrides_ordered_parity_then_cargo_then_default() {
        let c = bin_dir_candidates_from(
            Some(OsString::from("/tmp/fresh")),
            Some(OsString::from("/build/wylde")),
            root(),
        );
        assert_eq!(
            c,
            vec![
                PathBuf::from("/tmp/fresh"),
                PathBuf::from("/build/wylde/release"),
                PathBuf::from("/repo/rust/target/release"),
            ]
        );
    }
}
