//! YAML + env config — Rust port of `VPN/config.py` + `VPN/config.yaml`.
//!
//! The Python service reads `config.yaml` once at launcher start and
//! applies every value to the process environment, then `config.py`
//! reads from the env. The Rust port collapses that into a single load:
//! the YAML is read into [`Config`] at first access (cached behind a
//! `OnceLock`), with env-var overrides applied per-field so the
//! existing `VPN_*` / `LINK_*` / `PORT` env vars still take precedence.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_PORT: u16 = 8020;
const DEFAULT_LINK_LISTEN_PORT: u16 = 51821;
const DEFAULT_DNS_STUB_PORT: u16 = 5300;

#[derive(Debug, Clone)]
pub struct Config {
    pub wylde_root: PathBuf,

    /// HTTP control-plane port. `PORT`, defaults to 8020.
    pub port: u16,

    // ── Outbound VPN (wg0) ─────────────────────────────────────────────
    pub vpn_enabled: bool,
    pub vpn_endpoint: String,
    pub vpn_peer_pubkey: String,
    pub vpn_private_key: String,
    pub vpn_tunnel_addr: String,
    pub vpn_dns: String,
    pub vpn_allowed_ips: String,

    // ── WyldeLink inbound (wg1) ────────────────────────────────────────
    pub link_enabled: bool,
    pub link_private_key: String,
    pub link_tunnel_addr: String,
    pub link_listen_port: u16,
    pub link_stun_servers: Vec<String>,
    pub link_peer_subnet: String,
    pub link_public_host: String,
    pub link_token_ttl: u64,
    pub link_pair_rate_max: u32,
    pub link_pair_rate_win: u64,

    // ── Relay (TURN) ───────────────────────────────────────────────────
    pub link_relay_host: String,
    pub link_relay_port: u16,
    pub link_relay_user: String,
    pub link_relay_pass: String,
    pub link_relay_realm: String,
    pub link_relay_lifetime: u64,

    // ── DNS stub ───────────────────────────────────────────────────────
    pub dns_stub_host: String,
    pub dns_stub_port: u16,

    // ── Discovery (2.D) ────────────────────────────────────────────────
    pub mdns_enabled: bool,
    pub mdns_service_name: String,
    pub mdns_instance_name: String,
    pub ddns_enabled: bool,
    pub ddns_provider: String,
    pub ddns_domain: String,
    pub ddns_token: String,
    pub ddns_extra: std::collections::BTreeMap<String, String>,
    pub ddns_update_interval_s: u64,

    // ── Monitoring (2.D) ───────────────────────────────────────────────
    pub heartbeat_interval_s: u64,

    /// Where the peer store + STUN cache live. `LINK_DATA_DIR`, defaults
    /// to `<WYLDE_ROOT>/data/wylde-link` on Windows (Python defaults to
    /// `/data/wylde-link` which only makes sense on the Linux deploy).
    pub link_data_dir: PathBuf,
}

impl Config {
    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }

    fn load() -> Self {
        let wylde_root = std::env::var_os("WYLDE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        // The Python launcher applies config.yaml into the env before
        // config.py reads it. We replicate that by reading the YAML
        // here and using it as defaults; env vars (when present) win.
        let yaml = read_yaml(&wylde_root);

        let link_listen_port = env_u16(
            "LINK_LISTEN_PORT",
            yaml.link.listen_port.unwrap_or(DEFAULT_LINK_LISTEN_PORT),
        );

        let link_stun_servers_raw = std::env::var("LINK_STUN_SERVERS")
            .ok()
            .or_else(|| yaml.link.stun_servers.clone())
            .unwrap_or_else(|| "stun.l.google.com:19302,stun1.l.google.com:19302".to_string());
        let link_stun_servers: Vec<String> = link_stun_servers_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let link_data_dir = std::env::var_os("LINK_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| wylde_root.join("data").join("wylde-link"));

        Self {
            port: env_u16("PORT", yaml.service.port.unwrap_or(DEFAULT_PORT)),

            vpn_enabled: env_bool("VPN_ENABLED", yaml.vpn.enabled.unwrap_or(false)),
            vpn_endpoint: env_str(
                "VPN_ENDPOINT",
                yaml.vpn.endpoint.clone().unwrap_or_default(),
            ),
            vpn_peer_pubkey: env_str(
                "VPN_PEER_PUBKEY",
                yaml.vpn.peer_pubkey.clone().unwrap_or_default(),
            ),
            vpn_private_key: env_str(
                "VPN_PRIVATE_KEY",
                yaml.vpn.private_key.clone().unwrap_or_default(),
            ),
            vpn_tunnel_addr: env_str(
                "VPN_TUNNEL_ADDR",
                yaml.vpn
                    .tunnel_addr
                    .clone()
                    .unwrap_or_else(|| "10.8.0.2/24".to_string()),
            ),
            vpn_dns: env_str(
                "VPN_DNS",
                yaml.vpn
                    .dns
                    .clone()
                    .unwrap_or_else(|| "1.1.1.1".to_string()),
            ),
            vpn_allowed_ips: env_str(
                "VPN_ALLOWED_IPS",
                yaml.vpn
                    .allowed_ips
                    .clone()
                    .unwrap_or_else(|| "0.0.0.0/0, ::/0".to_string()),
            ),

            link_enabled: env_bool("LINK_ENABLED", yaml.link.enabled.unwrap_or(false)),
            link_private_key: env_str(
                "LINK_PRIVATE_KEY",
                yaml.link.private_key.clone().unwrap_or_default(),
            ),
            link_tunnel_addr: env_str(
                "LINK_TUNNEL_ADDR",
                yaml.link
                    .tunnel_addr
                    .clone()
                    .unwrap_or_else(|| "192.0.2.1/24".to_string()),
            ),
            link_listen_port,
            link_stun_servers,
            link_peer_subnet: env_str(
                "LINK_PEER_SUBNET",
                yaml.link
                    .peer_subnet
                    .clone()
                    .unwrap_or_else(|| "192.0.2.0/24".to_string()),
            ),
            link_public_host: env_str(
                "LINK_PUBLIC_HOST",
                yaml.link.public_host.clone().unwrap_or_default(),
            ),
            link_token_ttl: env_u64("LINK_TOKEN_TTL", yaml.link.token_ttl.unwrap_or(300)),
            link_pair_rate_max: env_u32("LINK_PAIR_RATE_MAX", yaml.link.pair_rate_max.unwrap_or(5)),
            link_pair_rate_win: env_u64(
                "LINK_PAIR_RATE_WIN",
                yaml.link.pair_rate_win.unwrap_or(60),
            ),

            link_relay_host: env_str(
                "LINK_RELAY_HOST",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.host.clone())
                    .unwrap_or_default(),
            ),
            link_relay_port: env_u16(
                "LINK_RELAY_PORT",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.port)
                    .unwrap_or(3478),
            ),
            link_relay_user: env_str(
                "LINK_RELAY_USER",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.user.clone())
                    .unwrap_or_else(|| "wylde".to_string()),
            ),
            link_relay_pass: env_str(
                "LINK_RELAY_PASS",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.password.clone())
                    .unwrap_or_default(),
            ),
            link_relay_realm: env_str(
                "LINK_RELAY_REALM",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.realm.clone())
                    .unwrap_or_else(|| "wylde.local".to_string()),
            ),
            link_relay_lifetime: env_u64(
                "LINK_RELAY_LIFETIME",
                yaml.link
                    .relay
                    .as_ref()
                    .and_then(|r| r.lifetime)
                    .unwrap_or(600),
            ),

            dns_stub_host: env_str(
                "DNS_STUB_HOST",
                yaml.dns_stub
                    .as_ref()
                    .and_then(|d| d.host.clone())
                    .unwrap_or_else(|| "0.0.0.0".to_string()),
            ),
            dns_stub_port: env_u16(
                "DNS_STUB_PORT",
                yaml.dns_stub
                    .as_ref()
                    .and_then(|d| d.port)
                    .unwrap_or(DEFAULT_DNS_STUB_PORT),
            ),

            mdns_enabled: env_bool(
                "MDNS_ENABLED",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.mdns.as_ref())
                    .and_then(|m| m.enabled)
                    .unwrap_or(true),
            ),
            mdns_service_name: env_str(
                "MDNS_SERVICE_NAME",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.mdns.as_ref())
                    .and_then(|m| m.service_name.clone())
                    .unwrap_or_else(|| "_wylde-link._udp".to_string()),
            ),
            mdns_instance_name: env_str(
                "MDNS_INSTANCE_NAME",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.mdns.as_ref())
                    .and_then(|m| m.instance_name.clone())
                    .unwrap_or_else(|| "Wylde Desktop".to_string()),
            ),
            ddns_enabled: env_bool(
                "DDNS_ENABLED",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.ddns.as_ref())
                    .and_then(|m| m.enabled)
                    .unwrap_or(false),
            ),
            ddns_provider: env_str(
                "DDNS_PROVIDER",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.ddns.as_ref())
                    .and_then(|m| m.provider.clone())
                    .unwrap_or_default(),
            ),
            ddns_domain: env_str(
                "DDNS_DOMAIN",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.ddns.as_ref())
                    .and_then(|m| m.domain.clone())
                    .unwrap_or_default(),
            ),
            ddns_token: env_str(
                "DDNS_TOKEN",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.ddns.as_ref())
                    .and_then(|m| m.token.clone())
                    .unwrap_or_default(),
            ),
            ddns_extra: yaml
                .discovery
                .as_ref()
                .and_then(|d| d.ddns.as_ref())
                .and_then(|m| m.extra.clone())
                .unwrap_or_default(),
            ddns_update_interval_s: env_u64(
                "DDNS_UPDATE_INTERVAL_S",
                yaml.discovery
                    .as_ref()
                    .and_then(|d| d.ddns.as_ref())
                    .and_then(|m| m.update_interval_s)
                    .unwrap_or(300),
            ),

            heartbeat_interval_s: env_u64(
                "HEARTBEAT_INTERVAL_S",
                yaml.monitoring
                    .as_ref()
                    .and_then(|m| m.heartbeat_interval_s)
                    .unwrap_or(30),
            ),

            link_data_dir,
            wylde_root,
        }
    }

    /// Path the Python launcher uses for the YAML file the GET/PATCH
    /// config actions read/write. Resolved under `WYLDE_ROOT/VPN/config.yaml`
    /// to match where the file actually lives in the repo.
    pub fn yaml_path(&self) -> PathBuf {
        self.wylde_root.join("VPN").join("config.yaml")
    }
}

#[derive(Debug, Default, Deserialize)]
struct YamlRoot {
    #[serde(default)]
    service: ServiceBlock,
    #[serde(default)]
    vpn: VpnBlock,
    #[serde(default)]
    link: LinkBlock,
    #[serde(default)]
    dns_stub: Option<DnsStubBlock>,
    #[serde(default)]
    discovery: Option<DiscoveryBlock>,
    #[serde(default)]
    monitoring: Option<MonitoringBlock>,
}

#[derive(Debug, Default, Deserialize)]
struct DiscoveryBlock {
    #[serde(default)]
    mdns: Option<MdnsBlock>,
    #[serde(default)]
    ddns: Option<DdnsBlock>,
}

#[derive(Debug, Default, Deserialize)]
struct MdnsBlock {
    enabled: Option<bool>,
    service_name: Option<String>,
    instance_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DdnsBlock {
    enabled: Option<bool>,
    provider: Option<String>,
    domain: Option<String>,
    token: Option<String>,
    update_interval_s: Option<u64>,
    /// Provider-specific keys (e.g. Cloudflare `zone_id`, `record_id`).
    /// Stored as a flat string map — values must be representable as
    /// strings, matching Python's `extra: dict` shape in `ddns.py`.
    extra: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
struct MonitoringBlock {
    heartbeat_interval_s: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ServiceBlock {
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct VpnBlock {
    enabled: Option<bool>,
    endpoint: Option<String>,
    peer_pubkey: Option<String>,
    private_key: Option<String>,
    tunnel_addr: Option<String>,
    dns: Option<String>,
    allowed_ips: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LinkBlock {
    enabled: Option<bool>,
    private_key: Option<String>,
    tunnel_addr: Option<String>,
    listen_port: Option<u16>,
    stun_servers: Option<String>,
    peer_subnet: Option<String>,
    public_host: Option<String>,
    token_ttl: Option<u64>,
    pair_rate_max: Option<u32>,
    pair_rate_win: Option<u64>,
    relay: Option<RelayBlock>,
}

#[derive(Debug, Default, Deserialize)]
struct RelayBlock {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    realm: Option<String>,
    lifetime: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct DnsStubBlock {
    host: Option<String>,
    port: Option<u16>,
}

fn read_yaml(wylde_root: &Path) -> YamlRoot {
    let path = wylde_root.join("VPN").join("config.yaml");
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_yaml::from_str::<YamlRoot>(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "wylde-vpn: failed to parse {}: {} — using defaults",
                    path.display(),
                    e
                );
                YamlRoot::default()
            }
        },
        Err(_) => YamlRoot::default(),
    }
}

// ── env helpers (mirror wylde-ollama/src/config.rs) ──────────────────────

fn env_str(name: &str, default: String) -> String {
    std::env::var(name).unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Patchable top-level keys under `link:` — mirrors the Python
/// `_LINK_PATCHABLE` map. Each entry pairs the key name with the type
/// the inbound JSON value gets coerced to before being written.
pub const LINK_PATCHABLE: &[(&str, PatchKind)] = &[
    ("enabled", PatchKind::Bool),
    ("public_host", PatchKind::Str),
    ("listen_port", PatchKind::Int),
    ("tunnel_addr", PatchKind::Str),
    ("peer_subnet", PatchKind::Str),
    ("token_ttl", PatchKind::Int),
    ("pair_rate_max", PatchKind::Int),
    ("pair_rate_win", PatchKind::Int),
];

/// Patchable keys under `link.relay:`.
pub const LINK_RELAY_PATCHABLE: &[(&str, PatchKind)] = &[
    ("host", PatchKind::Str),
    ("port", PatchKind::Int),
    ("user", PatchKind::Str),
    ("password", PatchKind::Str),
    ("realm", PatchKind::Str),
    ("lifetime", PatchKind::Int),
];

#[derive(Debug, Clone, Copy)]
pub enum PatchKind {
    Bool,
    Int,
    Str,
}

#[derive(Debug)]
pub struct LinkConfigPatchResult {
    pub view: Value,
    pub restart_required: bool,
}

/// Path-injected variant of [`patch_link_config`] for testing — lets
/// callers point at any YAML file, not just the process's
/// `Config::yaml_path()`. The runtime/restart-required comparison
/// still uses the singleton `Config` so the public view shape matches
/// `link.config.patch`'s production response.
pub fn patch_link_config_at(
    yaml_path: &std::path::Path,
    patch: &Value,
) -> anyhow::Result<LinkConfigPatchResult> {
    do_patch(yaml_path, patch)
}

/// Patch the link section of `VPN/config.yaml` in place. Mirrors the
/// Python `link_patch_config` handler:
///
/// * Read the YAML as a generic `serde_yaml::Value` so we preserve
///   sections we don't own (`vpn:`, `dns_stub:`, `discovery:`,
///   `nat:`, `monitoring:`).
/// * For every key in the patch body, look up its type in
///   `LINK_PATCHABLE` / `LINK_RELAY_PATCHABLE`. Unknown keys collect
///   into `invalid` — return BadRequest if any.
/// * Atomic write via `<path>.tmp` + rename, matching Python's
///   `_write_yaml_config`.
/// * Recompute the `restart_required` flag the same way Python does:
///   patched enabled/listen_port/public_host differ from the process's
///   current runtime values.
pub fn patch_link_config(patch: &Value) -> anyhow::Result<LinkConfigPatchResult> {
    let cfg = Config::get();
    do_patch(&cfg.yaml_path(), patch)
}

fn do_patch(yaml_path: &std::path::Path, patch: &Value) -> anyhow::Result<LinkConfigPatchResult> {
    let cfg = Config::get();
    let patch_obj = patch
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("request body must be an object"))?;

    let mut root: serde_yaml::Value = if yaml_path.exists() {
        let raw = std::fs::read_to_string(yaml_path)?;
        serde_yaml::from_str(&raw)?
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    let invalid = apply_patch(&mut root, patch_obj)?;
    if !invalid.is_empty() {
        anyhow::bail!("invalid or unknown fields: {}", invalid.join(", "));
    }

    // Atomic write.
    let tmp = yaml_path.with_extension("yaml.tmp");
    if let Some(parent) = yaml_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_yaml::to_string(&root)?;
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, yaml_path)?;

    let view = link_view_from_yaml(&root);
    let view_obj = view.as_object().expect("link view must be an object");
    let view_enabled = view_obj
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let view_listen_port = view_obj
        .get("listen_port")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let view_public_host = view_obj
        .get("public_host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let restart_required = view_enabled != cfg.link_enabled
        || view_listen_port as u16 != cfg.link_listen_port
        || view_public_host != cfg.link_public_host;

    let mut result_view = view;
    if let Some(obj) = result_view.as_object_mut() {
        obj.insert(
            "runtime".into(),
            json!({
                "enabled": cfg.link_enabled,
                "listen_port": cfg.link_listen_port,
                "public_host": cfg.link_public_host,
            }),
        );
        obj.insert("restart_required".into(), Value::Bool(restart_required));
    }

    Ok(LinkConfigPatchResult {
        view: result_view,
        restart_required,
    })
}

fn apply_patch(
    root: &mut serde_yaml::Value,
    patch: &serde_json::Map<String, Value>,
) -> anyhow::Result<Vec<String>> {
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("config.yaml root must be a mapping"))?;
    let link_key = serde_yaml::Value::String("link".into());
    let link = map
        .entry(link_key)
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let link_map = link
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("link section must be a mapping"))?;

    let mut invalid = Vec::new();
    for (key, value) in patch {
        if key == "relay" {
            let Some(relay_obj) = value.as_object() else {
                invalid.push("relay".into());
                continue;
            };
            let relay_key = serde_yaml::Value::String("relay".into());
            let relay = link_map
                .entry(relay_key)
                .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            let relay_map = relay
                .as_mapping_mut()
                .ok_or_else(|| anyhow::anyhow!("link.relay must be a mapping"))?;
            for (rk, rv) in relay_obj {
                match find_kind(LINK_RELAY_PATCHABLE, rk.as_str()) {
                    Some(kind) => match coerce(rv, kind) {
                        Some(yv) => {
                            relay_map.insert(serde_yaml::Value::String(rk.clone()), yv);
                        }
                        None => invalid.push(format!("relay.{rk}")),
                    },
                    None => invalid.push(format!("relay.{rk}")),
                }
            }
            continue;
        }
        match find_kind(LINK_PATCHABLE, key.as_str()) {
            Some(kind) => match coerce(value, kind) {
                Some(yv) => {
                    link_map.insert(serde_yaml::Value::String(key.clone()), yv);
                }
                None => invalid.push(key.clone()),
            },
            None => invalid.push(key.clone()),
        }
    }
    Ok(invalid)
}

fn find_kind(table: &[(&'static str, PatchKind)], key: &str) -> Option<PatchKind> {
    table
        .iter()
        .find_map(|(k, kind)| if *k == key { Some(*kind) } else { None })
}

fn coerce(value: &Value, kind: PatchKind) -> Option<serde_yaml::Value> {
    match kind {
        PatchKind::Bool => match value {
            Value::Bool(b) => Some(serde_yaml::Value::Bool(*b)),
            Value::String(s) => Some(serde_yaml::Value::Bool(matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            ))),
            Value::Number(n) => Some(serde_yaml::Value::Bool(n.as_i64().unwrap_or(0) != 0)),
            Value::Null => None,
            _ => None,
        },
        PatchKind::Int => match value {
            Value::Number(n) => n.as_i64().map(serde_yaml::Value::from),
            Value::String(s) => s.parse::<i64>().ok().map(serde_yaml::Value::from),
            Value::Bool(b) => Some(serde_yaml::Value::from(*b as i64)),
            _ => None,
        },
        PatchKind::Str => match value {
            Value::String(s) => Some(serde_yaml::Value::String(s.clone())),
            Value::Null => Some(serde_yaml::Value::String("".into())),
            Value::Number(n) => Some(serde_yaml::Value::String(n.to_string())),
            Value::Bool(b) => Some(serde_yaml::Value::String(b.to_string())),
            Value::Array(_) | Value::Object(_) => None,
        },
    }
}

fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => n
            .as_i64()
            .map(Value::from)
            .or_else(|| n.as_u64().map(Value::from))
            .or_else(|| {
                n.as_f64().map(|f| {
                    serde_json::Number::from_f64(f)
                        .map(Value::from)
                        .unwrap_or(Value::Null)
                })
            })
            .unwrap_or(Value::Null),
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => Value::Array(seq.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => serde_yaml::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                obj.insert(key, yaml_to_json(v));
            }
            Value::Object(obj)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

fn link_view_from_yaml(root: &serde_yaml::Value) -> Value {
    let link = root.get("link").cloned().unwrap_or(serde_yaml::Value::Null);
    let link_json = yaml_to_json(&link);
    let link_obj = link_json.as_object().cloned().unwrap_or_default();
    let relay = link_obj
        .get("relay")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    let relay_obj = relay.as_object().cloned().unwrap_or_default();

    json!({
        "enabled": link_obj.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "public_host": link_obj.get("public_host").and_then(Value::as_str).unwrap_or(""),
        "listen_port": link_obj.get("listen_port").and_then(Value::as_u64).unwrap_or(51821),
        "tunnel_addr": link_obj.get("tunnel_addr").and_then(Value::as_str).unwrap_or("192.0.2.1/24"),
        "peer_subnet": link_obj.get("peer_subnet").and_then(Value::as_str).unwrap_or("192.0.2.0/24"),
        "token_ttl": link_obj.get("token_ttl").and_then(Value::as_u64).unwrap_or(300),
        "pair_rate_max": link_obj.get("pair_rate_max").and_then(Value::as_u64).unwrap_or(5),
        "pair_rate_win": link_obj.get("pair_rate_win").and_then(Value::as_u64).unwrap_or(60),
        "relay": {
            "host": relay_obj.get("host").and_then(Value::as_str).unwrap_or(""),
            "port": relay_obj.get("port").and_then(Value::as_u64).unwrap_or(3478),
            "user": relay_obj.get("user").and_then(Value::as_str).unwrap_or("wylde"),
            "password": relay_obj.get("password").and_then(Value::as_str).unwrap_or(""),
            "realm": relay_obj.get("realm").and_then(Value::as_str).unwrap_or("wylde.local"),
            "lifetime": relay_obj.get("lifetime").and_then(Value::as_u64).unwrap_or(600),
        },
    })
}

/// Public view of the `/api/link/config` GET payload. Same shape Python
/// emits (the `_link_view` helper in `VPN/api.py`).
pub fn link_config_view(cfg: &Config) -> Value {
    json!({
        "enabled": cfg.link_enabled,
        "public_host": cfg.link_public_host,
        "listen_port": cfg.link_listen_port,
        "tunnel_addr": cfg.link_tunnel_addr,
        "peer_subnet": cfg.link_peer_subnet,
        "token_ttl": cfg.link_token_ttl,
        "pair_rate_max": cfg.link_pair_rate_max,
        "pair_rate_win": cfg.link_pair_rate_win,
        "relay": {
            "host": cfg.link_relay_host,
            "port": cfg.link_relay_port,
            "user": cfg.link_relay_user,
            "password": cfg.link_relay_pass,
            "realm": cfg.link_relay_realm,
            "lifetime": cfg.link_relay_lifetime,
        },
        "runtime": {
            "enabled": cfg.link_enabled,
            "listen_port": cfg.link_listen_port,
            "public_host": cfg.link_public_host,
        },
        // The runtime value is identical to the disk value in this
        // process (we re-read once at boot) so `restart_required` is
        // always false here. The Python service computes this against
        // the live process env; we'll regain parity once the PATCH
        // path lands and the on-disk vs in-process values can drift.
        "restart_required": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        // No WYLDE_ROOT, no env overrides — exercise the env-fallback
        // path so the Config::load() defaults align with config.py.
        let cfg = Config::load();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(!cfg.vpn_enabled);
        assert!(!cfg.link_enabled);
        assert_eq!(cfg.link_listen_port, DEFAULT_LINK_LISTEN_PORT);
        assert_eq!(cfg.dns_stub_port, DEFAULT_DNS_STUB_PORT);
        assert!(cfg
            .link_stun_servers
            .iter()
            .any(|s| s.contains("stun.l.google.com")));
    }

    #[test]
    fn link_config_view_has_expected_keys() {
        let cfg = Config::load();
        let v = link_config_view(&cfg);
        for key in [
            "enabled",
            "public_host",
            "listen_port",
            "tunnel_addr",
            "peer_subnet",
            "token_ttl",
            "pair_rate_max",
            "pair_rate_win",
            "relay",
            "runtime",
            "restart_required",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        for key in ["host", "port", "user", "password", "realm", "lifetime"] {
            assert!(v["relay"].get(key).is_some(), "missing relay.{key}");
        }
    }
}
