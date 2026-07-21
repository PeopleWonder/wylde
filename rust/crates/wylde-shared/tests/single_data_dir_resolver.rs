//! #138 structural gate — convention A (`<root>/.wylde/data`) has exactly ONE
//! resolver.
//!
//! Before #138 the `WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data`
//! body was copy-pasted as a private `fn data_dir()` in seven crates, free to
//! drift, and the tests named for the property could not fail. This test walks
//! every crate's `src/` and fails — as a REQUIRED backend test, not an advisory
//! lint — if any file other than the canonical `wylde-shared/src/paths.rs`
//! defines a `fn data_dir` that resolves the `.wylde/data` root.
//!
//! It keys on the COMBINATION `fn data_dir(...)` + a `.join(".wylde")` in the
//! same file, so it flags a re-pasted convention-A copy while ignoring the
//! genuinely-different resolvers (`data/model_registry`, `device_gate/data`,
//! `<ROOT>/data`) and the convention-B `store_path`/`config_path` helpers that
//! also land at `.wylde/data` but are not named `data_dir`.

use std::fs;
use std::path::{Path, PathBuf};

/// `<repo>/rust/crates` — `CARGO_MANIFEST_DIR` is `<repo>/rust/crates/wylde-shared`.
fn rust_crates_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("wylde-shared has a parent (rust/crates)")
        .to_path_buf()
}

fn rs_files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            rs_files_under(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// A file that defines a `fn data_dir` AND joins `.wylde` — i.e. a convention-A
/// resolver. Doc/`//` comment lines are ignored for the definition check.
fn defines_convention_a_data_dir(text: &str) -> bool {
    let has_def = text.lines().any(|line| {
        let t = line.trim_start();
        (t.starts_with("fn data_dir(") || t.starts_with("pub fn data_dir(")) && !t.starts_with("//")
    });
    has_def && text.contains(".join(\".wylde\")")
}

#[test]
fn convention_a_data_dir_has_a_single_resolver() {
    let crates = rust_crates_root();
    let canonical_suffix = Path::new("wylde-shared").join("src").join("paths.rs");

    let canonical = crates.join(&canonical_suffix);
    assert!(
        canonical.is_file(),
        "the ONE canonical resolver is missing at {}",
        canonical.display()
    );

    let crate_dirs = fs::read_dir(&crates)
        .unwrap_or_else(|e| panic!("cannot read rust/crates at {}: {e}", crates.display()));

    let mut offenders: Vec<PathBuf> = Vec::new();
    for c in crate_dirs.flatten() {
        let src = c.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rs_files_under(&src, &mut files);
        for f in files {
            if f.ends_with(&canonical_suffix) {
                continue; // the canonical resolver is allowed to define it
            }
            let Ok(text) = fs::read_to_string(&f) else {
                continue;
            };
            if defines_convention_a_data_dir(&text) {
                offenders.push(f);
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "convention-A `fn data_dir` (`.wylde/data`) must live ONLY in \
         wylde-shared/src/paths.rs (#138); delegate with \
         `pub use wylde_shared::paths::data_dir;`. Re-pasted copies found:\n{}",
        offenders
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
