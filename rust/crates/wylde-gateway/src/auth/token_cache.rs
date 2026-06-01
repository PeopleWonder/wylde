//! Async-mutex token cache for device-token verification.
//!
//! `require_device` resolves every `Authorization: Bearer <token>` header
//! against the device-gate pipe. That is a round-trip per request; this
//! cache collapses repeat lookups of the same token within a 60-second
//! window so a chatty mobile client doesn't hammer device-gate.
//!
//! The cache is the standalone extraction of the verification-result
//! caching that conceptually belongs next to `services::device_gate` —
//! kept as its own module so the auth tier owns its cache rather than
//! threading it through the pipe wrapper.
//!
//! Shape: `Arc<Mutex<HashMap<token, (Device, expires_at)>>>` with a 60s
//! TTL. A stale entry is evicted the moment it is read — there is no
//! background sweeper, so the map only ever holds tokens seen since the
//! last read of each slot.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Time-to-live for a cached device record. Past this window the entry
/// is treated as a miss and re-verified against device-gate.
pub const TOKEN_TTL: Duration = Duration::from_secs(60);

/// A verified device. The Bearer token is the cache key, so it is not
/// duplicated inside the value. Mirrors the `{device_id, tier}` subset of
/// Python's `Gateway/auth/device.py::DeviceAuth` that the Gateway needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub device_id: String,
    pub tier: String,
}

/// One cache slot: the device plus the instant the slot goes stale.
struct Entry {
    device: Device,
    expires_at: Instant,
}

/// Async-mutex token cache. Cloning shares the backing map (it is an
/// `Arc` inside), so the process-wide [`global`] instance and any clone
/// observe the same entries.
#[derive(Clone, Default)]
pub struct TokenCache {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl TokenCache {
    /// Construct an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up `token`. Returns the cached [`Device`] only when the slot
    /// exists and is still live; a stale slot is evicted on the spot and
    /// reported as a miss.
    pub async fn get(&self, token: &str) -> Option<Device> {
        let mut map = self.inner.lock().await;
        match map.get(token) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.device.clone()),
            Some(_) => {
                // Eviction on read: a stale slot is dropped immediately
                // so the map doesn't accumulate dead tokens between
                // writes.
                map.remove(token);
                None
            }
            None => None,
        }
    }

    /// Insert (or refresh) `token` -> `device` with a fresh [`TOKEN_TTL`].
    pub async fn insert(&self, token: String, device: Device) {
        self.insert_with_ttl(token, device, TOKEN_TTL).await;
    }

    /// Insert with an explicit TTL. Used by the unit tests, which drive
    /// expiry deterministically rather than waiting 60 real seconds.
    pub async fn insert_with_ttl(&self, token: String, device: Device, ttl: Duration) {
        let entry = Entry {
            device,
            expires_at: Instant::now() + ttl,
        };
        self.inner.lock().await.insert(token, entry);
    }

    /// Number of slots currently held. Stale slots are dropped only on
    /// read, so this counts both live and not-yet-read-stale entries.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// True when the cache holds no slots.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

/// Process-wide token cache. `require_device` shares this single instance
/// across every request.
pub fn global() -> &'static TokenCache {
    static CACHE: OnceLock<TokenCache> = OnceLock::new();
    CACHE.get_or_init(TokenCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> Device {
        Device {
            device_id: id.to_owned(),
            tier: "tool_use".to_owned(),
        }
    }

    #[tokio::test]
    async fn insert_then_get_is_a_hit() {
        let cache = TokenCache::new();
        cache.insert("tok-a".to_owned(), device("dev-a")).await;
        let got = cache.get("tok-a").await;
        assert_eq!(got, Some(device("dev-a")));
    }

    #[tokio::test]
    async fn get_unknown_token_is_a_miss() {
        let cache = TokenCache::new();
        assert!(cache.get("never-inserted").await.is_none());
    }

    #[tokio::test]
    async fn insert_refreshes_existing_token() {
        let cache = TokenCache::new();
        cache.insert("tok".to_owned(), device("old")).await;
        cache.insert("tok".to_owned(), device("new")).await;
        assert_eq!(cache.get("tok").await, Some(device("new")));
        assert_eq!(cache.len().await, 1, "refresh must not duplicate the slot");
    }

    #[tokio::test(start_paused = true)]
    async fn entry_expires_after_ttl_and_is_evicted_on_read() {
        let cache = TokenCache::new();
        cache.insert("tok".to_owned(), device("dev")).await;
        // Still inside the 60s window — live hit.
        assert!(cache.get("tok").await.is_some());

        // Jump past the TTL. `start_paused` makes `tokio::time::Instant`
        // a mock clock so this is deterministic, not a real 61s wait.
        tokio::time::advance(TOKEN_TTL + Duration::from_secs(1)).await;

        assert!(
            cache.get("tok").await.is_none(),
            "entry past its TTL must read as a miss"
        );
        assert_eq!(
            cache.len().await,
            0,
            "the stale entry must be evicted on the stale read"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn entry_just_inside_ttl_is_still_live() {
        let cache = TokenCache::new();
        cache.insert("tok".to_owned(), device("dev")).await;
        tokio::time::advance(TOKEN_TTL - Duration::from_secs(1)).await;
        assert!(
            cache.get("tok").await.is_some(),
            "entry one second short of the TTL must still hit"
        );
    }

    #[tokio::test]
    async fn clones_share_one_backing_map() {
        let cache = TokenCache::new();
        let clone = cache.clone();
        cache.insert("tok".to_owned(), device("dev")).await;
        assert!(
            clone.get("tok").await.is_some(),
            "a clone must observe inserts on the original"
        );
    }
}
