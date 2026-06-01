//! Soft LRU keep-warm hints for recently-leased (service, model) pairs.
//!
//! Rust port of `Core/resource_monitor/broker/model_cache.py`. The cache does
//! not own VRAM; it only remembers which models were recently leased so that:
//!   * repeated reserves for the same (service, model) can re-use the previous
//!     lease's accounting if the original is still live;
//!   * eviction picks lower-priority *or* less-recently-used victims when ties
//!     in priority occur.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::time::now_secs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCacheEntry {
    pub service: String,
    pub model: String,
    pub bytes: u64,
    pub last_used: f64,
    #[serde(default)]
    pub last_priority: i32,
}

pub struct ModelCache {
    ttl_s: f64,
    entries: Mutex<HashMap<String, ModelCacheEntry>>,
}

impl ModelCache {
    pub fn new(ttl_s: f64) -> Self {
        Self {
            ttl_s,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn ttl_s(&self) -> f64 {
        self.ttl_s
    }

    fn key(service: &str, model: &str) -> String {
        format!("{service}:{model}")
    }

    pub fn touch(&self, service: &str, model: &str, nbytes: u64, priority: i32) {
        let mut g = self.entries.lock().expect("model cache poisoned");
        g.insert(
            Self::key(service, model),
            ModelCacheEntry {
                service: service.to_owned(),
                model: model.to_owned(),
                bytes: nbytes,
                last_used: now_secs(),
                last_priority: priority,
            },
        );
    }

    pub fn last_used(&self, service: &str, model: &str) -> f64 {
        let g = self.entries.lock().expect("model cache poisoned");
        g.get(&Self::key(service, model))
            .map(|e| e.last_used)
            .unwrap_or(0.0)
    }

    pub fn warm_for(&self, service: &str, model: &str) -> bool {
        let g = self.entries.lock().expect("model cache poisoned");
        match g.get(&Self::key(service, model)) {
            None => false,
            Some(e) => (now_secs() - e.last_used) < self.ttl_s,
        }
    }

    pub fn all(&self) -> Vec<ModelCacheEntry> {
        let cutoff = now_secs() - self.ttl_s;
        let g = self.entries.lock().expect("model cache poisoned");
        g.values()
            .filter(|e| e.last_used >= cutoff)
            .cloned()
            .collect()
    }

    pub fn prune(&self) -> usize {
        let cutoff = now_secs() - self.ttl_s;
        let mut g = self.entries.lock().expect("model cache poisoned");
        let stale: Vec<String> = g
            .iter()
            .filter(|(_, e)| e.last_used < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let n = stale.len();
        for k in stale {
            g.remove(&k);
        }
        n
    }

    /// Test-only: clear in place. Keeps the global pointer stable across
    /// resets so callers that captured a `&'static ModelCache` stay valid.
    pub fn reset(&self) {
        let mut g = self.entries.lock().expect("model cache poisoned");
        g.clear();
    }
}

/// Process-wide model cache. Lazy-init so `Config::get` (which it depends on)
/// is also resolved lazily.
pub fn model_cache() -> &'static ModelCache {
    static MC: OnceLock<ModelCache> = OnceLock::new();
    MC.get_or_init(|| ModelCache::new(crate::config::Config::get().model_cache_ttl_s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_and_lookup() {
        let mc = ModelCache::new(1800.0);
        assert!(!mc.warm_for("svc", "m"));
        mc.touch("svc", "m", 100, 40);
        assert!(mc.warm_for("svc", "m"));
        let entries = mc.all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].service, "svc");
        assert_eq!(entries[0].bytes, 100);
    }

    #[test]
    fn prune_drops_stale() {
        let mc = ModelCache::new(0.0001);
        mc.touch("svc", "m", 100, 40);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let pruned = mc.prune();
        assert_eq!(pruned, 1);
        assert_eq!(mc.all().len(), 0);
    }
}
