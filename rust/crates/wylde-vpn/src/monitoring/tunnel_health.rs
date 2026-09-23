//! Tunnel health monitor — port of `Wylde/VPN/monitoring/tunnel_health.py`.
//!
//! Polls the active link tunnel's last-rx age every `interval` (default
//! 30s) and classifies every registered peer as `online` / `stale` /
//! `offline`:
//!
//! * < 30s since last rx          → `online`
//! * < `stale_after` (default 180s) → `stale`
//! * otherwise                    → `offline`
//!
//! Peers in the store that aren't the currently-connected wg1 peer
//! always resolve to `offline` — the Rust data plane runs a single
//! boringtun `Tunn` per session, so it can only physically track one
//! peer at a time. Once multi-peer wg1 server mode lands we extend
//! `TunnelManager::link_active_handshake` to return a vector and the
//! classifier loop here generalises automatically.
//!
//! Surfaced two ways:
//!
//! * `link.status` action grows a `handshakes: [...]` array — one entry
//!   per registered peer (see [`crate::actions::handle_link_status`]).
//! * On every transition the optional `on_state_change` callback fires
//!   so subscribers (e.g. a future "peer left" push notification) can
//!   react without polling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::peers::PeerStore;
use crate::tunnel::state::TunnelManager;

/// Thresholds for the three-state classifier. Matches `_tick` defaults
/// in `tunnel_health.py`.
pub const ONLINE_MAX_AGE_S: f64 = 30.0;
pub const DEFAULT_STALE_AFTER_S: f64 = 180.0;
pub const DEFAULT_INTERVAL_S: u64 = 30;

/// One peer's current health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    Online,
    Stale,
    Offline,
}

impl PeerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerState::Online => "online",
            PeerState::Stale => "stale",
            PeerState::Offline => "offline",
        }
    }

    /// Classify a peer given how long it's been since we last saw a
    /// packet from them. `None` (never seen) → `offline`.
    pub fn classify(last_rx_age_s: Option<f64>, stale_after_s: f64) -> PeerState {
        match last_rx_age_s {
            Some(age) if age < ONLINE_MAX_AGE_S => PeerState::Online,
            Some(age) if age < stale_after_s => PeerState::Stale,
            _ => PeerState::Offline,
        }
    }
}

/// JSON-friendly per-peer record emitted in `link.status` payloads.
#[derive(Debug, Clone, Serialize)]
pub struct HandshakeRecord {
    pub peer_pubkey: String,
    pub state: PeerState,
    pub last_handshake_age_s: Option<f64>,
}

/// Callback signature — fires on every state transition. The argument
/// tuple matches `(peer_pubkey, previous, current, last_rx_age_s)`.
pub type OnStateChange = Arc<dyn Fn(&str, PeerState, PeerState, Option<f64>) + Send + Sync>;

/// Live monitor handle — controls the background tokio task that polls
/// the tunnel + peer store and emits transition callbacks.
pub struct TunnelHealth {
    inner: Arc<Inner>,
}

struct Inner {
    interval: Duration,
    stale_after_s: f64,
    on_change: Option<OnStateChange>,
    state: Mutex<HashMap<String, PeerState>>,
    stop: Notify,
}

impl TunnelHealth {
    /// Build a monitor with default thresholds. Polling cadence and
    /// stale threshold mirror Python's defaults.
    pub fn new(on_change: Option<OnStateChange>) -> Self {
        Self::with_settings(
            Duration::from_secs(DEFAULT_INTERVAL_S),
            DEFAULT_STALE_AFTER_S,
            on_change,
        )
    }

    pub fn with_settings(
        interval: Duration,
        stale_after_s: f64,
        on_change: Option<OnStateChange>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                interval,
                stale_after_s,
                on_change,
                state: Mutex::new(HashMap::new()),
                stop: Notify::new(),
            }),
        }
    }

    /// Take a snapshot of the current per-peer classification —
    /// callable from any thread/task without disturbing the polling
    /// loop. Sourced from the peer store + the active link tunnel
    /// (does NOT wait for a tick).
    pub fn snapshot(&self) -> Vec<HandshakeRecord> {
        classify_all(&self.inner.state, self.inner.stale_after_s)
    }

    /// Compute the snapshot **and** advance the cached state, applying
    /// transition callbacks. Public for tests so the timing can be
    /// driven deterministically.
    pub fn tick_once(&self) -> Vec<HandshakeRecord> {
        Self::tick(&self.inner)
    }

    /// Trigger graceful shutdown — the next loop iteration breaks.
    pub fn stop(&self) {
        self.inner.stop.notify_waiters();
    }

    /// Spawn the polling loop. The returned `JoinHandle` resolves when
    /// the loop exits cleanly.
    pub fn start(&self) -> JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            loop {
                Self::tick(&inner);
                tokio::select! {
                    _ = inner.stop.notified() => break,
                    _ = tokio::time::sleep(inner.interval) => {}
                }
            }
        })
    }

    fn tick(inner: &Arc<Inner>) -> Vec<HandshakeRecord> {
        let records = classify_all(&inner.state, inner.stale_after_s);
        let mut state = inner.state.lock().unwrap();
        for rec in &records {
            let prev = state.get(&rec.peer_pubkey).copied();
            state.insert(rec.peer_pubkey.clone(), rec.state);
            if let Some(prev_state) = prev {
                if prev_state != rec.state {
                    if let Some(cb) = inner.on_change.clone() {
                        // Drop the lock before invoking user code so
                        // callbacks that call back into the monitor
                        // (or take other locks) don't deadlock.
                        drop(state);
                        cb(
                            &rec.peer_pubkey,
                            prev_state,
                            rec.state,
                            rec.last_handshake_age_s,
                        );
                        state = inner.state.lock().unwrap();
                    }
                }
            }
        }
        records
    }
}

/// Walk the peer store + active link tunnel, returning a per-peer
/// record. Pure function over the cached previous-state map (does not
/// mutate it — the polling tick handles that).
fn classify_all(
    _prev_state: &Mutex<HashMap<String, PeerState>>,
    stale_after_s: f64,
) -> Vec<HandshakeRecord> {
    let cfg = crate::config::Config::get();
    let store = PeerStore::new(&cfg.link_data_dir);
    let peers = store.list();
    let active = TunnelManager::get().link_active_handshake();

    peers
        .into_iter()
        .map(|p| {
            let last = match active.as_ref() {
                Some((pk, age)) if pk == &p.public_key => *age,
                _ => None,
            };
            HandshakeRecord {
                peer_pubkey: p.public_key,
                state: PeerState::classify(last, stale_after_s),
                last_handshake_age_s: last,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_thresholds() {
        assert_eq!(PeerState::classify(Some(5.0), 180.0), PeerState::Online);
        assert_eq!(PeerState::classify(Some(29.999), 180.0), PeerState::Online);
        assert_eq!(PeerState::classify(Some(30.0), 180.0), PeerState::Stale);
        assert_eq!(PeerState::classify(Some(179.999), 180.0), PeerState::Stale);
        assert_eq!(PeerState::classify(Some(180.0), 180.0), PeerState::Offline);
        assert_eq!(PeerState::classify(None, 180.0), PeerState::Offline);
    }

    #[test]
    fn peer_state_string_form() {
        assert_eq!(PeerState::Online.as_str(), "online");
        assert_eq!(PeerState::Stale.as_str(), "stale");
        assert_eq!(PeerState::Offline.as_str(), "offline");
    }

    #[test]
    fn classify_respects_custom_stale_threshold() {
        // A shorter `stale_after` flips earlier into `offline`.
        assert_eq!(PeerState::classify(Some(60.0), 45.0), PeerState::Offline);
        // A longer `stale_after` keeps the peer `stale` for longer.
        assert_eq!(PeerState::classify(Some(300.0), 600.0), PeerState::Stale);
    }
}
