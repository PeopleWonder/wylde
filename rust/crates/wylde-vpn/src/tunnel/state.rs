//! Tunnel manager — owns the live wg0/wg1 sessions, coordinates the
//! enable/disable sequencing (tunnel up → mark active; reverse on the
//! way down).
//!
//! There is a single process-wide [`TunnelManager`] (`get()`), backed
//! by [`super::backend::RealBackend`] in production and the
//! [`super::backend::StubBackend`] in tests. The manager exposes:
//!
//! * `enable_vpn` / `disable_vpn` — outbound wg0 lifecycle (returns
//!   the same `{status, endpoint, public_key}` envelope Python emits).
//! * `connect_link` — bring up an inbound tunnel to a known peer (the
//!   wg1 / WyldeLink side; uses the same data-plane primitive).
//! * `vpn_runtime` / `link_runtime` — snapshot accessors for
//!   `vpn.status` / `link.status`.
//! * `shutdown_all` — service-shutdown hook.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use base64::Engine;
use parking_lot::Mutex;
use x25519_dalek::{PublicKey, StaticSecret};

use super::backend::{Backend, RealBackend, SessionHandle};
use super::datapath::{TunnelParams, TunnelStatsSnapshot};
use crate::config::Config;

/// Public snapshot of outbound (wg0) state — matches the Python
/// `get_vpn_status()` dict.
#[derive(Debug, Clone, Default)]
pub struct VpnRuntime {
    pub enabled: bool,
    pub connected: bool,
    pub connected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub endpoint: Option<String>,
    pub tunnel_ip: Option<String>,
    pub public_key: Option<String>,
    pub error: Option<String>,
    pub stats: Option<TunnelStatsSnapshot>,
}

/// Public snapshot of inbound (wg1 / WyldeLink) state.
#[derive(Debug, Clone, Default)]
pub struct LinkRuntime {
    pub enabled: bool,
    pub interface_up: bool,
    pub listen_port: u16,
    pub server_pubkey: Option<String>,
    pub stats: Option<TunnelStatsSnapshot>,
}

struct ActiveTunnel {
    session: SessionHandle,
    endpoint: String,
    public_key: String,
    /// Peer's wireguard pubkey (base64). Populated by `connect_link`;
    /// for `enable_vpn` it's the configured server we're tunnelling to.
    /// Powers the handshake monitor's per-peer state classification.
    peer_pubkey: String,
    tunnel_ip: String,
    connected_at: chrono::DateTime<chrono::Utc>,
    /// Hold a pointer to the data-plane stats so the snapshot accessor
    /// doesn't need to thread the SessionHandle's inner type.
    stats_ref: Option<Arc<super::datapath::TunnelStats>>,
}

pub struct TunnelManager {
    backend: Arc<dyn Backend>,
    state: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    vpn: Option<ActiveTunnel>,
    link: Option<ActiveTunnel>,
}

impl TunnelManager {
    fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            state: Mutex::new(ManagerState::default()),
        }
    }

    /// Process-wide singleton. Lazily initialised with [`RealBackend`].
    pub fn get() -> &'static TunnelManager {
        use std::sync::OnceLock;
        static M: OnceLock<TunnelManager> = OnceLock::new();
        M.get_or_init(|| TunnelManager::with_backend(Arc::new(RealBackend::new())))
    }

    // ── vpn.enable / vpn.disable ──────────────────────────────────────

    /// Bring up the outbound tunnel. Mirrors `enable_vpn` in
    /// `Wylde/VPN/tunnel/wireguard.py`:
    ///
    /// 1. Validate inputs (endpoint + peer_pubkey required; private key
    ///    auto-generated if blank).
    /// 2. Start the data plane (boringtun + wintun on Windows).
    /// 3. On success, store the active tunnel.
    pub fn enable_vpn(&self, req: EnableRequest) -> Result<EnableOutcome> {
        let mut state = self.state.lock();
        if let Some(active) = &state.vpn {
            return Ok(EnableOutcome::AlreadyConnected {
                endpoint: active.endpoint.clone(),
            });
        }

        let private_key = match decode_key(&req.private_key)? {
            Some(k) => k,
            None => {
                // Mirror Python `_wg_genkey`: generate fresh if blank.
                StaticSecret::random_from_rng(rand_core::OsRng).to_bytes()
            }
        };
        let peer_public = match decode_key(&req.peer_pubkey)? {
            Some(k) => k,
            None => return Err(anyhow!("endpoint and peer_pubkey are required")),
        };
        if req.endpoint.trim().is_empty() {
            return Err(anyhow!("endpoint and peer_pubkey are required"));
        }
        let secret = StaticSecret::from(private_key);
        let public = PublicKey::from(&secret);
        let pub_b64 = base64_encode(public.as_bytes());

        let params = TunnelParams {
            iface_name: "wg0".to_string(),
            static_private: private_key,
            peer_public_key: peer_public,
            endpoint: req.endpoint.clone(),
            tunnel_addr: req.tunnel_addr.clone(),
            allowed_ips: req.allowed_ips.clone(),
            keepalive_secs: Some(25),
        };
        let session = self
            .backend
            .start_tunnel(params)
            .map_err(|e| anyhow!("start_tunnel failed: {e:#}"))?;

        let tunnel_ip = req
            .tunnel_addr
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();

        let stats_ref = session
            .inner
            .as_ref()
            .and_then(|b| b.downcast_ref::<super::datapath::RunningTunnel>())
            .map(|rt| Arc::clone(rt.stats_handle()));

        let active = ActiveTunnel {
            session,
            endpoint: req.endpoint.clone(),
            public_key: pub_b64.clone(),
            peer_pubkey: req.peer_pubkey.clone(),
            tunnel_ip: tunnel_ip.clone(),
            connected_at: chrono::Utc::now(),
            stats_ref,
        };
        state.vpn = Some(active);

        Ok(EnableOutcome::Connected {
            endpoint: req.endpoint,
            public_key: pub_b64,
            tunnel_ip,
        })
    }

    pub fn disable_vpn(&self) -> Result<DisableOutcome> {
        let mut state = self.state.lock();
        let active = match state.vpn.take() {
            Some(a) => a,
            None => return Ok(DisableOutcome::NotConnected),
        };
        let _ = self.backend.stop_tunnel(active.session);
        Ok(DisableOutcome::Disconnected)
    }

    // ── link.connect ──────────────────────────────────────────────────

    /// Bring up the inbound (wg1 / WyldeLink) tunnel against a known
    /// peer. Idempotent.
    pub fn connect_link(&self, req: LinkConnectRequest) -> Result<LinkConnectOutcome> {
        let mut state = self.state.lock();
        let cfg = Config::get();
        if state.link.is_some() {
            // Python's `connect` is also idempotent — repeating the
            // request just re-pushes the peer and re-emits params.
            return Ok(LinkConnectOutcome::AlreadyConnected {
                tunnel_ip: req.tunnel_ip.clone(),
            });
        }

        let private_key = decode_key(&cfg.link_private_key)?
            .ok_or_else(|| anyhow!("link_private_key not configured (run vpn.keygen first)"))?;
        let peer_public = decode_key(&req.peer_public_key)?
            .ok_or_else(|| anyhow!("peer_public_key required"))?;
        if req.endpoint.trim().is_empty() {
            return Err(anyhow!("endpoint required"));
        }

        let params = TunnelParams {
            iface_name: "wg1".to_string(),
            static_private: private_key,
            peer_public_key: peer_public,
            endpoint: req.endpoint.clone(),
            tunnel_addr: cfg.link_tunnel_addr.clone(),
            allowed_ips: vec![format!("{}/32", req.tunnel_ip)],
            keepalive_secs: Some(25),
        };
        let session = self
            .backend
            .start_tunnel(params)
            .map_err(|e| anyhow!("start_tunnel failed: {e:#}"))?;

        let stats_ref = session
            .inner
            .as_ref()
            .and_then(|b| b.downcast_ref::<super::datapath::RunningTunnel>())
            .map(|rt| Arc::clone(rt.stats_handle()));

        let secret = StaticSecret::from(private_key);
        let public = PublicKey::from(&secret);
        let pub_b64 = base64_encode(public.as_bytes());

        let active = ActiveTunnel {
            session,
            endpoint: req.endpoint.clone(),
            public_key: pub_b64,
            peer_pubkey: req.peer_public_key.clone(),
            tunnel_ip: req.tunnel_ip.clone(),
            connected_at: chrono::Utc::now(),
            stats_ref,
        };
        state.link = Some(active);

        Ok(LinkConnectOutcome::Connected {
            endpoint: req.endpoint,
            tunnel_ip: req.tunnel_ip,
            server_pubkey: public_from_private(&cfg.link_private_key).unwrap_or_default(),
        })
    }

    // ── snapshots ─────────────────────────────────────────────────────

    pub fn vpn_runtime(&self) -> VpnRuntime {
        let state = self.state.lock();
        let cfg = Config::get();
        let Some(active) = &state.vpn else {
            return VpnRuntime {
                enabled: cfg.vpn_enabled,
                ..Default::default()
            };
        };
        VpnRuntime {
            enabled: true,
            connected: true,
            connected_at: Some(active.connected_at),
            endpoint: Some(active.endpoint.clone()),
            tunnel_ip: Some(active.tunnel_ip.clone()),
            public_key: Some(active.public_key.clone()),
            error: None,
            stats: active.stats_ref.as_ref().map(|s| s.snapshot()),
        }
    }

    pub fn link_runtime(&self) -> LinkRuntime {
        let state = self.state.lock();
        let cfg = Config::get();
        let server_pubkey = public_from_private(&cfg.link_private_key);
        let Some(active) = &state.link else {
            return LinkRuntime {
                enabled: cfg.link_enabled,
                interface_up: false,
                listen_port: cfg.link_listen_port,
                server_pubkey,
                stats: None,
            };
        };
        LinkRuntime {
            enabled: true,
            interface_up: true,
            listen_port: cfg.link_listen_port,
            server_pubkey,
            stats: active.stats_ref.as_ref().map(|s| s.snapshot()),
        }
    }

    /// Per-peer handshake age (seconds) for whoever is currently
    /// connected to wg1. Returns `(peer_pubkey, age_seconds)` if the
    /// link tunnel is up. The Rust data plane runs a single boringtun
    /// `Tunn` per session, so there is at most one wg1 peer in this
    /// list at any given time; the handshake monitor combines it with
    /// the peer store to classify peers that aren't currently
    /// reachable as `offline`.
    ///
    /// Age is sourced from `TunnelStats::last_rx_age_s` — the
    /// time-since-last-decapsulated-packet counter. It's a cheap proxy
    /// for boringtun's `Tunn::time_since_last_handshake` that doesn't
    /// require taking the data-plane mutex, and it's strictly more
    /// conservative (rx ≥ handshake — every rx implies a recent
    /// handshake).
    pub fn link_active_handshake(&self) -> Option<(String, Option<f64>)> {
        let state = self.state.lock();
        let active = state.link.as_ref()?;
        let age = active.stats_ref.as_ref().and_then(|s| s.snapshot().last_rx_age_s);
        Some((active.peer_pubkey.clone(), age))
    }

    /// Service-shutdown hook — disable both tunnels.
    pub fn shutdown_all(&self) {
        let _ = self.disable_vpn();
        let mut state = self.state.lock();
        if let Some(active) = state.link.take() {
            let _ = self.backend.stop_tunnel(active.session);
        }
    }
}

// Expose the stats Arc to the manager. Implemented here rather than as
// a public field on RunningTunnel so the inner workings stay private.
impl super::datapath::RunningTunnel {
    pub(crate) fn stats_handle(&self) -> &Arc<super::datapath::TunnelStats> {
        &self.stats
    }
}

// ── request / response shapes ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnableRequest {
    pub endpoint: String,
    pub peer_pubkey: String,
    pub private_key: String,
    pub tunnel_addr: String,
    pub allowed_ips: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum EnableOutcome {
    Connected {
        endpoint: String,
        public_key: String,
        tunnel_ip: String,
    },
    AlreadyConnected {
        endpoint: String,
    },
}

#[derive(Debug, Clone)]
pub enum DisableOutcome {
    Disconnected,
    NotConnected,
}

#[derive(Debug, Clone)]
pub struct LinkConnectRequest {
    pub peer_public_key: String,
    pub endpoint: String,
    pub tunnel_ip: String,
}

#[derive(Debug, Clone)]
pub enum LinkConnectOutcome {
    Connected {
        endpoint: String,
        tunnel_ip: String,
        server_pubkey: String,
    },
    AlreadyConnected {
        tunnel_ip: String,
    },
}

// ── helpers ───────────────────────────────────────────────────────────

fn decode_key(b64: &str) -> Result<Option<[u8; 32]>> {
    let trimmed = b64.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| anyhow!("invalid base64 key: {e}"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("expected 32-byte key, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Some(arr))
}

fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Derive the X25519 public key from a base64 private key, returning
/// `None` if the input is empty or malformed (matches the existing
/// foundation-slice behaviour at `actions.rs::derive_pubkey_or_null`).
pub fn public_from_private(b64: &str) -> Option<String> {
    let bytes = decode_key(b64).ok().flatten()?;
    let secret = StaticSecret::from(bytes);
    let public = PublicKey::from(&secret);
    Some(base64_encode(public.as_bytes()))
}

#[cfg(test)]
mod tests {
    use crate::tunnel::backend::{Op, StubBackend};
    use super::*;

    fn fresh_manager() -> (TunnelManager, Arc<StubBackend>) {
        let backend = Arc::new(StubBackend::new());
        let manager = TunnelManager::with_backend(backend.clone());
        (manager, backend)
    }

    fn fake_keypair() -> (String, String) {
        let secret = StaticSecret::random_from_rng(rand_core::OsRng);
        let public = PublicKey::from(&secret);
        (base64_encode(&secret.to_bytes()), base64_encode(public.as_bytes()))
    }

    fn fake_request() -> EnableRequest {
        let (priv_, _) = fake_keypair();
        let (_, peer_pub) = fake_keypair();
        EnableRequest {
            endpoint: "vpn.example.com:51820".into(),
            peer_pubkey: peer_pub,
            private_key: priv_,
            tunnel_addr: "10.8.0.2/24".into(),
            allowed_ips: vec!["0.0.0.0/0".into()],
        }
    }

    #[test]
    fn enable_validates_endpoint_and_peer_pubkey() {
        let (m, _b) = fresh_manager();
        let mut req = fake_request();
        req.endpoint = "".into();
        let err = m.enable_vpn(req).unwrap_err().to_string();
        assert!(err.contains("endpoint"));

        let mut req = fake_request();
        req.peer_pubkey = "".into();
        let err = m.enable_vpn(req).unwrap_err().to_string();
        assert!(err.contains("peer_pubkey"));
    }

    #[test]
    fn enable_starts_tunnel() {
        let (m, b) = fresh_manager();
        let req = fake_request();
        let out = m.enable_vpn(req).unwrap();
        assert!(matches!(out, EnableOutcome::Connected { .. }));

        let ops = b.ops();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            Op::StartTunnel { iface, .. } => assert_eq!(iface, "wg0"),
            other => panic!("unexpected op sequence: {other:?}"),
        }
    }

    #[test]
    fn enable_is_idempotent() {
        let (m, b) = fresh_manager();
        let req = fake_request();
        let _ = m.enable_vpn(req.clone()).unwrap();
        let second = m.enable_vpn(req).unwrap();
        assert!(matches!(second, EnableOutcome::AlreadyConnected { .. }));
        // No extra StartTunnel.
        let starts = b
            .ops()
            .iter()
            .filter(|o| matches!(o, Op::StartTunnel { .. }))
            .count();
        assert_eq!(starts, 1);
    }

    #[test]
    fn enable_surfaces_tunnel_failure() {
        let (m, b) = fresh_manager();
        b.arm_start_failure("synthetic wintun create error");
        let req = fake_request();
        let err = m.enable_vpn(req).unwrap_err().to_string();
        assert!(err.contains("start_tunnel"));
        // StartTunnel never returned a session, so no ops were recorded.
        assert!(b.ops().is_empty());
    }

    #[test]
    fn disable_when_not_connected_reports_not_connected() {
        let (m, b) = fresh_manager();
        let out = m.disable_vpn().unwrap();
        assert!(matches!(out, DisableOutcome::NotConnected));
        assert!(b.ops().is_empty());
    }

    #[test]
    fn disable_runs_stop_tunnel() {
        let (m, b) = fresh_manager();
        let req = fake_request();
        let _ = m.enable_vpn(req).unwrap();
        // Reset op list to focus on the disable sequence.
        b.ops.lock().unwrap().clear();
        let out = m.disable_vpn().unwrap();
        assert!(matches!(out, DisableOutcome::Disconnected));
        let ops = b.ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::StopTunnel { .. }));
    }

    #[test]
    fn vpn_runtime_reflects_active_state() {
        let (m, _) = fresh_manager();
        let pre = m.vpn_runtime();
        assert!(!pre.connected);

        let req = fake_request();
        let endpoint = req.endpoint.clone();
        let _ = m.enable_vpn(req).unwrap();
        let post = m.vpn_runtime();
        assert!(post.connected);
        assert_eq!(post.endpoint.as_deref(), Some(endpoint.as_str()));
        assert_eq!(post.tunnel_ip.as_deref(), Some("10.8.0.2"));
        assert!(post.public_key.is_some());
    }

    #[test]
    fn shutdown_all_disables_active_tunnels() {
        let (m, b) = fresh_manager();
        let _ = m.enable_vpn(fake_request()).unwrap();
        m.shutdown_all();
        let rt = m.vpn_runtime();
        assert!(!rt.connected);
        let stop_count = b
            .ops()
            .iter()
            .filter(|o| matches!(o, Op::StopTunnel { .. }))
            .count();
        assert_eq!(stop_count, 1);
    }

    #[test]
    fn decode_key_rejects_short_input() {
        assert!(decode_key("YWJjZA==").is_err()); // 4 bytes, not 32
        assert!(decode_key(&base64_encode(&[0u8; 32])).unwrap().is_some());
        assert!(decode_key("").unwrap().is_none());
        assert!(decode_key("not-base64!!!").is_err());
    }

    #[test]
    fn public_from_private_round_trips() {
        let (priv_, expected) = fake_keypair();
        let derived = public_from_private(&priv_).unwrap();
        assert_eq!(derived, expected);
        assert!(public_from_private("").is_none());
        assert!(public_from_private("nope").is_none());
    }
}
