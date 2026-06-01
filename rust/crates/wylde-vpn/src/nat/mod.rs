//! NAT traversal — Rust port of `Wylde/VPN/nat/*.py`.
//!
//! Phase 2.C scope. The module is intentionally library-shaped — no
//! state owned at this layer, callers (the `link.stun` / `link.connect`
//! action handlers, plus the endpoint updater) wire things together.
//!
//! * [`stun`] — RFC 5389 binding requests + RFC-5780-lite NAT
//!   classification. Hand-rolled wire protocol to stay byte-equivalent
//!   with `VPN/nat/stun.py` (master plan §9 risk #11).
//! * [`turn`] — RFC 5766 short-credential allocation. Hand-rolled for
//!   the same reason; `webrtc-rs/turn` would add ~30 transitive deps
//!   for a single Allocate handshake.
//! * [`hole_puncher`] — coordinated UDP datagram burst, port of
//!   `VPN/nat/hole_puncher.py`.
//! * [`endpoint_updater`] — periodic STUN probe + change notification,
//!   port of `VPN/nat/endpoint_updater.py`.

pub mod endpoint_updater;
pub mod hole_puncher;
pub mod stun;
pub mod turn;
