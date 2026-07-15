//! Persistent peer registry — Rust port of `VPN/peers/store.py`.
//!
//! JSON-file backed, thread-safe via a process-global `Mutex`. The
//! on-disk layout (`{public_key: PeerRecord}` map) is byte-equivalent
//! with the Python store so a strangler-fig flip back to Python doesn't
//! lose state.
//!
//! Location: `<LINK_DATA_DIR>/peers.json`. The file is written via the
//! `tmp + rename` atomic pattern Python uses (a sibling `.tmp` is moved
//! over the target).

pub mod push;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A registered WyldeLink peer. Fields mirror the Python store's dict
/// keys exactly. Unknown fields are preserved on round-trip via the
/// `extra` catch-all so a future schema bump doesn't silently drop data.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeerRecord {
    pub public_key: String,
    #[serde(default)]
    pub label: String,
    pub tunnel_ip: String,
    #[serde(default)]
    pub registered_at: String,
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub allowed_services: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub struct PeerStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl PeerStore {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        Self {
            path: data_dir.as_ref().join("peers.json"),
            lock: Mutex::new(()),
        }
    }

    /// Read-only snapshot. Empty list if the file is missing or
    /// unparseable (matches Python's "fail soft" behaviour).
    pub fn list(&self) -> Vec<PeerRecord> {
        let _g = self.lock.lock().expect("peer store lock poisoned");
        load(&self.path)
            .map(|m| m.into_values().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, public_key: &str) -> Option<PeerRecord> {
        let _g = self.lock.lock().expect("peer store lock poisoned");
        load(&self.path)
            .ok()
            .and_then(|m| m.get(public_key).cloned())
    }

    pub fn upsert(&self, peer: PeerRecord) -> Result<()> {
        let _g = self.lock.lock().expect("peer store lock poisoned");
        let mut peers = load(&self.path).unwrap_or_default();
        peers.insert(peer.public_key.clone(), peer);
        save(&self.path, &peers)
    }

    pub fn remove(&self, public_key: &str) -> Result<bool> {
        let _g = self.lock.lock().expect("peer store lock poisoned");
        let mut peers = load(&self.path).unwrap_or_default();
        let removed = peers.remove(public_key).is_some();
        if removed {
            save(&self.path, &peers)?;
        }
        Ok(removed)
    }

    /// Lowest available `/24` address in `192.0.2.0/24` (matches the
    /// Python store's hardcoded subnet — this will move to config when
    /// the full peer-management actions land).
    pub fn next_tunnel_ip(&self) -> Option<String> {
        let _g = self.lock.lock().expect("peer store lock poisoned");
        let peers = load(&self.path).unwrap_or_default();
        let used: std::collections::HashSet<&str> =
            peers.values().map(|p| p.tunnel_ip.as_str()).collect();
        (2..254u8)
            .map(|i| format!("192.0.2.{i}"))
            .find(|ip| !used.contains(ip.as_str()))
    }
}

fn load(path: &Path) -> Result<std::collections::HashMap<String, PeerRecord>> {
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("peer store: read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("peer store: parse {}", path.display()))
}

fn save(path: &Path, peers: &std::collections::HashMap<String, PeerRecord>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("peer store: create dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec_pretty(peers).context("peer store: serialize")?;
    std::fs::write(&tmp, &body).with_context(|| format!("peer store: write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("peer store: rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, PeerStore) {
        let dir = TempDir::new().unwrap();
        let store = PeerStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn empty_store_returns_empty_list() {
        let (_dir, store) = fresh_store();
        assert!(store.list().is_empty());
    }

    #[test]
    fn upsert_and_get_round_trip() {
        let (_dir, store) = fresh_store();
        let peer = PeerRecord {
            public_key: "abc=".to_string(),
            label: "phone".to_string(),
            tunnel_ip: "192.0.2.2".to_string(),
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            last_seen: None,
            allowed_services: vec!["chat".to_string()],
            extra: serde_json::Map::new(),
        };
        store.upsert(peer.clone()).unwrap();
        let got = store.get("abc=").expect("peer round-trips");
        assert_eq!(got.label, "phone");
        assert_eq!(got.tunnel_ip, "192.0.2.2");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn remove_returns_true_only_when_present() {
        let (_dir, store) = fresh_store();
        assert!(!store.remove("missing").unwrap());

        let peer = PeerRecord {
            public_key: "xyz=".to_string(),
            tunnel_ip: "192.0.2.3".to_string(),
            ..Default::default()
        };
        store.upsert(peer).unwrap();
        assert!(store.remove("xyz=").unwrap());
        assert!(store.get("xyz=").is_none());
    }

    #[test]
    fn next_tunnel_ip_skips_used() {
        let (_dir, store) = fresh_store();
        // Empty → first available is .2.
        assert_eq!(store.next_tunnel_ip().as_deref(), Some("192.0.2.2"));

        // After .2 is taken, the next free address is .3.
        store
            .upsert(PeerRecord {
                public_key: "k=".to_string(),
                tunnel_ip: "192.0.2.2".to_string(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.next_tunnel_ip().as_deref(), Some("192.0.2.3"));
    }
}
