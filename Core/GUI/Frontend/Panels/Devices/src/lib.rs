//! Wylde Devices panel — gpui-era surface over `device_gate`.
//!
//! Wraps the nine `device_gate.*` actions the Svelte alpha drove
//! through `lib/api.js`.  Same flows, native widgets:
//!
//!   * **Paired list** — `device_gate.list_devices`, polled every 10 s
//!     so `last_seen` / `is_active` stays honest while the panel is
//!     open.  Rows show name, short fingerprint, paired-on date, tier
//!     pill, and a per-device recent-activity strip backed by
//!     `device_gate.recent_actions` (newest-first audit of pair / tier /
//!     rotate / revoke); the strip reads "No recent activity" until the
//!     first logged mutation lands.
//!   * **Pair a new device** — `device_gate.start_pairing` returns a
//!     6-digit code + Unix `expires_at`.  The panel renders the code
//!     prominently with a live countdown (1 s tick) and a QR matrix
//!     encoding `wylde://pair?code=<code>` so the mobile companion can
//!     scan it instead of typing.
//!   * **Permission tier** — segmented pill row (Read only · Tool use ·
//!     Full access) per device; click swaps via
//!     `device_gate.set_tier`.  The destructive-tool tier asks for an
//!     inline confirmation (no modal) so the user can't fat-finger a
//!     phone into write-access.
//!   * **Revoke** — per-row button with the same inline-confirmation
//!     pattern the Models panel uses for delete; `device_gate.revoke`
//!     fires once Yes is clicked.
//!   * **Rotate token** — per-row button → `device_gate.rotate_token`;
//!     the freshly-minted bearer renders inline (selectable text) so
//!     the user can copy it into a recovery channel.  No modal — the
//!     Models panel's delete-confirmation pattern stretches naturally
//!     to "here is the new secret, dismiss when copied".
//!
//! Cross-panel nav: the panel surfaces "View remote access" which
//! fires `wylde_gui_pipe::request_nav("core/remote_access")` so the
//! user can jump to the WyldeLink peer view (devices typically pair
//! via the same tunnel the RemoteAccess panel manages).
//!
//! ## Service surface
//!
//!   * `device_gate.recent_actions` — wired (2026-05-30).  The Python
//!     service records a `{action, timestamp, status}` entry on every
//!     successful pair / tier-change / rotate / revoke; the panel reads
//!     the newest few per device into the activity strip.

pub mod devices_panel;
pub mod ipc;
pub mod qr;

pub use devices_panel::DevicesPanel;
