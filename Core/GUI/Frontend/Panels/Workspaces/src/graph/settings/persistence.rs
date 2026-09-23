//! `graph_profiles.json` load/save (Slice C-settings, Plan v2 §10).
//!
//! The library lives at `<data_dir>/graph_profiles.json`, where `<data_dir>`
//! resolves exactly the way every Wylde service resolves it (convention A —
//! the canonical `wylde_shared::paths::data_dir`: `WYLDE_DATA_DIR` → `DATA_DIR`
//! → `<WYLDE_ROOT>/.wylde/data`). This is a **sanctioned copy** of that body:
//! the GUI panel deliberately doesn't link the service crates (Build Order: the
//! GUI's only backend dependency is the pipe), so it can't `use` the shared
//! resolver without an approved dependency addition. #138 unified the six
//! rust/crates copies onto the shared resolver and gates against new ones;
//! folding this last copy in is tracked there and needs that dep decision.
//!
//! Stored as plain JSON, deliberately **outside** the OI-14 encryption sweep:
//! profiles are visual preferences (zoom feel, layout choice), not user
//! data, and a hand-editable file is a feature for feel-testing. Writes are
//! atomic (temp file + rename); a missing or corrupt file loads as the
//! default library so the panel never fails to mount.

use std::path::{Path, PathBuf};

use super::profiles::ProfileLibrary;

/// `<data_dir>` per the Wylde-wide convention (see module docs).
fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join(".wylde").join("data")
}

/// The canonical library path: `<data_dir>/graph_profiles.json`.
pub fn profiles_path() -> PathBuf {
    data_dir().join("graph_profiles.json")
}

/// Load the library from `path`. Missing file → the default library;
/// malformed file → the default library plus the parse error (surfaced so
/// the Settings tab can tell the user their hand-edit didn't take, instead
/// of silently shadowing it).
pub fn load_from(path: &Path) -> (ProfileLibrary, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<ProfileLibrary>(&text) {
            Ok(mut lib) => {
                lib.ensure_default();
                (lib, None)
            }
            Err(e) => (
                ProfileLibrary::with_default(),
                Some(format!("graph_profiles.json parse error: {e}")),
            ),
        },
        Err(_) => (ProfileLibrary::with_default(), None),
    }
}

/// Save the library to `path` atomically (write a sibling temp file, then
/// rename over the target). Parent directories are created as needed.
pub fn save_to(path: &Path, lib: &ProfileLibrary) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(lib).map_err(|e| format!("serialize graph_profiles: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    // Windows rename does not overwrite — remove the target first (the
    // worst-case crash window leaves the .tmp beside an intact original).
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::cluster::ClusterConfig;
    use crate::graph::layout::LayoutKind;
    use crate::graph::navigation::NavConfig;
    use crate::graph::settings::profiles::{GraphProfile, DEFAULT_PROFILE};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("wylde-graph-profiles-tests") // graph profiles dir name, not the dead memgraph service (wylde-check: dead-ref-ok)
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("graph_profiles.json")
    }

    #[test]
    fn missing_file_loads_default_library() {
        let path = temp_path("missing");
        let (lib, err) = load_from(&path);
        assert!(err.is_none());
        assert!(lib.get(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn save_load_round_trip() {
        let path = temp_path("roundtrip");
        let mut lib = ProfileLibrary::with_default();
        lib.upsert(GraphProfile::capture(
            "Focus",
            LayoutKind::StableGrid,
            ClusterConfig {
                target_visible_nodes: 99,
                ..ClusterConfig::default()
            },
            NavConfig::default(),
            false,
        ));
        lib.set_pointer("ws-1", "Focus");
        save_to(&path, &lib).unwrap();

        let (back, err) = load_from(&path);
        assert!(err.is_none());
        assert_eq!(back, lib);
        assert_eq!(back.pointer("ws-1"), Some("Focus"));

        // Saving again overwrites in place (the Windows remove+rename path).
        save_to(&path, &back).unwrap();
        let (again, _) = load_from(&path);
        assert_eq!(again, lib);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_surfaces_error_and_falls_back() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let (lib, err) = load_from(&path);
        assert!(err.is_some(), "parse error surfaced");
        assert!(lib.get(DEFAULT_PROFILE).is_some(), "default fallback");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// #138 — a real path-shape gate (was `ends_with("graph_profiles.json")`,
    /// true by construction under any `data_dir()`). Pins that the library
    /// lands under the resolved data dir, and — when no data-dir override is set
    /// on the runner — that the convention-A `.wylde/data` tail holds. A
    /// regression of `data_dir()` to `.` makes the clean-env branch red.
    #[test]
    fn profiles_path_lands_in_data_dir() {
        let p = profiles_path();
        assert!(p.ends_with("graph_profiles.json"));
        // When no data-dir override is set on the runner, the convention-A tail
        // must hold — a regression of `data_dir()` to `.` makes this red.
        // (Read-only env check, so a concurrent test can't perturb it.)
        if std::env::var_os("WYLDE_DATA_DIR").is_none() && std::env::var_os("DATA_DIR").is_none() {
            assert!(p.ends_with(Path::new(".wylde").join("data").join("graph_profiles.json")));
        }
    }
}
