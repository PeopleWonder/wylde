//! Per-verb read-through TTL cache (scope v2 §7.6).
//!
//! A cache hit within TTL skips the pipe entirely (this is also how the
//! §2.5 cached-`symbols.find` budget of <20ms is met). TTLs are per-verb
//! and declared in the [`crate::verbs`] table (`list_mru` 30s, graph 5s,
//! `symbols.find` 60s, `anchors.list` 30s); verbs with no TTL (writes,
//! `ping`) never cache.
//!
//! Entries are keyed by `verb` + the request payload, so different
//! arguments to the same verb cache independently.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug, Clone)]
struct Entry {
    value: Value,
    expires_at: Instant,
}

/// In-memory, TTL-bounded, read-through cache shared by one client.
#[derive(Debug, Default)]
pub struct VerbCache {
    entries: Mutex<HashMap<String, Entry>>,
}

impl VerbCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Build a stable cache key from a verb + its request payload.
    pub fn key(verb: &str, payload: &Value) -> String {
        // `Value`'s Display is stable enough for a process-local key; object
        // key ordering is preserved by serde_json's default (insertion), and
        // we only ever compare a key to itself within a run.
        format!("{verb}:{payload}")
    }

    /// Fetch a non-expired entry, if present. Expired entries are evicted
    /// lazily on read.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut entries = self.entries.lock().expect("cache poisoned");
        match entries.get(key) {
            Some(e) if e.expires_at > Instant::now() => Some(e.value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Store a value under `key` with a `ttl` lifetime.
    pub fn put(&self, key: String, value: Value, ttl: Duration) {
        let mut entries = self.entries.lock().expect("cache poisoned");
        entries.insert(
            key,
            Entry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Drop every cached entry. Consumers call this on a `Broken`→reconnect
    /// transition so a stale render doesn't outlive a service restart.
    pub fn clear(&self) {
        self.entries.lock().expect("cache poisoned").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hit_within_ttl() {
        let c = VerbCache::new();
        let k = VerbCache::key("graph", &json!({"ws": 1}));
        c.put(k.clone(), json!({"nodes": []}), Duration::from_secs(5));
        assert_eq!(c.get(&k), Some(json!({"nodes": []})));
    }

    #[test]
    fn miss_after_expiry() {
        let c = VerbCache::new();
        let k = VerbCache::key("graph", &json!(null));
        c.put(k.clone(), json!(1), Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(c.get(&k), None);
    }

    #[test]
    fn distinct_payloads_cache_separately() {
        let c = VerbCache::new();
        let k1 = VerbCache::key("symbols.find", &json!({"q": "a"}));
        let k2 = VerbCache::key("symbols.find", &json!({"q": "b"}));
        c.put(k1.clone(), json!(["a"]), Duration::from_secs(60));
        assert_eq!(c.get(&k1), Some(json!(["a"])));
        assert_eq!(c.get(&k2), None);
    }

    #[test]
    fn clear_drops_everything() {
        let c = VerbCache::new();
        let k = VerbCache::key("graph", &json!(null));
        c.put(k.clone(), json!(1), Duration::from_secs(5));
        c.clear();
        assert_eq!(c.get(&k), None);
    }
}
