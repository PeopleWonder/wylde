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
//! Scope note (#250): the four resolvers #138 deferred — model selection,
//! routing / model registry, per-model Ollama overrides, and the device gate —
//! now root here too. Each keeps its own env override as a test seam and
//! operator escape hatch, and each adopts its legacy location on first touch
//! via [`crate::data_migration`], so an upgrade does not silently reset the
//! user's starred default, inference overrides, routing profiles, or paired
//! devices. `docs/data-roots.md` is the one table of store → canonical path →
//! env override → legacy path.
//!
//! What still lives under the legacy sibling root [`legacy_data_dir`]
//! (`<ROOT>/data`) and is deliberately NOT in #250's remit: the service
//! manifest dir (`manifest.rs`), the consent store (`tooling/consent.rs`), and
//! the VRAM-broker state file (`wylde-vram-broker`). Those are root-anchored
//! already — they carry neither of #250's two hazards — and none is named in
//! its acceptance criteria; see `docs/data-roots.md` §"Not yet unified".

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

/// The **legacy** sibling data root under an explicit root: `<root>/data`.
///
/// This is where the four resolvers #250 unified used to land — either by
/// naming it outright (`ollama_overrides`) or by falling back to a *cwd*-
/// relative `"data"` that lifecycle happened to pin at `WYLDE_ROOT`
/// (`model_state`, `routing`). Nothing new may root here; it exists so the
/// migration in [`crate::data_migration`] can find an existing install's
/// bytes, and so a test can name the pre-#250 layout without re-deriving it.
pub fn legacy_data_dir_under(root: &Path) -> PathBuf {
    root.join("data")
}

/// [`legacy_data_dir_under`] resolved against [`wylde_root`].
///
/// Deliberately **not** env-overridable: `DATA_DIR` and friends point at the
/// *canonical* root, and a legacy path that moved with them would make the
/// migration a self-copy rather than a rescue.
pub fn legacy_data_dir() -> PathBuf {
    legacy_data_dir_under(&wylde_root())
}

/// The device gate's legacy root: `<root>/device_gate/data`. The one legacy
/// location that is not a `<root>/data` child, kept here so the gate crate
/// does not re-derive a root of its own (#250).
pub fn legacy_device_gate_dir_under(root: &Path) -> PathBuf {
    root.join("device_gate").join("data")
}

/// [`legacy_device_gate_dir_under`] resolved against [`wylde_root`].
pub fn legacy_device_gate_dir() -> PathBuf {
    legacy_device_gate_dir_under(&wylde_root())
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

    /// #250: the legacy roots are pinned to the SAME explicit root as
    /// convention A — the migration's whole job is to move bytes between two
    /// siblings, so if these ever resolved against different roots (or against
    /// the cwd) it would copy from nowhere and report success.
    #[test]
    fn legacy_roots_are_siblings_of_convention_a_under_one_root() {
        let root = Path::new("C:/wylde/estate-root");
        assert_eq!(legacy_data_dir_under(root), root.join("data"));
        assert_eq!(
            legacy_device_gate_dir_under(root),
            root.join("device_gate").join("data")
        );
        // Legacy and canonical are DISTINCT — a migration that no-ops because
        // source == destination would silently lose the user's data.
        assert_ne!(legacy_data_dir_under(root), data_dir_under(root));
        assert_ne!(legacy_device_gate_dir_under(root), data_dir_under(root));
        // ...and both hang off the given root, never the process cwd.
        assert!(legacy_data_dir_under(root).starts_with(root));
        assert!(legacy_device_gate_dir_under(root).starts_with(root));
    }
}
