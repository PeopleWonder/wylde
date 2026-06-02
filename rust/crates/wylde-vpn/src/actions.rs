//! WyldeLink action surface — 17 actions on `\\.\pipe\wylde-vpn`.
//!
//! Action inventory per master plan §Phase 2 (Action contract table).
//! Phase 2.C promoted `link.stun` out of `service_unavailable` and
//! extended `link.connect` to consult [`crate::nat::stun::classify`]
//! when deciding direct vs relay paths.
//!
//! | Action               | Status         | Notes                                              |
//! |---------------------|----------------|----------------------------------------------------|
//! | `vpn.status`         | impl           | Reports live tunnel state from TunnelManager.       |
//! | `vpn.enable`         | impl (2.B)     | boringtun + wintun on Windows; clean err elsewhere.|
//! | `vpn.disable`        | impl (2.B)     | Reverses enable sequence (tear down tunnel).       |
//! | `vpn.keygen`         | impl           | Pure x25519 — base64-encoded private + public keys.|
//! | `link.status`        | impl           | Live wg1 state (interface_up, peer counts, stats). |
//! | `link.pair`          | impl (2.B)     | Issue urlsafe pairing token, rate-limited per IP.  |
//! | `link.register`      | impl (2.B)     | Consume token, register peer, return WG config.    |
//! | `link.stun`          | impl (2.C)     | STUN classification + optional TURN allocation.    |
//! | `link.peers`         | impl           | Reads the JSON peer store.                         |
//! | `link.peers.remove`  | impl           | Removes from the store. (Wg1 push fires via peer table.)|
//! | `link.connect`       | impl (2.C)     | Bring up wg1; classify + emit `recommend` field.   |
//! | `link.qr`            | impl           | Renders a pairing URI as SVG. Looks up by token or |
//! |                      |                | accepts a raw URI.                                  |
//! | `link.config.get`    | impl           | Returns the YAML view (`_link_view` in Python).    |
//! | `link.services`      | impl           | Inventory of services reachable behind the tunnel  |
//! |                      |                | (manifest registry; Python never shipped this).    |
//! | `link.config.patch`  | impl (2.B)     | serde_yaml patch + atomic write + restart_required.|
//! | `link.restart`       | impl           | Schedules `std::process::exit(0)` after 250ms.     |
//! | `link.config_changed`| event-only     | Internal — not registered as a callable action.    |

use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use rand_core::OsRng;
use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::config::{link_config_view, patch_link_config, Config};
use crate::pairing::{self, PairingError, RegisterStatus};
use crate::peers::PeerStore;
use crate::tunnel::state::{
    EnableOutcome, EnableRequest, LinkConnectOutcome, LinkConnectRequest, TunnelManager,
};

/// Module path stamped into the action contract for handlers in this file.
const HANDLER_MODULE: &str = "wylde_vpn::actions";

/// Cached PeerStore — initialised at first action call so unit tests
/// don't need a live config to round-trip through.
fn peer_store() -> &'static PeerStore {
    static STORE: OnceLock<PeerStore> = OnceLock::new();
    STORE.get_or_init(|| PeerStore::new(&Config::get().link_data_dir))
}

/// Doc + module pairs for the cross-language action contract.
pub fn contract_metadata() -> Vec<(&'static str, &'static str)> {
    vec![
        ("vpn.status", "GET /api/vpn/status — current VPN tunnel state (TunnelManager-backed)."),
        ("vpn.enable", "POST /api/vpn/enable — bring wg0 up via boringtun + wintun (Windows; Linux kernel mode deferred)."),
        ("vpn.disable", "POST /api/vpn/disable — tear wg0 down."),
        ("vpn.keygen", "POST /api/vpn/keygen — fresh x25519 keypair as base64 strings."),
        ("link.status", "GET /api/link/status — WyldeLink server live tunnel state + peer count + per-peer handshake classification (Phase 2.D adds `handshakes: [{peer_pubkey, state, last_handshake_age_s}]`)."),
        ("link.pair", "POST /api/link/pair — issue a single-use pairing token (urlsafe, 20-byte) with TTL + per-IP rate limit."),
        ("link.register", "POST /api/link/register — consume pairing token and register a peer + return WG config."),
        ("link.stun", "GET /api/link/stun — STUN-classified external endpoint, NAT type, optional TURN allocation."),
        ("link.peers", "GET /api/link/peers — registered WyldeLink peers."),
        ("link.peers.remove", "POST /api/link/peers/remove — drop a peer from the store."),
        ("link.connect", "POST /api/link/connect — bring up the wg1 tunnel to a paired peer."),
        ("link.qr", "GET /api/link/qr/<token> — render a pairing URI as SVG bytes."),
        ("link.config.get", "GET /api/link/config — current YAML view."),
        ("link.services", "GET /api/link/services — inventory of Wylde services reachable behind the WyldeLink tunnel ({name, description, port}), sourced from the runtime manifest registry."),
        ("link.config.patch", "PATCH /api/link/config — mutate link + relay keys + atomically rewrite VPN/config.yaml."),
        ("link.restart", "POST /api/restart — schedule a graceful self-exit so the launcher respawns with fresh config."),
    ]
}

/// Canonical list of action names, sorted. Useful for tests + the
/// manifest's `contributes.wylde_vpn.actions` block.
pub fn all_action_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = contract_metadata().into_iter().map(|(n, _)| n).collect();
    names.sort();
    names
}

// ── Implemented handlers ─────────────────────────────────────────────────

pub async fn handle_vpn_status(_payload: Value) -> Reply {
    let cfg = Config::get();
    let rt = TunnelManager::get().vpn_runtime();
    let mut payload = json!({
        "enabled": rt.enabled || cfg.vpn_enabled,
        "endpoint": rt.endpoint.unwrap_or_else(|| cfg.vpn_endpoint.clone()),
        "tunnel_addr": cfg.vpn_tunnel_addr,
        "connected": rt.connected,
        "interface_up": rt.connected,
        "connected_at": rt.connected_at.map(|t| t.to_rfc3339()),
        "tunnel_ip": rt.tunnel_ip,
        "public_key": rt.public_key,
        "error": rt.error,
        "impl": "rust-2.B",
    });
    if let Some(stats) = rt.stats {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("rx_bytes".into(), json!(stats.rx_bytes));
            obj.insert("tx_bytes".into(), json!(stats.tx_bytes));
            obj.insert("uptime_s".into(), json!(stats.uptime_s));
        }
    }
    Reply::ok(payload)
}

pub async fn handle_vpn_keygen(_payload: Value) -> Reply {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    let priv_b64 = base64::engine::general_purpose::STANDARD.encode(secret.to_bytes());
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(public.as_bytes());
    Reply::ok(json!({
        "private_key": priv_b64,
        "public_key": pub_b64,
    }))
}

pub async fn handle_link_status(_payload: Value) -> Reply {
    use crate::monitoring::tunnel_health::{HandshakeRecord, PeerState, DEFAULT_STALE_AFTER_S};
    let cfg = Config::get();
    let peers = peer_store().list();
    let rt = TunnelManager::get().link_runtime();

    // Phase 2.D — per-peer handshake classification, exposed for the
    // mobile app's "peer health" UI and the gateway dashboard. Computed
    // inline (not against the monitor's cached state) so a one-off
    // `link.status` call still returns fresh data even if the
    // background monitor hasn't ticked recently.
    let active = TunnelManager::get().link_active_handshake();
    let handshakes: Vec<HandshakeRecord> = peers
        .iter()
        .map(|p| {
            let last = match active.as_ref() {
                Some((pk, age)) if pk == &p.public_key => *age,
                _ => None,
            };
            HandshakeRecord {
                peer_pubkey: p.public_key.clone(),
                state: PeerState::classify(last, DEFAULT_STALE_AFTER_S),
                last_handshake_age_s: last,
            }
        })
        .collect();

    let mut payload = json!({
        "enabled": rt.enabled || cfg.link_enabled,
        "listen_port": rt.listen_port,
        "server_pubkey": rt
            .server_pubkey
            .map(Value::String)
            .unwrap_or(Value::Null),
        "interface_up": rt.interface_up,
        "peer_count": peers.len(),
        "handshakes": handshakes,
        "phase": 2,
        "impl": "rust-2.D",
    });
    if let Some(stats) = rt.stats {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("rx_bytes".into(), json!(stats.rx_bytes));
            obj.insert("tx_bytes".into(), json!(stats.tx_bytes));
            obj.insert("uptime_s".into(), json!(stats.uptime_s));
        }
    }
    Reply::ok(payload)
}

pub async fn handle_link_peers(_payload: Value) -> Reply {
    let peers = peer_store().list();
    let peers_json: Vec<Value> = peers.into_iter().map(peer_to_json).collect();
    Reply::ok(json!({ "peers": peers_json }))
}

pub async fn handle_link_peers_remove(payload: Value) -> Reply {
    let public_key = match payload
        .as_object()
        .and_then(|o| o.get("public_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => {
            return Reply::err(IpcError::new("bad_request", "public_key required"));
        }
    };
    match peer_store().remove(&public_key) {
        Ok(true) => Reply::ok(json!({ "status": "removed", "public_key": public_key })),
        Ok(false) => Reply::err(IpcError::new(
            "not_found",
            format!("peer {public_key:?} not in store"),
        )),
        Err(e) => Reply::err(IpcError::new("store_failed", format!("{e:#}"))),
    }
}

pub async fn handle_link_qr(payload: Value) -> Reply {
    // Python's `/api/link/qr/<token>` looks up the URI the pairing flow
    // issued for that token; if not found, 404. The Rust action accepts:
    //   * `{token: "<token>"}` — look up issued URI; falls back to a
    //     synthesised URI if the token isn't in the pairing table (the
    //     HTTP route always passes a token, so this matches Python's
    //     behaviour for the common case).
    //   * `{uri: "<full uri>"}` — render an arbitrary URI directly.
    let (uri, token) = match payload.as_object() {
        Some(o) => {
            if let Some(u) = o.get("uri").and_then(Value::as_str) {
                (u.to_string(), None)
            } else if let Some(t) = o.get("token").and_then(Value::as_str) {
                let token = t.to_string();
                let uri = pairing::lookup_uri(&token)
                    .unwrap_or_else(|| format!("wylde://link/pair?token={token}"));
                (uri, Some(token))
            } else {
                return Reply::err(IpcError::new(
                    "bad_request",
                    "payload must include either 'token' or 'uri'",
                ));
            }
        }
        None => {
            return Reply::err(IpcError::new("bad_request", "payload must be an object"));
        }
    };

    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(256, 256)
                .build();
            Reply::ok(json!({
                "uri": uri,
                "token": token,
                "content_type": "image/svg+xml",
                "svg": svg,
            }))
        }
        Err(e) => Reply::err(IpcError::new("qr_failed", format!("QR encode failed: {e}"))),
    }
}

pub async fn handle_link_config_get(_payload: Value) -> Reply {
    Reply::ok(link_config_view(Config::get()))
}

/// `GET /api/link/services` / `link.services` — the inventory of Wylde
/// services reachable remotely through the WyldeLink tunnel.
///
/// The Python VPN never shipped this route (the RemoteAccess panel's
/// Services tab 404'd against Flask), so there is no byte-for-byte
/// parity to match — this is the canonical implementation. The source
/// of truth is the live runtime manifest registry under
/// `WYLDE_ROOT/data/manifests/*.json`: every service that publishes an
/// HTTP `port` is, by definition, reachable behind the tunnel (principle
/// #16 — the WyldeLink VPN is the single auth boundary, so every local
/// HTTP service sits behind it). Each manifest projects to the
/// `{name, description, port}` shape the panel's `ServiceRow` parses.
///
/// Services without a `port` (pure pipe-only actors) are omitted — the
/// panel surfaces "services available remotely", and a portless service
/// has no remotely-addressable HTTP surface.
pub async fn handle_link_services(_payload: Value) -> Reply {
    let dir = Config::get().wylde_root.join("data").join("manifests");
    let mut services: Vec<Value> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            // Only HTTP-reachable services belong on the "available
            // remotely" list — skip manifests with no/null port AND
            // pipe-only services that publish `port: 0` (they have no
            // remotely-addressable HTTP surface behind the tunnel).
            let Some(port) = doc.get("port").and_then(Value::as_u64).filter(|p| *p > 0) else {
                continue;
            };
            let name = doc
                .get("service")
                .and_then(Value::as_str)
                .or_else(|| doc.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = doc
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            services.push(json!({
                "name": name,
                "description": description,
                "port": port,
            }));
        }
    }

    Reply::ok(json!({ "services": services }))
}

pub async fn handle_link_restart(_payload: Value) -> Reply {
    // Mirror the Python `/api/restart` semantics: respond first, then
    // exit after a short delay so the launcher can respawn the process.
    // 250ms matches `VPN/api.py::restart`.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        tracing::info!("wylde-vpn: link.restart honoured — exiting");
        std::process::exit(0);
    });
    Reply::ok(json!({ "status": "restarting", "delay_ms": 250 }))
}

// ── Phase 2.B handlers ──────────────────────────────────────────────────

pub async fn handle_vpn_enable(payload: Value) -> Reply {
    let cfg = Config::get();
    let obj = payload.as_object();
    let endpoint = obj
        .and_then(|o| o.get("endpoint"))
        .and_then(Value::as_str)
        .unwrap_or(&cfg.vpn_endpoint)
        .to_string();
    let peer_pubkey = obj
        .and_then(|o| o.get("peer_pubkey"))
        .and_then(Value::as_str)
        .unwrap_or(&cfg.vpn_peer_pubkey)
        .to_string();
    let private_key = obj
        .and_then(|o| o.get("private_key"))
        .and_then(Value::as_str)
        .unwrap_or(&cfg.vpn_private_key)
        .to_string();
    let tunnel_addr = obj
        .and_then(|o| o.get("tunnel_addr"))
        .and_then(Value::as_str)
        .unwrap_or(&cfg.vpn_tunnel_addr)
        .to_string();
    let allowed_ips: Vec<String> = obj
        .and_then(|o| o.get("allowed_ips"))
        .and_then(Value::as_str)
        .unwrap_or(&cfg.vpn_allowed_ips)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let req = EnableRequest {
        endpoint,
        peer_pubkey,
        private_key,
        tunnel_addr,
        allowed_ips,
    };

    let result = tokio::task::spawn_blocking(move || TunnelManager::get().enable_vpn(req))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("join error: {e}")));

    match result {
        Ok(EnableOutcome::Connected {
            endpoint,
            public_key,
            tunnel_ip,
        }) => Reply::ok(json!({
            "status": "connected",
            "endpoint": endpoint,
            "public_key": public_key,
            "tunnel_ip": tunnel_ip,
        })),
        Ok(EnableOutcome::AlreadyConnected { endpoint }) => Reply::ok(json!({
            "status": "already_connected",
            "endpoint": endpoint,
        })),
        Err(e) => Reply::err(map_enable_error(&format!("{e:#}"))),
    }
}

pub async fn handle_vpn_disable(_payload: Value) -> Reply {
    let result = tokio::task::spawn_blocking(|| TunnelManager::get().disable_vpn())
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("join error: {e}")));
    match result {
        Ok(crate::tunnel::state::DisableOutcome::Disconnected) => {
            Reply::ok(json!({ "status": "disconnected" }))
        }
        Ok(crate::tunnel::state::DisableOutcome::NotConnected) => {
            Reply::ok(json!({ "status": "not_connected" }))
        }
        Err(e) => Reply::err(IpcError::new("tunnel_failed", format!("{e:#}"))),
    }
}

pub async fn handle_link_pair(payload: Value) -> Reply {
    let label = payload
        .as_object()
        .and_then(|o| o.get("label"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let remote_ip = payload
        .as_object()
        .and_then(|o| o.get("_remote_ip"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match pairing::pair(label, &remote_ip) {
        Ok(out) => Reply::ok(json!({
            "token": out.token,
            "label": out.label,
            "uri": out.uri,
            "expires_in": out.expires_in_s,
            "endpoint": out.endpoint,
            "server_pubkey": out.server_pubkey,
            "stun_servers": out.stun_servers,
        })),
        Err(PairingError::RateLimited) => Reply::err(IpcError::new(
            "rate_limited",
            "too many pairing attempts, try again shortly",
        )),
        Err(e) => Reply::err(IpcError::new("pairing_failed", e.to_string())),
    }
}

pub async fn handle_link_register(payload: Value) -> Reply {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => {
            return Reply::err(IpcError::new("bad_request", "payload must be an object"))
        }
    };
    let token = obj
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let public_key = obj
        .get("public_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let label = obj.get("label").and_then(Value::as_str).map(str::to_string);
    let allowed_services: Vec<String> = obj
        .get("allowed_services")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let store = peer_store();
    let result = pairing::register(&token, &public_key, label, allowed_services, store);
    match result {
        Ok(out) => {
            let status_str = match out.status {
                RegisterStatus::Ok => "ok",
                RegisterStatus::AlreadyRegistered => "already_registered",
            };
            Reply::ok(json!({
                "status": status_str,
                "peer": peer_record_to_json(&out.peer),
                "server_pubkey": out.connection.server_pubkey,
                "server_addr": out.connection.server_addr,
                "endpoint": out.connection.endpoint,
                "listen_port": out.connection.listen_port,
                "peer_subnet": out.connection.peer_subnet,
            }))
        }
        Err(PairingError::Bad(msg)) => Reply::err(IpcError::new("bad_request", msg)),
        Err(PairingError::InvalidOrExpired) => Reply::err(IpcError::new(
            "pairing_token_invalid_or_expired",
            "invalid or expired pairing token",
        )),
        Err(PairingError::PeerSpaceExhausted) => Reply::err(IpcError::new(
            "service_unavailable",
            "peer address space exhausted",
        )),
        Err(PairingError::RateLimited) => Reply::err(IpcError::new(
            "rate_limited",
            "too many pairing attempts, try again shortly",
        )),
    }
}

pub async fn handle_link_stun(_payload: Value) -> Reply {
    let cfg = Config::get();
    let payload = tokio::task::spawn_blocking(|| build_stun_info(Config::get()))
        .await
        .unwrap_or_else(|e| {
            json!({
                "stun_servers": cfg.link_stun_servers,
                "link_port": cfg.link_listen_port,
                "discovered": Value::Null,
                "public_endpoint": format!("<host>:{}", cfg.link_listen_port),
                "relay": Value::Null,
                "nat_type": "unknown",
                "nat_detail": nat_hint(cfg),
                "nat_mappings": Vec::<Value>::new(),
                "recommend": "relay",
                "error": format!("join error: {e}"),
            })
        });
    Reply::ok(payload)
}

/// Synchronous build for the `link.stun` payload — runs STUN
/// classification + (optionally) a TURN allocation. Mirrors
/// `Wylde/VPN/peers/pairing.py::stun_info` exactly.
fn build_stun_info(cfg: &Config) -> Value {
    let stun_timeout = std::time::Duration::from_millis(2_500);
    let discovered = crate::nat::stun::discover_endpoint(
        &cfg.link_stun_servers,
        cfg.link_listen_port,
        std::time::Duration::from_secs(3),
    );
    let classification = crate::nat::stun::classify(
        &cfg.link_stun_servers,
        cfg.link_listen_port,
        stun_timeout,
    );

    let mut relay: Option<Value> = None;
    if !cfg.link_relay_host.is_empty() {
        let allocation = crate::nat::turn::allocate(
            &cfg.link_relay_host,
            cfg.link_relay_port,
            &cfg.link_relay_user,
            &cfg.link_relay_pass,
            &cfg.link_relay_realm,
            cfg.link_relay_lifetime as u32,
            std::time::Duration::from_secs(3),
        );
        let reachable = allocation.is_some();
        relay = Some(json!({
            "host": cfg.link_relay_host,
            "port": cfg.link_relay_port,
            "username": cfg.link_relay_user,
            "password": cfg.link_relay_pass,
            "protocol": "TURN/UDP",
            "realm": cfg.link_relay_realm,
            "allocation": allocation,
            "reachable": reachable,
        }));
    }

    let mut recommend = classification.recommend.as_str().to_string();
    if recommend == "relay"
        && !relay
            .as_ref()
            .and_then(|r| r.get("reachable"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        recommend = "relay-unavailable".to_string();
    }

    let public_endpoint = discovered
        .as_ref()
        .and_then(|d| {
            let ip = d.get("ip")?.as_str()?;
            let port = d.get("port")?.as_u64()?;
            Some(format!("{ip}:{port}"))
        })
        .unwrap_or_else(|| format!("<host>:{}", cfg.link_listen_port));

    json!({
        "stun_servers": cfg.link_stun_servers,
        "link_port": cfg.link_listen_port,
        "discovered": discovered,
        "public_endpoint": public_endpoint,
        "relay": relay,
        "nat_type": classification.nat_type.as_str(),
        "nat_detail": nat_hint(cfg),
        "nat_mappings": classification.mappings,
        "recommend": recommend,
    })
}

/// Equivalent of `Wylde/VPN/peers/pairing.py::_nat_hint` — coarse
/// label kept for back-compat with mobile clients that inspect it.
fn nat_hint(cfg: &Config) -> &'static str {
    if !cfg.link_public_host.is_empty() {
        "static-override"
    } else {
        "wsl2-double-nat"
    }
}

pub async fn handle_link_connect(payload: Value) -> Reply {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => return Reply::err(IpcError::new("bad_request", "payload must be an object")),
    };
    let public_key = obj
        .get("public_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if public_key.is_empty() {
        return Reply::err(IpcError::new("bad_request", "public_key required"));
    }
    let store = peer_store();
    let peer = match store.get(&public_key) {
        Some(p) => p,
        None => {
            return Reply::err(IpcError::new(
                "not_found",
                "peer not registered — call link.register first",
            ));
        }
    };
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if endpoint.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "endpoint required (host:port of paired peer)",
        ));
    }

    let req = LinkConnectRequest {
        peer_public_key: public_key.clone(),
        endpoint: endpoint.clone(),
        tunnel_ip: peer.tunnel_ip.clone(),
    };

    let result = tokio::task::spawn_blocking(move || TunnelManager::get().connect_link(req))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("join error: {e}")));

    let cfg = Config::get();
    let server_addr = cfg.link_tunnel_addr.split('/').next().unwrap_or("");
    let wg_config = format!(
        "[Interface]\nAddress = {tip}/32\nDNS = {dns}\n\n[Peer]\nPublicKey = {pubkey}\nAllowedIPs = {subnet}\nEndpoint = {ep}\nPersistentKeepalive = 25\n",
        tip = peer.tunnel_ip,
        dns = server_addr,
        pubkey = crate::tunnel::state::public_from_private(&cfg.link_private_key).unwrap_or_default(),
        subnet = cfg.link_peer_subnet,
        ep = endpoint,
    );

    // Phase 2.C — classify this side's NAT before emitting the reply so
    // the caller (mobile app) knows whether to attempt direct P2P
    // (open / *-cone) or fall back to the configured TURN relay
    // (symmetric / blocked). The classification is best-effort: timeouts
    // surface as `unknown` and never gate the connect itself.
    let nat = tokio::task::spawn_blocking(|| classify_for_connect(Config::get()))
        .await
        .unwrap_or_else(|_| ConnectClassification::unknown());

    let nat_json = json!({
        "nat_type": nat.nat_type,
        "recommend": nat.recommend,
        "public_endpoint": nat.public_endpoint,
    });

    match result {
        Ok(LinkConnectOutcome::Connected {
            endpoint,
            tunnel_ip,
            server_pubkey,
        }) => Reply::ok(json!({
            "status": "connected",
            "peer": peer_record_to_json(&peer),
            "wg_config": wg_config,
            "endpoint": endpoint,
            "tunnel_ip": tunnel_ip,
            "server_pubkey": server_pubkey,
            "nat": nat_json,
            "relay": nat.relay,
        })),
        Ok(LinkConnectOutcome::AlreadyConnected { tunnel_ip }) => Reply::ok(json!({
            "status": "already_connected",
            "peer": peer_record_to_json(&peer),
            "tunnel_ip": tunnel_ip,
            "nat": nat_json,
            "relay": nat.relay,
        })),
        Err(e) => Reply::err(IpcError::new("tunnel_failed", format!("{e:#}"))),
    }
}

/// Slim view of the classification result that `link.connect` returns.
/// Only the fields a paired peer needs to decide its own connect path:
/// the NAT type label, the recommended strategy, the desktop's public
/// endpoint, and (when configured + reachable) the TURN relay block
/// — which doubles as the symmetric-NAT fallback path.
struct ConnectClassification {
    nat_type: String,
    recommend: String,
    public_endpoint: Option<String>,
    relay: Value,
}

impl ConnectClassification {
    fn unknown() -> Self {
        Self {
            nat_type: "unknown".to_string(),
            recommend: "direct".to_string(),
            public_endpoint: None,
            relay: Value::Null,
        }
    }
}

fn classify_for_connect(cfg: &Config) -> ConnectClassification {
    if cfg.link_stun_servers.is_empty() {
        return ConnectClassification::unknown();
    }
    let classification = crate::nat::stun::classify(
        &cfg.link_stun_servers,
        cfg.link_listen_port,
        std::time::Duration::from_millis(1_500),
    );

    let mut relay = Value::Null;
    // Allocate TURN only when the classification recommends relay AND a
    // relay is configured. Saves the 2× UDP round-trip on the common
    // direct/hole-punch paths.
    if classification.recommend == crate::nat::stun::Recommend::Relay
        && !cfg.link_relay_host.is_empty()
    {
        let allocation = crate::nat::turn::allocate(
            &cfg.link_relay_host,
            cfg.link_relay_port,
            &cfg.link_relay_user,
            &cfg.link_relay_pass,
            &cfg.link_relay_realm,
            cfg.link_relay_lifetime as u32,
            std::time::Duration::from_secs(3),
        );
        let reachable = allocation.is_some();
        relay = json!({
            "host": cfg.link_relay_host,
            "port": cfg.link_relay_port,
            "username": cfg.link_relay_user,
            "realm": cfg.link_relay_realm,
            "allocation": allocation,
            "reachable": reachable,
        });
    }

    let mut recommend = classification.recommend.as_str().to_string();
    if recommend == "relay"
        && !relay
            .get("reachable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        recommend = "relay-unavailable".to_string();
    }

    ConnectClassification {
        nat_type: classification.nat_type.as_str().to_string(),
        recommend,
        public_endpoint: classification.public_endpoint,
        relay,
    }
}

pub async fn handle_link_config_patch(payload: Value) -> Reply {
    let result = tokio::task::spawn_blocking(move || patch_link_config(&payload))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("join error: {e}")));
    match result {
        Ok(r) => Reply::ok(r.view),
        Err(e) => {
            let msg = format!("{e:#}");
            let code = if msg.contains("invalid or unknown fields") || msg.contains("must be") {
                "bad_request"
            } else {
                "config_write_failed"
            };
            Reply::err(IpcError::new(code, msg))
        }
    }
}

/// Convert a tunnel-manager error message to the right `IpcError` code.
/// Validation errors → bad_request; tunnel failures → tunnel_failed.
fn map_enable_error(msg: &str) -> IpcError {
    if msg.contains("endpoint and peer_pubkey")
        || msg.contains("required")
        || msg.contains("invalid base64")
    {
        IpcError::new("bad_request", msg.to_string())
    } else {
        IpcError::new("tunnel_failed", msg.to_string())
    }
}

fn peer_record_to_json(p: &crate::peers::PeerRecord) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("public_key".into(), Value::String(p.public_key.clone()));
    obj.insert("label".into(), Value::String(p.label.clone()));
    obj.insert("tunnel_ip".into(), Value::String(p.tunnel_ip.clone()));
    obj.insert("registered_at".into(), Value::String(p.registered_at.clone()));
    obj.insert(
        "last_seen".into(),
        p.last_seen
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "allowed_services".into(),
        Value::Array(
            p.allowed_services
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    Value::Object(obj)
}

// ── helpers ───────────────────────────────────────────────────────────────

fn peer_to_json(p: crate::peers::PeerRecord) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("public_key".into(), Value::String(p.public_key));
    obj.insert("label".into(), Value::String(p.label));
    obj.insert("tunnel_ip".into(), Value::String(p.tunnel_ip));
    obj.insert("registered_at".into(), Value::String(p.registered_at));
    obj.insert(
        "last_seen".into(),
        p.last_seen.map(Value::String).unwrap_or(Value::Null),
    );
    obj.insert(
        "allowed_services".into(),
        Value::Array(p.allowed_services.into_iter().map(Value::String).collect()),
    );
    for (k, v) in p.extra {
        obj.entry(k).or_insert(v);
    }
    Value::Object(obj)
}

// `derive_pubkey_or_null` is retained on the TunnelManager
// (`public_from_private`) since link.status now sources the server
// public key from there; the foundation-slice copy here is gone.

/// Stamped on each registered action so the contract file carries the
/// right `handler_module`.
pub fn handler_module() -> &'static str {
    HANDLER_MODULE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vpn_keygen_produces_valid_base64() {
        let reply = handle_vpn_keygen(Value::Null).await;
        assert!(reply.ok);
        let priv_b64 = reply.data["private_key"].as_str().unwrap();
        let pub_b64 = reply.data["public_key"].as_str().unwrap();
        let priv_bytes = base64::engine::general_purpose::STANDARD
            .decode(priv_b64)
            .unwrap();
        let pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(pub_b64)
            .unwrap();
        assert_eq!(priv_bytes.len(), 32);
        assert_eq!(pub_bytes.len(), 32);
        // Re-derive public from private and check it matches.
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&priv_bytes);
        let secret = StaticSecret::from(arr);
        let public = PublicKey::from(&secret);
        assert_eq!(pub_bytes.as_slice(), public.as_bytes());
    }

    #[tokio::test]
    async fn vpn_keygen_is_non_deterministic() {
        let a = handle_vpn_keygen(Value::Null).await;
        let b = handle_vpn_keygen(Value::Null).await;
        assert_ne!(a.data["private_key"], b.data["private_key"]);
    }

    #[tokio::test]
    async fn link_stun_returns_classification_payload() {
        // No real STUN server reachable from the test environment, but
        // the action still has to emit the full envelope (with
        // nat_type/recommend filled in, even if classification times
        // out → `blocked`). This guards against regressing back to a
        // `service_unavailable` stub.
        let r = handle_link_stun(Value::Null).await;
        assert!(r.ok, "link.stun should succeed even when probes fail");
        for key in [
            "stun_servers",
            "link_port",
            "discovered",
            "public_endpoint",
            "relay",
            "nat_type",
            "nat_detail",
            "nat_mappings",
            "recommend",
        ] {
            assert!(
                r.data.get(key).is_some(),
                "link.stun payload missing key {key}"
            );
        }
        // The NAT-type string must be one of the documented categories.
        let nat = r.data["nat_type"].as_str().unwrap();
        assert!(
            [
                "open",
                "full-cone",
                "restricted-cone",
                "port-restricted",
                "symmetric",
                "blocked",
                "unknown",
            ]
            .contains(&nat),
            "unexpected nat_type {nat:?}"
        );
    }

    #[tokio::test]
    async fn vpn_enable_rejects_missing_endpoint_with_bad_request() {
        // Empty endpoint + empty config defaults → bad_request, not
        // service_unavailable. Confirms the action is wired up.
        let r = handle_vpn_enable(json!({"endpoint": "", "peer_pubkey": ""})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn link_register_rejects_missing_token() {
        let r = handle_link_register(json!({"public_key": "abc"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn link_connect_rejects_missing_public_key() {
        let r = handle_link_connect(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn link_qr_renders_svg_from_uri() {
        let reply = handle_link_qr(json!({"uri": "wylde://link/pair?token=test"})).await;
        assert!(reply.ok);
        let svg = reply.data["svg"].as_str().unwrap();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
        assert_eq!(reply.data["content_type"], "image/svg+xml");
    }

    #[tokio::test]
    async fn link_qr_falls_back_to_synthesised_uri_for_unknown_token() {
        // Unknown tokens fall back to synthesising the URI so the QR
        // endpoint keeps working — matches the previous slice behaviour
        // for any token Python hasn't seen.
        let reply = handle_link_qr(json!({"token": "abc-123"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["uri"], "wylde://link/pair?token=abc-123");
        assert_eq!(reply.data["token"], "abc-123");
    }

    #[tokio::test]
    async fn link_qr_rejects_empty_payload() {
        let reply = handle_link_qr(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn link_status_reports_disabled_by_default() {
        let reply = handle_link_status(Value::Null).await;
        assert!(reply.ok);
        // Default config has link_enabled=false.
        assert_eq!(reply.data["enabled"], false);
        assert!(reply.data["peer_count"].is_u64());
    }

    #[tokio::test]
    async fn vpn_status_reports_disconnected_when_no_active_tunnel() {
        let reply = handle_vpn_status(Value::Null).await;
        assert!(reply.ok);
        // Without an active tunnel the manager reports disconnected.
        assert_eq!(reply.data["connected"], false);
        assert_eq!(reply.data["interface_up"], false);
        assert_eq!(reply.data["impl"], "rust-2.B");
    }

    #[test]
    fn all_actions_listed_and_sorted() {
        let names = all_action_names();
        assert_eq!(names.len(), 16); // 15 prior + link.services; link.config_changed stays event-only
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn contract_metadata_is_consistent_with_names() {
        let meta = contract_metadata();
        assert_eq!(meta.len(), 16);
        for (name, doc) in &meta {
            assert!(!name.is_empty());
            assert!(!doc.is_empty());
        }
    }
}
