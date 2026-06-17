//! Per-service user-data path store (`paths.get` / `paths.set`).
//!
//! Out-of-tree runtime foundation, the data-path half (locked decision 4;
//! plan §3). A data-owning service learns *where its user data lives*
//! without that path ever living inside the service folder, and the path
//! **outlives the binary** (a swap/reinstall keeps the library).
//!
//! This is a clone of [`crate::updater_prefs`]'s shape — a tiny Core-owned
//! JSON prefs file under the same data root (`WYLDE_DATA_DIR` →
//! `WYLDE_ROOT/.wylde/data/`), atomic temp-write + rename, defaults on any
//! read error — keyed by service name → `{ data_dir }`. Because the store
//! lives in **Core config, not in the service**, the path survives a binary
//! swap.
//!
//! ## The contract (generic; Images is the first user)
//!
//! 1. **Persisted store** — `service_paths.json`, keyed by canonical
//!    service name (e.g. `wylde-images`).
//! 2. **Default = a sibling of the Core repo** — when no entry exists,
//!    [`default_data_dir`] is `<root-parent>/WyldeData/<svc>/` (a sibling
//!    of `WYLDE_ROOT`, never inside `Services/<svc>/`), so user data leaves
//!    the repo tree entirely.
//! 3. **First-open picker** — the GUI writes the chosen folder via
//!    `paths.set`; if dismissed, the service falls back to the default
//!    (step 2) so it is never blocked. (Flow only — no GUI here.)
//! 4. **Injection** — at spawn the daemon injects
//!    [`data_dir_env_name`] (`WYLDE_<SVC>_DATA_DIR`) =
//!    [`resolve_data_dir`] into every child (see
//!    `state::services::spawn_rust_binary`). A data-owning service reads
//!    that env var in place of any hardcoded path; a path change takes
//!    effect on the next service bounce.
//!
//! Nothing here is Images-specific except, eventually, the one env-var the
//! service reads. The store, default-sibling rule, and injection are the
//! reusable contract for any future data-owning service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One persisted per-service entry. A struct (rather than a bare string)
/// so the store can grow forward-compatibly (quota, last-opened, …)
/// without a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePathEntry {
    /// Absolute path to the service's user-data library.
    pub data_dir: String,
}

/// The whole store: canonical service name → entry. Absent name ⇒ use the
/// computed default ([`default_data_dir`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePaths {
    #[serde(default)]
    pub services: BTreeMap<String, ServicePathEntry>,
}

impl ServicePaths {
    /// Serialise to the wire/JSON object the GUI parses.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({ "services": {} }))
    }

    /// The persisted override for `service`, if any (no default applied).
    pub fn get(&self, service: &str) -> Option<&str> {
        self.services
            .get(service)
            .map(|e| e.data_dir.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Set (or replace) the override for `service`.
    pub fn set(&mut self, service: &str, data_dir: &str) {
        self.services.insert(
            service.to_owned(),
            ServicePathEntry {
                data_dir: data_dir.to_owned(),
            },
        );
    }
}

/// Resolve the store file path. Honours the same env-var ladder the rest
/// of the data layer uses (`WYLDE_DATA_DIR` →
/// `WYLDE_ROOT/.wylde/data/service_paths.json`) so every component reads
/// one location — identical to [`crate::updater_prefs::prefs_path`].
pub fn store_path() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p.join("service_paths.json");
        }
    }
    wylde_root()
        .join(".wylde")
        .join("data")
        .join("service_paths.json")
}

/// Load the store, returning defaults on any error (missing file, bad JSON).
pub fn load() -> ServicePaths {
    load_at(&store_path())
}

pub fn load_at(path: &Path) -> ServicePaths {
    let Ok(bytes) = std::fs::read(path) else {
        return ServicePaths::default();
    };
    match serde_json::from_slice::<ServicePaths>(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "wylde-lifecycle: service_paths unreadable at {} ({e}); using defaults",
                path.display()
            );
            ServicePaths::default()
        }
    }
}

/// Persist the store atomically (temp-write + rename).
pub fn save(store: &ServicePaths) -> std::io::Result<()> {
    save_at(store, &store_path())
}

pub fn save_at(store: &ServicePaths, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(store).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// The default data dir for `service`: `<root-parent>/WyldeData/<stripped>/`
/// — a sibling of the Core repo (`WYLDE_ROOT`), so user data never lives in
/// the repo tree or the service folder. `<stripped>` drops the `wylde-`
/// prefix (`wylde-images` → `images`), matching the env-var name and the
/// pre-extraction `data/images` layout. Falls back to `<root>/WyldeData/…`
/// only when the root has no parent (a filesystem root).
pub fn default_data_dir(service: &str) -> PathBuf {
    let stripped = service.strip_prefix("wylde-").unwrap_or(service);
    let root = wylde_root_abs();
    let base = root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.clone());
    base.join("WyldeData").join(stripped)
}

/// The data dir to inject for `service`: the persisted override if set,
/// else [`default_data_dir`]. The single resolver the spawn path and the
/// `paths.get` action both call.
pub fn resolve_data_dir(service: &str) -> PathBuf {
    if let Some(over) = load().get(service) {
        return PathBuf::from(over);
    }
    default_data_dir(service)
}

/// The env-var name a data-owning service reads for its library:
/// `WYLDE_<SVC>_DATA_DIR`, where `<SVC>` is the `wylde-`-stripped name,
/// uppercased, dashes→underscores (`wylde-images` →
/// `WYLDE_IMAGES_DATA_DIR`). Matches the plan's §3 example exactly.
pub fn data_dir_env_name(service: &str) -> String {
    let stripped = service.strip_prefix("wylde-").unwrap_or(service);
    format!("WYLDE_{}_DATA_DIR", stripped.to_uppercase().replace('-', "_"))
}

fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolute `WYLDE_ROOT` (so `default_data_dir`'s `.parent()` is
/// meaningful even when the launcher leaves `WYLDE_ROOT` relative).
fn wylde_root_abs() -> PathBuf {
    let root = wylde_root();
    if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&root))
            .unwrap_or(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_loads_default_empty_store() {
        let td = TempDir::new().unwrap();
        let s = load_at(&td.path().join("nope.json"));
        assert_eq!(s, ServicePaths::default());
        assert!(s.services.is_empty());
    }

    #[test]
    fn set_get_and_round_trip_through_disk() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("service_paths.json");
        let mut s = ServicePaths::default();
        s.set("wylde-images", "D:/Pictures/WyldeLibrary");
        assert_eq!(s.get("wylde-images"), Some("D:/Pictures/WyldeLibrary"));
        assert_eq!(s.get("wylde-notes"), None);
        save_at(&s, &path).unwrap();
        let back = load_at(&path);
        assert_eq!(s, back);
    }

    #[test]
    fn empty_override_is_treated_as_unset() {
        let mut s = ServicePaths::default();
        s.set("wylde-images", "");
        assert_eq!(s.get("wylde-images"), None);
    }

    #[test]
    fn default_data_dir_is_sibling_of_root() {
        // With an absolute root, the default is <root-parent>/WyldeData/<svc>
        // — outside the repo tree, never inside Services/<svc>/.
        std::env::set_var("WYLDE_ROOT", r"C:\wylde\Wylde-release");
        let d = default_data_dir("wylde-images");
        std::env::remove_var("WYLDE_ROOT");
        assert!(
            d.ends_with(Path::new("WyldeData").join("images")),
            "expected .../WyldeData/images, got {d:?}"
        );
        // The parent of the data dir's WyldeData is the root's parent.
        let wylde_data = d.parent().unwrap();
        assert_eq!(wylde_data.file_name().unwrap(), "WyldeData");
        assert_eq!(
            wylde_data.parent().unwrap(),
            Path::new(r"C:\wylde")
        );
    }

    #[test]
    fn resolve_prefers_override_else_default() {
        let td = TempDir::new().unwrap();
        // Point the store ladder at the tempdir and seed an override.
        std::env::set_var("WYLDE_DATA_DIR", td.path());
        let mut s = ServicePaths::default();
        s.set("wylde-images", "E:/CustomLib");
        save(&s).unwrap();
        assert_eq!(resolve_data_dir("wylde-images"), PathBuf::from("E:/CustomLib"));
        // A service with no override falls through to the default sibling.
        let notes = resolve_data_dir("wylde-notes");
        std::env::remove_var("WYLDE_DATA_DIR");
        assert!(notes.ends_with(Path::new("WyldeData").join("notes")));
    }

    #[test]
    fn data_dir_env_name_strips_and_uppercases() {
        assert_eq!(data_dir_env_name("wylde-images"), "WYLDE_IMAGES_DATA_DIR");
        assert_eq!(
            data_dir_env_name("wylde-foo-bar"),
            "WYLDE_FOO_BAR_DATA_DIR"
        );
        // A name without the prefix is uppercased as-is.
        assert_eq!(data_dir_env_name("images"), "WYLDE_IMAGES_DATA_DIR");
    }
}
