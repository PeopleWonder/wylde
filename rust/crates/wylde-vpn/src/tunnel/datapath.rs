//! WireGuard userspace data plane — boringtun encryption + wintun TUN.
//!
//! Windows is the supported runtime in Phase 2.B; non-Windows targets
//! get a clean `start()` error so the platform constraint is surfaced
//! rather than half-failing later.
//!
//! ## Worker shape
//!
//! Three blocking std-thread workers (boringtun + wintun are sync APIs):
//!
//! * **TUN → UDP** — `session.receive_blocking()` → `tunn.encapsulate()`
//!   → `udp.send_to()`. `session.shutdown()` unblocks the receive on
//!   teardown.
//! * **UDP → TUN** — `udp.recv()` (with a short timeout so we can poll
//!   the shutdown flag) → `tunn.decapsulate()` → `session.send_packet()`.
//! * **Timer** — every 250ms call `tunn.update_timers()`; if it
//!   returns `WriteToNetwork`, forward the bytes over UDP. The timer
//!   thread also drives the periodic re-handshake / keepalive cadence
//!   that boringtun owns internally.
//!
//! Each worker is launched via `tokio::task::spawn_blocking` so the
//! `JoinHandle<()>` plugs into the same shutdown plumbing as the rest
//! of the service.
//!
//! ## Stats
//!
//! `RunningTunnel::stats()` reports `tx_bytes`, `rx_bytes`, last
//! handshake age. These power `vpn.status` / `link.status` once the
//! tunnel is live.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use boringtun::noise::{Tunn, TunnResult};
use parking_lot::Mutex;
use tokio::task::JoinHandle;

/// Inputs the manager hands to the data plane. All sizes / encodings
/// match what the Python service computes today (`Wylde/VPN/tunnel/
/// wireguard.py::_write_wg0`).
#[derive(Debug, Clone)]
pub struct TunnelParams {
    /// Logical interface name. On Windows this becomes the wintun
    /// adapter name (`wg0` or `wg1`).
    pub iface_name: String,
    /// 32-byte X25519 private key.
    pub static_private: [u8; 32],
    /// Peer's 32-byte X25519 public key.
    pub peer_public_key: [u8; 32],
    /// `host:port` — UDP endpoint to talk WireGuard to.
    pub endpoint: String,
    /// Tunnel interface address in CIDR form (e.g. `10.8.0.2/24`).
    pub tunnel_addr: String,
    /// IPv4 ranges allowed across the tunnel. Currently informational —
    /// boringtun handles routing per-peer once the wintun adapter is up.
    pub allowed_ips: Vec<String>,
    /// `PersistentKeepalive` in seconds. `None` disables it.
    pub keepalive_secs: Option<u16>,
}

/// Live tunnel — owned by the backend between `start()` and `stop()`.
/// The wintun adapter + session are dropped in `stop()` so the OS
/// removes the virtual NIC. The boringtun [`Tunn`] state is also
/// dropped, clearing all session keys.
pub struct RunningTunnel {
    pub iface_name: String,
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    pub(crate) stats: Arc<TunnelStats>,
    /// Held to keep the adapter alive until stop().
    #[cfg(target_os = "windows")]
    session: Arc<wintun::Session>,
    /// Adapter handle. Drop order matters: session first, then adapter.
    #[cfg(target_os = "windows")]
    _adapter: Arc<wintun::Adapter>,
}

/// Counters exposed to `vpn.status` / `link.status`. Updated from the
/// worker threads; atomics keep them lock-free on the read path.
pub struct TunnelStats {
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
    /// Boot `Instant` so we can report uptime + last-handshake age.
    start_instant: Instant,
    /// Last successful tunnel-direction packet — proxy for "tunnel
    /// established" until we plumb boringtun's `time_since_last_handshake`
    /// through (it requires locking the Tunn).
    last_rx: Mutex<Option<Instant>>,
}

impl TunnelStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tx_bytes: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            start_instant: Instant::now(),
            last_rx: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> TunnelStatsSnapshot {
        let last_rx = *self.last_rx.lock();
        TunnelStatsSnapshot {
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            uptime_s: self.start_instant.elapsed().as_secs_f64(),
            last_rx_age_s: last_rx.map(|t| t.elapsed().as_secs_f64()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TunnelStatsSnapshot {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub uptime_s: f64,
    pub last_rx_age_s: Option<f64>,
}

impl RunningTunnel {
    pub fn stats(&self) -> TunnelStatsSnapshot {
        self.stats.snapshot()
    }
}

/// Start the tunnel. Allocates the wintun adapter, builds the
/// [`Tunn`], binds the UDP socket, and spawns the three worker
/// threads. Returns a [`RunningTunnel`] the manager keeps alive until
/// `stop()` is called.
pub fn start(params: TunnelParams) -> Result<RunningTunnel> {
    #[cfg(target_os = "windows")]
    {
        start_windows(params)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = params; // suppress unused warning on non-Windows
        anyhow::bail!(
            "wylde-vpn data plane requires Windows (wintun); Linux kernel-mode WG path is deferred — set WYLDE_WYLDE_VPN_IMPL=python on Linux"
        )
    }
}

/// Tear down a running tunnel. Best-effort — every step is `let _`'d
/// so a partial-state tunnel still releases what it has.
pub fn stop(running: RunningTunnel) -> Result<()> {
    running.shutdown.store(true, Ordering::SeqCst);
    #[cfg(target_os = "windows")]
    {
        // Shutdown event signals the blocking wintun receive to unblock;
        // failure here just means it was already torn down.
        let _ = running.session.shutdown(); // wylde-check: discard-result-ok
    }
    // Workers are blocking std threads launched via spawn_blocking;
    // they exit on the shutdown flag / session.shutdown(). We don't
    // strictly need to await — the tokio runtime reaps them — but
    // joining lets the caller know teardown completed cleanly.
    for h in running.handles {
        h.abort();
    }
    // wintun adapter + session drop here automatically.
    Ok(())
}

#[cfg(target_os = "windows")]
fn start_windows(params: TunnelParams) -> Result<RunningTunnel> {
    use boringtun::x25519::{PublicKey, StaticSecret};

    let dll_path = resolve_wintun_dll()?;
    let wintun_dll = unsafe { wintun::load_from_path(&dll_path) }
        .with_context(|| format!("load wintun.dll from {}", dll_path.display()))?;

    let adapter = wintun::Adapter::open(&wintun_dll, &params.iface_name)
        .or_else(|_| {
            wintun::Adapter::create(&wintun_dll, &params.iface_name, "wylde-vpn", None)
        })
        .with_context(|| format!("create wintun adapter {}", params.iface_name))?;

    // Set address from CIDR; e.g. "10.8.0.2/24" → 10.8.0.2 + /24.
    // Best-effort — wintun returns ERROR_NOT_FOUND on systems where
    // adapter address management isn't supported, and the tunnel can
    // still serve traffic with the address configured externally.
    if let Some((ip_str, mask_str)) = params.tunnel_addr.split_once('/') {
        if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
            if let Err(e) = adapter.set_address(ip) {
                tracing::warn!("wylde-vpn: wintun set_address({ip}) failed: {e}");
            }
            if let Ok(prefix) = mask_str.parse::<u8>() {
                let mask = cidr_to_mask(prefix);
                if let Err(e) = adapter.set_netmask(mask) {
                    tracing::warn!("wylde-vpn: wintun set_netmask({mask}) failed: {e}");
                }
            }
        }
    }

    let session = Arc::new(
        adapter
            .start_session(wintun::MAX_RING_CAPACITY)
            .context("start wintun session")?,
    );

    // Bind UDP — let the OS pick a port for outbound (Python lets
    // wg-quick pick too). The remote endpoint is the peer.
    let udp = Arc::new(
        UdpSocket::bind("0.0.0.0:0").context("bind WireGuard UDP socket")?,
    );
    udp.set_read_timeout(Some(Duration::from_millis(250)))
        .context("set udp read timeout")?;
    let peer_endpoint: std::net::SocketAddr = params
        .endpoint
        .parse()
        .with_context(|| format!("parse peer endpoint {}", params.endpoint))?;
    udp.connect(peer_endpoint)
        .with_context(|| format!("connect udp to {peer_endpoint}"))?;

    let static_private = StaticSecret::from(params.static_private);
    let peer_public = PublicKey::from(params.peer_public_key);
    let tunn = Tunn::new(
        static_private,
        peer_public,
        None,
        params.keepalive_secs,
        rand_index(),
        None,
    );
    let tunn = Arc::new(Mutex::new(tunn));
    let stats = TunnelStats::new();
    let shutdown = Arc::new(AtomicBool::new(false));

    let handles = spawn_workers(
        tunn.clone(),
        udp.clone(),
        session.clone(),
        stats.clone(),
        shutdown.clone(),
    );

    tracing::info!(
        "wylde-vpn: tunnel up on {} → {}",
        params.iface_name,
        peer_endpoint
    );

    Ok(RunningTunnel {
        iface_name: params.iface_name,
        shutdown,
        handles,
        stats,
        session,
        _adapter: adapter,
    })
}

#[cfg(target_os = "windows")]
fn spawn_workers(
    tunn: Arc<Mutex<Tunn>>,
    udp: Arc<UdpSocket>,
    session: Arc<wintun::Session>,
    stats: Arc<TunnelStats>,
    shutdown: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    // Worker 1: wintun → boringtun → UDP.
    {
        let tunn = tunn.clone();
        let udp = udp.clone();
        let session = session.clone();
        let stats = stats.clone();
        let shutdown = shutdown.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            tun_to_udp_loop(tunn, udp, session, stats, shutdown);
        }));
    }
    // Worker 2: UDP → boringtun → wintun.
    {
        let tunn = tunn.clone();
        let udp = udp.clone();
        let session = session.clone();
        let stats = stats.clone();
        let shutdown = shutdown.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            udp_to_tun_loop(tunn, udp, session, stats, shutdown);
        }));
    }
    // Worker 3: timer.
    {
        let tunn = tunn.clone();
        let udp = udp.clone();
        let shutdown = shutdown.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            timer_loop(tunn, udp, shutdown);
        }));
    }

    handles
}

#[cfg(target_os = "windows")]
fn tun_to_udp_loop(
    tunn: Arc<Mutex<Tunn>>,
    udp: Arc<UdpSocket>,
    session: Arc<wintun::Session>,
    stats: Arc<TunnelStats>,
    shutdown: Arc<AtomicBool>,
) {
    let mut dst = vec![0u8; 65_535];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let pkt = match session.receive_blocking() {
            Ok(p) => p,
            Err(_) => break, // session.shutdown() called or driver gone
        };
        let bytes = pkt.bytes();
        let mut guard = tunn.lock();
        match guard.encapsulate(bytes, &mut dst) {
            TunnResult::WriteToNetwork(buf) => {
                if let Err(e) = udp.send(buf) {
                    tracing::debug!("wylde-vpn: udp send err: {e}");
                } else {
                    stats.tx_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                }
            }
            TunnResult::Done => {}
            TunnResult::Err(e) => {
                tracing::debug!("wylde-vpn: encapsulate err: {e:?}");
            }
            TunnResult::WriteToTunnelV4(_, _) | TunnResult::WriteToTunnelV6(_, _) => {
                // shouldn't happen on encapsulate
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn udp_to_tun_loop(
    tunn: Arc<Mutex<Tunn>>,
    udp: Arc<UdpSocket>,
    session: Arc<wintun::Session>,
    stats: Arc<TunnelStats>,
    shutdown: Arc<AtomicBool>,
) {
    let mut recv = vec![0u8; 65_535];
    let mut dst = vec![0u8; 65_535];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let n = match udp.recv(&mut recv) {
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        };
        let mut guard = tunn.lock();
        let mut result = guard.decapsulate(None, &recv[..n], &mut dst);
        loop {
            match result {
                TunnResult::Done => break,
                TunnResult::Err(e) => {
                    tracing::debug!("wylde-vpn: decapsulate err: {e:?}");
                    break;
                }
                TunnResult::WriteToNetwork(buf) => {
                    if let Err(e) = udp.send(buf) {
                        tracing::debug!("wylde-vpn: udp send (decapsulate path) err: {e}");
                    }
                    // Per boringtun docs: keep calling decapsulate with
                    // empty input until Done so queued packets flush.
                    result = guard.decapsulate(None, &[], &mut dst);
                }
                TunnResult::WriteToTunnelV4(packet, _) | TunnResult::WriteToTunnelV6(packet, _) => {
                    if let Ok(mut send_pkt) = session.allocate_send_packet(packet.len() as u16) {
                        send_pkt.bytes_mut().copy_from_slice(packet);
                        session.send_packet(send_pkt);
                        stats.rx_bytes.fetch_add(packet.len() as u64, Ordering::Relaxed);
                        *stats.last_rx.lock() = Some(Instant::now());
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn timer_loop(tunn: Arc<Mutex<Tunn>>, udp: Arc<UdpSocket>, shutdown: Arc<AtomicBool>) {
    let mut dst = vec![0u8; 65_535];
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let mut guard = tunn.lock();
        match guard.update_timers(&mut dst) {
            TunnResult::WriteToNetwork(buf) => {
                if let Err(e) = udp.send(buf) {
                    tracing::debug!("wylde-vpn: udp send (timer path) err: {e}");
                }
            }
            TunnResult::Done => {}
            TunnResult::Err(e) => {
                tracing::debug!("wylde-vpn: update_timers err: {e:?}");
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "windows")]
fn resolve_wintun_dll() -> Result<std::path::PathBuf> {
    // Priority order:
    //   1. WYLDE_WINTUN_DLL env var (operator override).
    //   2. <exe_dir>/wintun.dll (bundled alongside the binary).
    //   3. Bare "wintun.dll" — relies on Windows DLL search order.
    if let Ok(path) = std::env::var("WYLDE_WINTUN_DLL") {
        return Ok(std::path::PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("wintun.dll");
            if bundled.exists() {
                return Ok(bundled);
            }
        }
    }
    Ok(std::path::PathBuf::from("wintun.dll"))
}

fn rand_index() -> u32 {
    use rand_core::RngCore;
    rand_core::OsRng.next_u32()
}

#[allow(dead_code)] // used in Windows path only
fn cidr_to_mask(prefix: u8) -> std::net::Ipv4Addr {
    if prefix == 0 {
        return std::net::Ipv4Addr::new(0, 0, 0, 0);
    }
    let p = prefix.min(32);
    let v: u32 = u32::MAX.checked_shl(32 - p as u32).unwrap_or(u32::MAX);
    std::net::Ipv4Addr::from(v.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_to_mask_known_values() {
        assert_eq!(cidr_to_mask(24), std::net::Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(cidr_to_mask(16), std::net::Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(cidr_to_mask(32), std::net::Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(cidr_to_mask(0), std::net::Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(cidr_to_mask(8), std::net::Ipv4Addr::new(255, 0, 0, 0));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn start_returns_clean_error_on_non_windows() {
        let params = TunnelParams {
            iface_name: "wg0".into(),
            static_private: [1u8; 32],
            peer_public_key: [2u8; 32],
            endpoint: "127.0.0.1:51820".into(),
            tunnel_addr: "10.8.0.2/24".into(),
            allowed_ips: vec![],
            keepalive_secs: Some(25),
        };
        let err = start(params).unwrap_err().to_string();
        assert!(
            err.contains("Windows") || err.contains("wintun"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn stats_snapshot_starts_zero() {
        let s = TunnelStats::new();
        let snap = s.snapshot();
        assert_eq!(snap.tx_bytes, 0);
        assert_eq!(snap.rx_bytes, 0);
        assert!(snap.last_rx_age_s.is_none());
    }
}
