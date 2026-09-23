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
//! ## Extended by #250
//!
//! The original gate keyed on the COMBINATION `fn data_dir(...)` + a
//! `.join(".wylde")` in the same file. That deliberately let four resolvers
//! through — model selection, routing / model registry, ollama overrides and
//! the device gate — because they rooted elsewhere and unifying them carried
//! data-migration risk. #250 did that unification, so they now fall inside
//! this gate's remit and it is the thing that keeps them there. Three checks:
//!
//! 1. **One resolver.** No file outside `wylde-shared/src/paths.rs` defines
//!    `fn data_dir` **at all** — the `.join(".wylde")` qualifier is gone,
//!    because after #250 there is no legitimate reason for a second one to
//!    exist under any convention. Delegate with
//!    `pub use wylde_shared::paths::data_dir;` or call it.
//! 2. **No cwd-relative store roots.** No crate `src/` builds a store root
//!    from a bare relative `"data"`. That was #250 hazard 1: `model_state`
//!    and `routing` fell back to `PathBuf::from("data")`, so *which* stores a
//!    process saw was a property of its working directory — stable only
//!    because lifecycle pins that to `wylde_root()`, and silently wrong for
//!    any harness started elsewhere.
//! 3. **The migrated stores stay delegated.** Each of the four files #250
//!    moved is named here and must still reference the canonical resolver.
//!    Without this a future edit could drop back to a private root and pass
//!    checks 1 and 2 by simply naming the function something else.
//!
//! Convention-B `store_path`/`config_path` helpers that also land at
//! `.wylde/data` are untouched — they are not root resolvers.

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

/// Every `.rs` file under every crate's `src/`, except the canonical
/// resolver itself.
fn crate_src_files_except_canonical(crates: &Path, canonical_suffix: &Path) -> Vec<PathBuf> {
    let crate_dirs = fs::read_dir(crates)
        .unwrap_or_else(|e| panic!("cannot read rust/crates at {}: {e}", crates.display()));
    let mut out = Vec::new();
    for c in crate_dirs.flatten() {
        let src = c.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rs_files_under(&src, &mut files);
        out.extend(files.into_iter().filter(|f| !f.ends_with(canonical_suffix)));
    }
    out
}

/// A file that defines `fn data_dir` in any form. Doc/`//` comment lines are
/// ignored, so the prose in this gate and in `paths.rs` does not self-trip.
///
/// #250 dropped the old `.join(".wylde")` qualifier: it existed only to let
/// the four non-convention-A resolvers through, and they are gone.
fn defines_a_data_dir_resolver(text: &str) -> bool {
    text.lines().any(|line| {
        let t = line.trim_start();
        (t.starts_with("fn data_dir(") || t.starts_with("pub fn data_dir(")) && !t.starts_with("//")
    })
}

/// A file that builds a store root out of a bare relative `"data"` — the
/// cwd-relative fallback #250 removed (hazard 1). Comment lines are ignored
/// so a doc line quoting the old shape does not trip the gate.
fn has_cwd_relative_store_root(text: &str) -> bool {
    text.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") {
            return false;
        }
        t.contains("PathBuf::from(\"data\")") || t.contains("Path::new(\"data\")")
    })
}

/// True when `text` reaches `wylde_shared::paths::data_dir`, whether written
/// fully-qualified or pulled in via a braced `use`. The brace strip is what
/// makes `paths::{data_dir, legacy_data_dir}` — the form rustfmt produces for
/// the files that need both — count as a reference.
fn references_canonical_resolver(text: &str) -> bool {
    text.replace(['{', '}'], "").contains("paths::data_dir")
}

/// The four stores #250 moved onto convention A, as `<crate>/src/<path>`
/// relative to `rust/crates`. Each must still reach the canonical resolver;
/// a private root under a different function name would otherwise slip past
/// the two structural checks above.
const MIGRATED_STORES: [(&str, &str); 4] = [
    (
        "wylde-harness/src/model_registry/model_state.rs",
        "model selection (default_model.json, active_model.json)",
    ),
    (
        "wylde-harness/src/model_registry/routing/mod.rs",
        "routing profiles / model registry",
    ),
    (
        "wylde-harness/src/settings/ollama_overrides.rs",
        "per-model Ollama overrides",
    ),
    (
        "wylde-device-gate/src/core.rs",
        "device gate (devices.json, htpasswd, action_log.json)",
    ),
];

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

    let mut offenders: Vec<PathBuf> = Vec::new();
    for f in crate_src_files_except_canonical(&crates, &canonical_suffix) {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        if defines_a_data_dir_resolver(&text) {
            offenders.push(f);
        }
    }

    assert!(
        offenders.is_empty(),
        "`fn data_dir` must live ONLY in wylde-shared/src/paths.rs (#138, \
         widened to every convention by #250); delegate with \
         `pub use wylde_shared::paths::data_dir;` or call it, and name a \
         per-store subdirectory helper something else (`store_root`). \
         Copies found:\n{}",
        offenders
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// #250 hazard 1: a store root built from a bare relative `"data"` is not a
/// location at all — it is whatever the process working directory happens to
/// be. Two of the four migrated resolvers did exactly this, so a harness
/// started from anywhere but the estate root silently read and wrote a
/// different set of stores, with no error to notice.
#[test]
fn no_crate_roots_a_store_at_a_cwd_relative_data_dir() {
    let crates = rust_crates_root();
    let canonical_suffix = Path::new("wylde-shared").join("src").join("paths.rs");

    let mut offenders: Vec<PathBuf> = Vec::new();
    for f in crate_src_files_except_canonical(&crates, &canonical_suffix) {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        if has_cwd_relative_store_root(&text) {
            offenders.push(f);
        }
    }

    assert!(
        offenders.is_empty(),
        "a store root must be anchored, not cwd-relative (#250): resolve it \
         through `wylde_shared::paths::data_dir()` rather than \
         `PathBuf::from(\"data\")`. Found in:\n{}",
        offenders
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The four stores #250 moved must still reach the canonical resolver. The
/// two checks above are necessary but not sufficient: a future edit could
/// reintroduce a private root simply by not calling the function `data_dir`,
/// and this is the check that notices.
#[test]
fn the_migrated_stores_still_delegate_to_the_canonical_resolver() {
    let crates = rust_crates_root();
    let mut broken: Vec<String> = Vec::new();

    for (rel, what) in MIGRATED_STORES {
        let path = crates.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(text) = fs::read_to_string(&path) else {
            broken.push(format!(
                "  - {rel} ({what}) — file missing; if it moved, update \
                 MIGRATED_STORES so the store keeps a gate"
            ));
            continue;
        };
        if !references_canonical_resolver(&text) {
            broken.push(format!(
                "  - {rel} ({what}) — no reference to \
                 `wylde_shared::paths::data_dir`"
            ));
        }
    }

    assert!(
        broken.is_empty(),
        "every store #250 unified must resolve through convention A, and \
         adopt its legacy location so an upgrade does not silently reset the \
         user's data (see docs/data-roots.md):\n{}",
        broken.join("\n")
    );
}
