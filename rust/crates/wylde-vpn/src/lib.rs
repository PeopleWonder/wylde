//! WyldeLink VPN — peer-to-peer WireGuard tunnel + NAT traversal.
//!
//! Phase 2 of the Rust migration. **FOUNDATION SLICE** — this crate
//! ships the control-plane scaffolding for the eventual full port of
//! `Wylde/VPN/*.py`. What's wired up right now:
//!
//! * 16 pipe actions registered on `\\.\pipe\wylde-vpn` (matching the
//!   master plan's enumeration). Read-only control actions (`vpn.status`,
//!   `link.status`, `link.peers`, `link.config.get`, `link.qr`) plus
//!   the pure-crypto `vpn.keygen` are implemented. Mutating /
//!   network-bound actions return `service_unavailable` until the
//!   later sub-phases land (see [`actions`] for the inventory).
//! * Axum HTTP control plane on `127.0.0.1:8020` mirroring the Flask
//!   routes the Python service serves today, so Gateway callers and the
//!   GUI keep working unchanged while the strangler-fig flag is off.
//! * Runtime manifest at `data/manifests/wylde-vpn.json` + action
//!   contract at `data/contracts/actions/wylde-vpn.json` — same shape
//!   the Python service produces.
//! * Persistent peer store as a JSON file at `LINK_DATA_DIR/peers.json`,
//!   byte-equivalent with `VPN/peers/store.py` so a flip back to Python
//!   doesn't lose state.
//!
//! Phase 2.C promoted `link.stun` (STUN classification + TURN
//! allocation + endpoint poller) from `service_unavailable` and added
//! the [`nat`] module. See its sub-modules for per-piece detail.
//!
//! Phase 2.D lands the four background workers from
//! `Wylde/VPN/discovery/` + `Wylde/VPN/peers/push.py` +
//! `Wylde/VPN/monitoring/tunnel_health.py`:
//!
//! * [`discovery::mdns`] — LAN advertisement (`_wylde-link._udp.local.`)
//!   via `mdns-sd`, so phones on the same WiFi find the desktop.
//! * [`discovery::ddns`] — WAN reachability — 4 providers (DuckDNS,
//!   No-IP, Cloudflare, Afraid) sharing a stub-able HTTP client.
//! * [`peers::push`] — webhook push delivery + per-peer queue. Wired
//!   to the `EndpointUpdater::on_change` callback in `main.rs` so the
//!   home endpoint moving fires a `WyldeLink endpoint changed` push
//!   to every subscriber.
//! * [`monitoring::tunnel_health`] — periodic wg1 handshake-age
//!   classifier (online/stale/offline). Surfaced as a `handshakes`
//!   array on the `link.status` payload.
//!
//! Phase 2.E (Gateway cutover — `requests.post(VPN_URL, ...)` → IPC
//! `send_action("wylde-vpn", ...)`) is the only remaining VPN
//! sub-phase before the strangler-fig flag can flip to `rust` by
//! default.
//!
//! See `docs/wylde-rust-migration-master-plan.md` §Phase 2 for the full
//! scope and the strangler-fig flag (`WYLDE_WYLDE_VPN_IMPL=python|rust`,
//! defaults to `python`).

pub mod actions;
pub mod config;
pub mod discovery;
pub mod http;
pub mod monitoring;
pub mod nat;
pub mod pairing;
pub mod peers;
pub mod service;
pub mod tunnel;

pub use service::{install, reset_for_tests, stop};
