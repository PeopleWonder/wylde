//! Egress subsystem — outbound HTTP through allowlisted destinations.
//!
//! Rust port of `Gateway/egress/`. Three modules, three single concerns:
//!
//! * [`destinations`] — registry of allowlisted upstream URLs, scoped to
//!   the declaring component's manifest.
//! * [`kill_switch`] — process-wide flag that short-circuits every
//!   outbound call. Flipped via env at boot or `POST /api/egress/kill`.
//! * [`client`] — async `reqwest`-based client that performs the actual
//!   upstream request, injects auth headers from [`crate::secrets`],
//!   validates the path allowlist, and emits one audit line via
//!   [`crate::middleware::audit_log::emit_egress`].
//!
//! Routes in [`crate::routes::egress`] glue these into HTTP endpoints;
//! pipe actions in [`crate::pipe`] glue them onto the named-pipe
//! transport. Both paths share the egress code below — no duplicated
//! allowlist / kill-switch logic.

pub mod client;
pub mod destinations;
pub mod kill_switch;

pub use client::{EgressError, EgressResult};
pub use destinations::{list_destinations, reload, Destination, EgressDestinationError};
pub use kill_switch::{is_blocked, set_blocked};
