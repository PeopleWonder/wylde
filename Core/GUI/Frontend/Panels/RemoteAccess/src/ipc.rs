//! Per-panel IPC helpers for the Remote Access panel.
//!
//! `wylde-vpn` uses HTTP-style verbs/paths over its named pipe (the
//! `pipe_call` shape with `http_verb` + `path`) rather than the
//! `/__action__` envelope the harness uses.  We model the routes
//! verbatim:
//!
//!   * `GET  /api/link/status`          — interface + config snapshot
//!   * `GET  /api/link/peers`           — registered peers + handshakes
//!   * `GET  /api/link/config`          — full link config view
//!   * `GET  /api/link/services`        — exposed-services list
//!
//! Each helper projects the JSON reply into a strongly-typed struct so
//! the View never touches `serde_json::Value` directly.  Errors carry
//! the raw transport message; the View's degraded-state branch shows
//! it inline so the user can debug without a console.

use serde_json::Value;

pub const SVC_VPN: &str = "wylde-vpn";

async fn get(path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(SVC_VPN, "GET", path, None).await
}

// ── Status (interface + listen socket) ──────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkStatus {
    pub enabled: bool,
    pub interface_up: bool,
    pub listen_port: u32,
    pub public_key: String,
}

impl LinkStatus {
    pub fn from_value(v: &Value) -> Self {
        Self {
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            interface_up: v
                .get("interface_up")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            listen_port: v.get("listen_port").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            public_key: v
                .get("public_key")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    pub fn is_unknown(&self) -> bool {
        !self.enabled && !self.interface_up && self.listen_port == 0 && self.public_key.is_empty()
    }
}

pub async fn read_status() -> Result<LinkStatus, String> {
    let v = get("/api/link/status").await?;
    Ok(LinkStatus::from_value(&v))
}

// ── Link config (DDNS-ish fields) ────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkConfig {
    pub enabled: bool,
    pub public_host: String,
    pub listen_port: u32,
    pub tunnel_addr: String,
    pub peer_subnet: String,
    pub restart_required: bool,
}

impl LinkConfig {
    pub fn from_value(v: &Value) -> Self {
        Self {
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            public_host: v
                .get("public_host")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            listen_port: v.get("listen_port").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            tunnel_addr: v
                .get("tunnel_addr")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            peer_subnet: v
                .get("peer_subnet")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            restart_required: v
                .get("restart_required")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        }
    }
}

pub async fn read_config() -> Result<LinkConfig, String> {
    let v = get("/api/link/config").await?;
    Ok(LinkConfig::from_value(&v))
}

// ── Peers ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerRow {
    pub public_key: String,
    pub label: String,
    pub tunnel_ip: String,
    pub online: bool,
    /// ISO-8601 string from the server; the View renders relative when
    /// it can parse a Unix timestamp out of it (`fmt_relative`).  Empty
    /// when the peer has never handshaked.
    pub last_handshake: String,
    /// Optional traffic counters surfaced by the wg show command.  0
    /// when the peer hasn't transferred anything yet.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub endpoint: String,
}

impl PeerRow {
    pub fn from_value(v: &Value) -> Self {
        Self {
            public_key: v
                .get("public_key")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            label: v
                .get("label")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            tunnel_ip: v
                .get("tunnel_ip")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            online: v.get("online").and_then(|x| x.as_bool()).unwrap_or(false),
            last_handshake: v
                .get("last_handshake")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            rx_bytes: v.get("rx_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            tx_bytes: v.get("tx_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
            endpoint: v
                .get("endpoint")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// 12-char prefix of the public key for the row chip — same
    /// truncation the Svelte page uses.
    pub fn short_key(&self) -> String {
        if self.public_key.chars().count() <= 12 {
            self.public_key.clone()
        } else {
            format!("{}…", self.public_key.chars().take(12).collect::<String>())
        }
    }
}

pub async fn read_peers() -> Result<Vec<PeerRow>, String> {
    let v = get("/api/link/peers").await?;
    let Some(arr) = v.get("peers").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(PeerRow::from_value).collect())
}

// ── Exposed services ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceRow {
    pub name: String,
    pub description: String,
    pub port: u32,
}

impl ServiceRow {
    pub fn from_value(v: &Value) -> Self {
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            description: v
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        }
    }
}

pub async fn read_services() -> Result<Vec<ServiceRow>, String> {
    let v = get("/api/link/services").await?;
    let Some(arr) = v.get("services").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(ServiceRow::from_value).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn link_status_parses_full_envelope() {
        let v = json!({
            "enabled": true,
            "interface_up": true,
            "listen_port": 51821,
            "public_key": "abcDEF123456=",
        });
        let s = LinkStatus::from_value(&v);
        assert!(s.enabled);
        assert!(s.interface_up);
        assert_eq!(s.listen_port, 51821);
        assert_eq!(s.public_key, "abcDEF123456=");
        assert!(!s.is_unknown());
    }

    #[test]
    fn link_status_unknown_for_empty_envelope() {
        let s = LinkStatus::from_value(&json!({}));
        assert!(s.is_unknown());
    }

    #[test]
    fn link_config_parses_public_host() {
        let v = json!({
            "enabled": false,
            "public_host": "wylde.example.com",
            "listen_port": 51821,
            "tunnel_addr": "192.0.2.1/24",
            "peer_subnet": "192.0.2.0/24",
            "restart_required": true,
        });
        let cfg = LinkConfig::from_value(&v);
        assert_eq!(cfg.public_host, "wylde.example.com");
        assert!(cfg.restart_required);
        assert_eq!(cfg.peer_subnet, "192.0.2.0/24");
    }

    #[test]
    fn peer_row_parses_handshake_and_keys() {
        let v = json!({
            "public_key": "ABCDEFGHIJKLMNOP",
            "label": "Pixel",
            "tunnel_ip": "192.0.2.42",
            "online": true,
            "last_handshake": "2026-05-29T10:00:00+00:00",
        });
        let p = PeerRow::from_value(&v);
        assert_eq!(p.label, "Pixel");
        assert!(p.online);
        assert!(p.short_key().ends_with('…'));
        assert!(p.short_key().starts_with("ABCDEFGHIJKL"));
    }

    #[test]
    fn peer_row_short_key_passes_through_when_short() {
        let p = PeerRow {
            public_key: "shortkey".into(),
            ..PeerRow::default()
        };
        assert_eq!(p.short_key(), "shortkey");
    }

    #[test]
    fn service_row_parses_envelope() {
        let v = json!({
            "name": "gateway",
            "description": "Wylde API gateway",
            "port": 8005,
        });
        let s = ServiceRow::from_value(&v);
        assert_eq!(s.name, "gateway");
        assert_eq!(s.port, 8005);
    }
}
