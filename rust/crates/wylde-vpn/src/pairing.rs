//! WyldeLink pairing protocol — Rust port of `Wylde/VPN/peers/pairing.py`.
//!
//! ## Token format
//!
//! `secrets.token_urlsafe(20)` in Python returns 20 random bytes
//! encoded as URL-safe base64 *without* padding (27 chars). The Rust
//! port produces the same shape:
//!
//! ```text
//! rand_core::OsRng → 20 bytes → base64::URL_SAFE_NO_PAD → 27 chars
//! ```
//!
//! ## Lifecycle
//!
//! 1. `pair()` issues a token, stamps `expires_at = now + LINK_TOKEN_TTL`,
//!    rate-limits per remote IP (`LINK_PAIR_RATE_MAX` issuances per
//!    `LINK_PAIR_RATE_WIN` seconds — sliding window).
//! 2. `register()` consumes the token (removes from table) and writes
//!    the peer to the persistent store.
//! 3. Tokens not consumed within TTL are reaped lazily on each
//!    `pair()` / `register()` call.
//!
//! ## Replay protection
//!
//! Tokens are single-use — `register()` removes the token from the
//! table on success. A replay attempt against the same token sees
//! `pairing_token_invalid_or_expired`. The token itself is 20 random
//! bytes (160 bits of entropy) so guessing is infeasible.
//!
//! ## Pairing URI
//!
//! `wylde://link/pair?token={t}&endpoint={e}&server_pubkey={pk}&version=2`
//! — byte-equivalent with Python's URI so mobile clients work
//! unchanged. The `endpoint` resolves to `LINK_PUBLIC_HOST:LISTEN_PORT`
//! if configured; otherwise a `<host>:port` placeholder (STUN-derived
//! endpoint lands in Phase 2.C — `link.stun`).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use base64::Engine;
use chrono::Utc;
use parking_lot::Mutex;
use rand_core::RngCore;

use crate::config::Config;
use crate::peers::{PeerRecord, PeerStore};
use crate::tunnel::state::public_from_private;

/// One pairing slot — created by `pair()`, consumed by `register()`,
/// reaped by either after TTL.
#[derive(Debug, Clone)]
pub struct PairingEntry {
    pub label: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Instant,
    pub ip: String,
    pub uri: String,
}

#[derive(Debug, Clone)]
pub struct PairOutcome {
    pub token: String,
    pub label: String,
    pub uri: String,
    pub expires_in_s: u64,
    pub endpoint: String,
    pub server_pubkey: String,
    pub stun_servers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    pub status: RegisterStatus,
    pub peer: PeerRecord,
    pub connection: ConnectionParams,
}

#[derive(Debug, Clone)]
pub enum RegisterStatus {
    Ok,
    AlreadyRegistered,
}

#[derive(Debug, Clone)]
pub struct ConnectionParams {
    pub server_pubkey: String,
    pub server_addr: String,
    pub endpoint: String,
    pub listen_port: u16,
    pub peer_subnet: String,
}

#[derive(Debug)]
pub enum PairingError {
    RateLimited,
    InvalidOrExpired,
    PeerSpaceExhausted,
    Bad(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => write!(f, "too many pairing attempts, try again shortly"),
            Self::InvalidOrExpired => write!(f, "invalid or expired pairing token"),
            Self::PeerSpaceExhausted => write!(f, "peer address space exhausted"),
            Self::Bad(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for PairingError {}

/// Process-wide table. Token strings are dense (27 chars) so HashMap is fine.
struct PairingTable {
    tokens: HashMap<String, PairingEntry>,
    /// Per-IP sliding window of issuance timestamps.
    rate_buckets: HashMap<String, Vec<Instant>>,
}

impl PairingTable {
    fn new() -> Self {
        Self {
            tokens: HashMap::new(),
            rate_buckets: HashMap::new(),
        }
    }
}

fn table() -> &'static Mutex<PairingTable> {
    static T: OnceLock<Mutex<PairingTable>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(PairingTable::new()))
}

/// Issue a pairing token. Returns the [`PairOutcome`] for the caller
/// to surface (token + QR-encodable URI + connection hints).
///
/// `remote_ip` is the client's IP (used for rate limiting). Pass an
/// empty string to skip rate limiting (used for internal callers /
/// tests).
pub fn pair(label: Option<String>, remote_ip: &str) -> Result<PairOutcome, PairingError> {
    let cfg = Config::get();
    if !remote_ip.is_empty() && !rate_ok(remote_ip, cfg) {
        return Err(PairingError::RateLimited);
    }
    let label = label.unwrap_or_else(|| "unnamed".to_string());
    let token = gen_token();
    let endpoint = cached_endpoint(cfg);
    let server_pubkey = public_from_private(&cfg.link_private_key).unwrap_or_default();
    let uri = format!(
        "wylde://link/pair?token={token}&endpoint={endpoint}&server_pubkey={server_pubkey}&version=2"
    );
    let entry = PairingEntry {
        label: label.clone(),
        created_at: Utc::now(),
        expires_at: Instant::now() + Duration::from_secs(cfg.link_token_ttl),
        ip: remote_ip.to_string(),
        uri: uri.clone(),
    };
    {
        let mut t = table().lock();
        reap_expired(&mut t);
        t.tokens.insert(token.clone(), entry);
    }
    Ok(PairOutcome {
        token,
        label,
        uri,
        expires_in_s: cfg.link_token_ttl,
        endpoint,
        server_pubkey,
        stun_servers: cfg.link_stun_servers.clone(),
    })
}

/// Consume a pairing token + register the peer. Mirrors Python's
/// `register_peer`: validates the token, allocates a tunnel IP if the
/// peer is new, returns either `Ok` (new registration) or
/// `AlreadyRegistered` (idempotent re-register with the same pubkey).
pub fn register(
    token: &str,
    public_key: &str,
    label: Option<String>,
    allowed_services: Vec<String>,
    store: &PeerStore,
) -> Result<RegisterOutcome, PairingError> {
    let public_key = public_key.trim();
    if public_key.is_empty() {
        return Err(PairingError::Bad("public_key required".into()));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(PairingError::Bad("pairing token required".into()));
    }

    // Validate + consume token under the lock; release before touching
    // the peer store so we don't hold both at once.
    let _entry = {
        let mut t = table().lock();
        reap_expired(&mut t);
        match t.tokens.remove(token) {
            Some(e) => e,
            None => return Err(PairingError::InvalidOrExpired),
        }
    };

    if let Some(existing) = store.get(public_key) {
        let cfg = Config::get();
        return Ok(RegisterOutcome {
            status: RegisterStatus::AlreadyRegistered,
            peer: existing,
            connection: connection_params(cfg),
        });
    }

    let tunnel_ip = store
        .next_tunnel_ip()
        .ok_or(PairingError::PeerSpaceExhausted)?;
    let label = label.unwrap_or_else(|| short_label(public_key));
    let peer = PeerRecord {
        public_key: public_key.to_string(),
        label,
        tunnel_ip,
        registered_at: Utc::now().to_rfc3339(),
        last_seen: None,
        allowed_services,
        extra: serde_json::Map::new(),
    };
    store
        .upsert(peer.clone())
        .map_err(|e| PairingError::Bad(format!("peer store: {e}")))?;
    let cfg = Config::get();
    Ok(RegisterOutcome {
        status: RegisterStatus::Ok,
        peer,
        connection: connection_params(cfg),
    })
}

/// Look up a previously-issued token's URI so the QR endpoint can
/// render the same URI a token belongs to. Returns `None` if the
/// token doesn't exist or has expired. Does NOT consume the token —
/// QR rendering is an out-of-band lookup, not a state transition.
pub fn lookup_uri(token: &str) -> Option<String> {
    let mut t = table().lock();
    reap_expired(&mut t);
    t.tokens.get(token).map(|e| e.uri.clone())
}

/// Test hook — clears the entire table.
#[cfg(test)]
pub fn reset_for_tests() {
    let mut t = table().lock();
    t.tokens.clear();
    t.rate_buckets.clear();
}

// ── helpers ───────────────────────────────────────────────────────────

fn gen_token() -> String {
    let mut bytes = [0u8; 20];
    rand_core::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn reap_expired(t: &mut PairingTable) {
    let now = Instant::now();
    t.tokens.retain(|_, e| e.expires_at > now);
}

fn rate_ok(ip: &str, cfg: &Config) -> bool {
    let window = Duration::from_secs(cfg.link_pair_rate_win);
    let max = cfg.link_pair_rate_max as usize;
    let now = Instant::now();
    let mut t = table().lock();
    let bucket = t.rate_buckets.entry(ip.to_string()).or_default();
    bucket.retain(|ts| now.duration_since(*ts) < window);
    if bucket.len() >= max {
        return false;
    }
    bucket.push(now);
    true
}

fn cached_endpoint(cfg: &Config) -> String {
    if !cfg.link_public_host.is_empty() {
        format!("{}:{}", cfg.link_public_host, cfg.link_listen_port)
    } else {
        // STUN-derived endpoint arrives in Phase 2.C (`link.stun`).
        // Until then we surface a clear placeholder so mobile UI can
        // tell the user they need to set LINK_PUBLIC_HOST.
        format!("<host>:{}", cfg.link_listen_port)
    }
}

fn connection_params(cfg: &Config) -> ConnectionParams {
    ConnectionParams {
        server_pubkey: public_from_private(&cfg.link_private_key).unwrap_or_default(),
        server_addr: cfg
            .link_tunnel_addr
            .split('/')
            .next()
            .unwrap_or("")
            .to_string(),
        endpoint: cached_endpoint(cfg),
        listen_port: cfg.link_listen_port,
        peer_subnet: cfg.link_peer_subnet.clone(),
    }
}

fn short_label(public_key: &str) -> String {
    public_key.chars().take(16).collect()
}

// Used by tests to confirm the token shape matches Python's
// `secrets.token_urlsafe(20)` (27 chars after url-safe base64 encoding
// of 20 bytes with no padding).
#[allow(dead_code)]
fn expected_token_len() -> usize {
    27
}

// Used by tests to convert Instant deadlines into wall-clock seconds.
#[allow(dead_code)]
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // pairing.rs uses process-wide state, so serialise tests.
    fn global_test_lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    fn fresh() -> (std::sync::MutexGuard<'static, ()>, PeerStore, TempDir) {
        let g = global_test_lock().lock().unwrap();
        reset_for_tests();
        let dir = TempDir::new().unwrap();
        let store = PeerStore::new(dir.path());
        (g, store, dir)
    }

    #[test]
    fn token_has_python_compatible_shape() {
        let (_g, _s, _d) = fresh();
        let out = pair(Some("phone".into()), "").unwrap();
        assert_eq!(out.token.len(), expected_token_len());
        // URL-safe base64 chars only.
        for c in out.token.chars() {
            assert!(c.is_ascii_alphanumeric() || c == '-' || c == '_');
        }
    }

    #[test]
    fn pair_to_register_round_trip() {
        let (_g, store, _d) = fresh();
        let pair_out = pair(Some("phone".into()), "").unwrap();
        let pub_key = "AAAA0000".repeat(4); // some non-empty key
        let reg = register(
            &pair_out.token,
            &pub_key,
            Some("phone".into()),
            vec!["chat".into()],
            &store,
        )
        .unwrap();
        match reg.status {
            RegisterStatus::Ok => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(reg.peer.public_key, pub_key);
        assert_eq!(reg.peer.label, "phone");
        assert!(reg.peer.tunnel_ip.starts_with("192.0.2."));

        // Token is consumed — replay fails.
        let replay = register(&pair_out.token, &pub_key, None, vec![], &store);
        assert!(matches!(replay, Err(PairingError::InvalidOrExpired)));
    }

    #[test]
    fn re_register_with_same_pubkey_is_idempotent_after_new_token() {
        let (_g, store, _d) = fresh();
        let pair1 = pair(None, "").unwrap();
        let pub_key = "BBBB1111".repeat(4);
        let r1 = register(&pair1.token, &pub_key, None, vec![], &store).unwrap();
        assert!(matches!(r1.status, RegisterStatus::Ok));
        // New pairing flow, same pubkey: peer is already in the store.
        let pair2 = pair(None, "").unwrap();
        let r2 = register(&pair2.token, &pub_key, None, vec![], &store).unwrap();
        assert!(matches!(r2.status, RegisterStatus::AlreadyRegistered));
        assert_eq!(r2.peer.tunnel_ip, r1.peer.tunnel_ip);
    }

    #[test]
    fn register_rejects_missing_fields() {
        let (_g, store, _d) = fresh();
        assert!(matches!(
            register("", "pub", None, vec![], &store),
            Err(PairingError::Bad(_))
        ));
        assert!(matches!(
            register("tok", "", None, vec![], &store),
            Err(PairingError::Bad(_))
        ));
        assert!(matches!(
            register("nonexistent-token", "pub", None, vec![], &store),
            Err(PairingError::InvalidOrExpired)
        ));
    }

    #[test]
    fn pair_uri_format_matches_python() {
        let (_g, _s, _d) = fresh();
        let out = pair(Some("x".into()), "").unwrap();
        assert!(out.uri.starts_with("wylde://link/pair?token="));
        assert!(out.uri.contains("&endpoint="));
        assert!(out.uri.contains("&server_pubkey="));
        assert!(out.uri.contains("&version=2"));
    }

    #[test]
    fn lookup_uri_returns_issued_uri_then_none_after_register() {
        let (_g, store, _d) = fresh();
        let out = pair(None, "").unwrap();
        assert_eq!(lookup_uri(&out.token).as_deref(), Some(out.uri.as_str()));
        let _ = register(&out.token, "pubkey", None, vec![], &store);
        assert!(lookup_uri(&out.token).is_none());
    }

    #[test]
    fn rate_limit_blocks_excess_requests_per_ip() {
        let (_g, _s, _d) = fresh();
        let cfg = Config::get();
        let max = cfg.link_pair_rate_max as usize;
        let ip = "1.2.3.4";
        for _ in 0..max {
            assert!(pair(None, ip).is_ok());
        }
        let err = pair(None, ip);
        assert!(matches!(err, Err(PairingError::RateLimited)));
        // Different IP is unaffected.
        assert!(pair(None, "5.6.7.8").is_ok());
    }

    #[test]
    fn rate_limit_not_applied_when_remote_ip_empty() {
        let (_g, _s, _d) = fresh();
        for _ in 0..50 {
            assert!(pair(None, "").is_ok());
        }
    }
}
