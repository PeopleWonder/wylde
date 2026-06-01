//! WireGuard tunnel lifecycle — Phase 2.B (boringtun + wintun).
//!
//! Replaces the Linux-only Python tunnel control (`Wylde/VPN/tunnel/`)
//! which shells out to `wg`, `wg-quick`, and `iptables`. The Rust port
//! drives a userspace WireGuard stack directly:
//!
//! * **Encryption:** [`boringtun::noise::Tunn`] holds the per-peer
//!   handshake state and produces / consumes UDP packets.
//! * **TUN device (Windows):** [`wintun::Adapter`] creates a virtual
//!   network interface that the OS treats like any other NIC; the
//!   userspace loop shuttles IP packets between the adapter and the
//!   boringtun encryptor.
//! * **TUN device (non-Windows):** the data plane is unavailable in
//!   2.B. `vpn.enable` returns a clean `service_unavailable` rather
//!   than half-instantiating state. Linux kernel-mode WireGuard is a
//!   later sub-phase per the master plan §Phase 2.
//!
//! ## Lifecycle invariants
//!
//! * `enable_*` is idempotent — a second call on an already-running
//!   tunnel returns `already_connected` and does not double-spawn the
//!   I/O workers (matches Python's `enable_vpn` behaviour).
//! * `disable_*` is symmetric — runs even if state is partially
//!   constructed (e.g. wintun adapter created but workers never spawned)
//!   so we always tear down cleanly.
//! * Service shutdown ([`crate::service::stop`]) calls into
//!   [`TunnelManager::shutdown_all`] so a SIGINT cleans up the wintun
//!   adapter.
//!
//! ## What's testable without admin / a real tunnel
//!
//! Everything in this module *except* the actual packet-shuttling
//! loops is tested through the [`backend::Backend`] trait — a stub
//! impl records the operations the manager would invoke. The boringtun
//! / wintun integration itself is exercised behind a `#[ignore]` test
//! that opts in via `WYLDE_VPN_LIVE=1`; without admin or with the DLL
//! missing it surfaces a clean error and the test self-skips.

pub mod backend;
pub mod datapath;
pub mod state;

pub use state::{LinkRuntime, TunnelManager, VpnRuntime};
