//! JSON-backed device record store + tier constants.
//!
//! Rust port of `device_gate/store.py`. One file (`devices.json`) holds every
//! paired device; writes are atomic (tmpfile + rename) so a crash mid-write
//! doesn't corrupt the store. The Python and Rust implementations share the
//! same file format, so a live cutover via the strangler-fig flag doesn't
//! invalidate existing pairings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Tiers ──────────────────────────────────────────────────────────────

/// Default tier assigned at pairing time. View-only access; cannot invoke tools.
pub const TIER_READ_ONLY: &str = "read_only";
/// Mid tier. Non-destructive tool calls (read / search / retrieve).
pub const TIER_TOOL_USE: &str = "tool_use";
/// Full surface, including write / delete / execute.
pub const TIER_DESTRUCTIVE: &str = "destructive_tool_access";

/// Tier list in rank order. Matches Python's `ALL_TIERS`.
pub const ALL_TIERS: [&str; 3] = [TIER_READ_ONLY, TIER_TOOL_USE, TIER_DESTRUCTIVE];

/// Numeric rank for "is tier X >= tier Y" comparisons at the call site.
pub fn tier_rank(tier: &str) -> i32 {
    match tier {
        TIER_READ_ONLY => 0,
        TIER_TOOL_USE => 1,
        TIER_DESTRUCTIVE => 2,
        _ => -1,
    }
}

pub fn is_valid_tier(tier: &str) -> bool {
    ALL_TIERS.contains(&tier)
}

// ── Device record ──────────────────────────────────────────────────────

/// One paired device. Wire shape (the JSON written into `devices.json`)
/// matches Python's `Device.to_dict(include_token=True)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub device_id: String,
    pub name: String,
    pub token: String,
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(default)]
    pub paired_at: f64,
    #[serde(default)]
    pub last_seen: f64,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

fn default_tier() -> String {
    TIER_READ_ONLY.to_string()
}

impl Device {
    /// GUI-facing dict — drops the token field. Mirrors Python's
    /// `Device.to_dict(include_token=False)`.
    pub fn to_public_value(&self) -> Value {
        serde_json::json!({
            "device_id": self.device_id,
            "name": self.name,
            "tier": self.tier,
            "paired_at": self.paired_at,
            "last_seen": self.last_seen,
            "metadata": self.metadata,
        })
    }
}

// ── Store ──────────────────────────────────────────────────────────────

/// On-disk envelope: `{"devices": [Device, ...]}`. Kept as its own struct
/// so the JSON shape matches Python exactly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DeviceFile {
    #[serde(default)]
    devices: Vec<Device>,
}

/// Thread-safe device-record store. One JSON file on disk.
///
/// Each operation re-reads the file (matching the Python store's behaviour
/// where the in-memory cache is the file itself). This keeps multi-process
/// safety simple: the only shared state is the file, and writes are atomic.
pub struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl DeviceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Vec<Device> {
        if !self.path.exists() {
            return Vec::new();
        }
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        // Accept both wrapped (`{"devices": [...]}`) and bare-array shapes so
        // any historic file layout still loads. Python's loader has the same
        // bivalent reader.
        if let Ok(file) = serde_json::from_str::<DeviceFile>(&raw) {
            return file.devices;
        }
        if let Ok(arr) = serde_json::from_str::<Vec<Device>>(&raw) {
            return arr;
        }
        Vec::new()
    }

    fn save(&self, devices: &[Device]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = DeviceFile {
            devices: devices.to_vec(),
        };
        let body = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        // Live device bearer tokens — restrict to owner-only access.
        // Fail-soft: a hardening failure must not fail the write.
        let _ = wylde_shared::secure_file::harden_perms(&self.path);
        Ok(())
    }

    // ── Read-side ─────────────────────────────────────────────────────

    pub fn list(&self) -> Vec<Device> {
        let _g = self.lock.lock().expect("device store poisoned");
        self.load()
    }

    pub fn get(&self, device_id: &str) -> Option<Device> {
        let _g = self.lock.lock().expect("device store poisoned");
        self.load().into_iter().find(|d| d.device_id == device_id)
    }

    pub fn find_by_token(&self, token: &str) -> Option<Device> {
        if token.is_empty() {
            return None;
        }
        let _g = self.lock.lock().expect("device store poisoned");
        self.load().into_iter().find(|d| d.token == token)
    }

    // ── Write-side ────────────────────────────────────────────────────

    /// Append a fresh device record. Returns an error if `device_id`
    /// already exists — the caller is expected to mint a unique id.
    pub fn add(&self, device: Device) -> Result<Device, StoreError> {
        let _g = self.lock.lock().expect("device store poisoned");
        let mut devices = self.load();
        if devices.iter().any(|d| d.device_id == device.device_id) {
            return Err(StoreError::Duplicate(device.device_id));
        }
        devices.push(device.clone());
        self.save(&devices).map_err(StoreError::Io)?;
        Ok(device)
    }

    /// Apply `updater` to the matching record. Returns the updated copy
    /// or `None` if the device wasn't found. Mirrors Python's `update`,
    /// which took `**fields` kwargs; the closure form is the Rust idiom.
    pub fn update<F>(&self, device_id: &str, updater: F) -> Option<Device>
    where
        F: FnOnce(&mut Device),
    {
        let _g = self.lock.lock().expect("device store poisoned");
        let mut devices = self.load();
        let target = devices.iter_mut().find(|d| d.device_id == device_id)?;
        updater(target);
        let updated = target.clone();
        let _ = self.save(&devices);
        Some(updated)
    }

    pub fn remove(&self, device_id: &str) -> bool {
        let _g = self.lock.lock().expect("device store poisoned");
        let mut devices = self.load();
        let before = devices.len();
        devices.retain(|d| d.device_id != device_id);
        if devices.len() == before {
            return false;
        }
        let _ = self.save(&devices);
        true
    }

    /// Update `last_seen`. Missing device_id is a no-op so a stale token
    /// check doesn't crash. Matches Python's `touch`.
    pub fn touch(&self, device_id: &str, when: f64) {
        let _g = self.lock.lock().expect("device store poisoned");
        let mut devices = self.load();
        let mut changed = false;
        for d in &mut devices {
            if d.device_id == device_id {
                d.last_seen = when;
                changed = true;
                break;
            }
        }
        if changed {
            let _ = self.save(&devices);
        }
    }
}

/// Errors returned by [`DeviceStore::add`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("device_id {0:?} already exists")]
    Duplicate(String),
    #[error("device store IO: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, DeviceStore) {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("devices.json");
        (tmp, DeviceStore::new(path))
    }

    fn make_device(id: &str, token: &str) -> Device {
        Device {
            device_id: id.into(),
            name: format!("name-{id}"),
            token: token.into(),
            tier: TIER_READ_ONLY.into(),
            paired_at: 1_000_000.0,
            last_seen: 1_000_000.0,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn add_get_remove_roundtrip() {
        let (_tmp, store) = fresh_store();
        store.add(make_device("d1", "t1")).unwrap();
        assert!(store.get("d1").is_some());
        assert_eq!(store.find_by_token("t1").unwrap().device_id, "d1");
        assert!(store.remove("d1"));
        assert!(store.get("d1").is_none());
        assert!(store.find_by_token("t1").is_none());
    }

    #[test]
    fn add_rejects_duplicate_device_id() {
        let (_tmp, store) = fresh_store();
        store.add(make_device("d1", "t1")).unwrap();
        let err = store.add(make_device("d1", "t2"));
        assert!(matches!(err, Err(StoreError::Duplicate(_))));
    }

    #[test]
    fn update_changes_fields() {
        let (_tmp, store) = fresh_store();
        store.add(make_device("d1", "t1")).unwrap();
        let updated = store
            .update("d1", |d| d.tier = TIER_TOOL_USE.into())
            .unwrap();
        assert_eq!(updated.tier, TIER_TOOL_USE);
        assert_eq!(store.get("d1").unwrap().tier, TIER_TOOL_USE);
    }

    #[test]
    fn update_returns_none_for_missing() {
        let (_tmp, store) = fresh_store();
        assert!(store
            .update("nope", |d| d.tier = TIER_TOOL_USE.into())
            .is_none());
    }

    #[test]
    fn touch_updates_last_seen() {
        let (_tmp, store) = fresh_store();
        store.add(make_device("d1", "t1")).unwrap();
        store.touch("d1", 2_000_000.0);
        assert_eq!(store.get("d1").unwrap().last_seen, 2_000_000.0);
    }

    #[test]
    fn touch_missing_is_noop() {
        let (_tmp, store) = fresh_store();
        store.touch("nope", 999.0); // must not panic.
    }

    #[test]
    fn missing_file_returns_empty_list() {
        let (_tmp, store) = fresh_store();
        assert!(store.list().is_empty());
    }

    #[test]
    fn parses_python_wrapped_format() {
        let (tmp, store) = fresh_store();
        let path = tmp.path().join("devices.json");
        std::fs::write(
            &path,
            r#"{"devices": [{"device_id": "x", "name": "n", "token": "t", "tier": "tool_use", "paired_at": 1.0, "last_seen": 2.0, "metadata": {}}]}"#,
        )
        .unwrap();
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tier, "tool_use");
    }

    #[test]
    fn tier_helpers_align_with_python() {
        assert_eq!(tier_rank(TIER_READ_ONLY), 0);
        assert_eq!(tier_rank(TIER_TOOL_USE), 1);
        assert_eq!(tier_rank(TIER_DESTRUCTIVE), 2);
        assert_eq!(tier_rank("nope"), -1);
        assert!(is_valid_tier(TIER_TOOL_USE));
        assert!(!is_valid_tier("superuser"));
    }
}
