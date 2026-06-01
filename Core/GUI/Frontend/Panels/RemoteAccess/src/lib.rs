//! Wylde Remote Access panel — gpui-era surface over `wylde-vpn`
//! (WyldeLink).
//!
//! Lifts the Svelte `RemoteAccess.svelte` layout to native widgets and
//! drops every interactive surface the slice spec calls out:
//!
//!   * **WyldeLink VPN status** — `interface_up`, `listen_port`,
//!     `public_key` short hash, configured DDNS hostname (via the
//!     `/api/link/config` reply's `public_host`), connected peer count
//!     (counted from the peers reply since the status route doesn't
//!     ship one).
//!   * **Peer list** — `label`, `tunnel_ip`, online dot, last
//!     handshake, public-key short hash.  Clicking a peer fires
//!     `wylde_gui_pipe::request_nav("core/devices")` per the spec —
//!     pairing-tier and remote-access are sibling concerns.
//!   * **DDNS configuration** — surfaces `public_host` + a manual-setup
//!     hint.  No DDNS pipe verb exists yet; the panel renders a static
//!     externality strip pointing at the docs that document the
//!     current bootstrap flow.
//!   * **Port forwarding hint** — the Wylde user's network uses an eero 7
//!     configured via the mobile app, no web UI; the panel renders the
//!     fixed step list so the user can walk to their phone without
//!     leaving the panel.
//!   * **DNS rewrites** — static list of the loopback hosts the
//!     RemoteAccess flow relies on.  AdGuard integration is not yet
//!     piped; documented as an externality.
//!
//! Auto-refresh: a long-lived task wakes every 5 s, re-reads status +
//! peers concurrently, and projects the result.  When wylde-vpn is
//! down each card degrades to a "service offline" strip rather than
//! the whole panel reading "cannot reach wylde-vpn" — matches the
//! Dashboard's per-card degradation pattern.

pub mod ipc;
pub mod remote_access_panel;

pub use remote_access_panel::RemoteAccessPanel;
