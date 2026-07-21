//! The single canonical data-root resolver (#138).
//!
//! Convention A — `WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data` —
//! is the root under which encryption prefs, graph profiles, `settings/*.json`,
//! the memory tiers, and the workspace registry all live. Before #138 its body
//! was copy-pasted as a private `fn data_dir()` in seven crates, each free to
//! drift, and the three tests named for the property asserted only that the
//! resolved path was *non-empty* — a gate that cannot fail.
//!
//! This is now the ONE definition. The others delegate here with a
//! `pub use wylde_shared::paths::data_dir;`. A structural gate
//! (`rust/crates/wylde-shared/tests/single_data_dir_resolver.rs`) walks every
//! crate's `src/` and fails if any file other than this one defines a
//! `fn data_dir` that resolves the `.wylde/data` root — so a re-paste turns a
//! required backend test red rather than silently reintroducing the drift.
//!
//! Scope note: the model-registry, device-gate, and ollama-override resolvers
//! deliberately root elsewhere (`data/model_registry`, `device_gate/data`,
//! `<ROOT>/data`) and are NOT convention A; unifying those carries data-migration
//! risk and is tracked separately in #138's remaining criteria. This module is
//! the single source of truth for convention A specifically.

use std::path::{Path, PathBuf};

/// `WYLDE_ROOT`, or the process cwd when unset. Lifecycle exports `WYLDE_ROOT`
/// when it spawns a service; the `.` fallback is the historical behaviour every
/// copy of this resolver shared (and the reason a mis-launched service resolves
/// a cwd-relative data dir — see #138 H6).
pub fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The data dir under an explicit root — the pure, env-free core of [`data_dir`].
/// Split out so a gate can pin the exact `<root>/.wylde/data` shape without
/// racing on process-wide env.
pub fn data_dir_under(root: &Path) -> PathBuf {
    root.join(".wylde").join("data")
}

/// The on-disk store root for convention A. Env precedence:
/// `WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data`.
///
/// THE one resolver — do not copy this body; call it (see the module docs).
pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    data_dir_under(&wylde_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL gate (#138 crit 4): pin the exact fallback shape via the pure
    /// helper, so it goes RED if the resolver is ever stubbed to `.` or the
    /// `.wylde/data` layout drifts — unlike the pre-#138 `assert!(!p.is_empty())`
    /// fakes, which stayed green under any convention.
    #[test]
    fn data_dir_under_pins_the_dot_wylde_data_shape() {
        let root = Path::new("C:/wylde/estate-root");
        assert_eq!(
            data_dir_under(root),
            root.join(".wylde").join("data"),
            "convention A is <root>/.wylde/data"
        );
        let d = data_dir_under(root);
        // The tail is `.wylde/data`...
        assert!(d.ends_with(Path::new(".wylde").join("data")));
        // ...and the parent chain is the given root, NOT the cwd — the property
        // the old gate's name claimed but never checked.
        assert_eq!(d.parent().unwrap(), root.join(".wylde"));
        assert_eq!(d.parent().unwrap().parent().unwrap(), root);
    }
}
