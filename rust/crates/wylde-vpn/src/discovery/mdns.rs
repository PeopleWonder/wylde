//! mDNS LAN advertisement — port of `VPN/discovery/mdns.py`.
//!
//! Advertises `_wylde-link._udp.local.` so phones on the same WiFi can
//! find the desktop without an external coordinator. TXT records carry
//! the gateway port + version + service name (matching the Python
//! `MdnsAdvertiser` so existing mobile clients keep working unchanged).
//!
//! Built on [`mdns-sd`](https://crates.io/crates/mdns-sd) — single
//! transitive (`if-addrs`); the daemon owns its own thread.
//!
//! Lifecycle: `start()` registers (returns `false` quietly if the local
//! IP can't be resolved or the daemon refuses to register — same fail-
//! soft behaviour as the Python module). `stop()` unregisters cleanly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Mutex;

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Default service-type name. Mirrors `_wylde-link._udp` from Python.
pub const DEFAULT_SERVICE_TYPE: &str = "_wylde-link._udp.local.";
/// Default instance/friendly name shown in mDNS browsers.
pub const DEFAULT_INSTANCE_NAME: &str = "Wylde Desktop";

/// Configuration knobs — mirrored from `MdnsAdvertiser.__init__`.
#[derive(Debug, Clone)]
pub struct MdnsConfig {
    pub hostname: String,
    pub port: u16,
    pub service_type: String,
    pub instance_name: String,
    pub gateway_port: u16,
    pub version: String,
}

impl Default for MdnsConfig {
    fn default() -> Self {
        Self {
            hostname: "wylde-desktop".to_string(),
            port: 51821,
            service_type: DEFAULT_SERVICE_TYPE.to_string(),
            instance_name: DEFAULT_INSTANCE_NAME.to_string(),
            gateway_port: 8021,
            version: "1.0".to_string(),
        }
    }
}

/// Long-lived handle holding the daemon + service info so `stop()` can
/// unregister cleanly.
pub struct MdnsAdvertiser {
    cfg: MdnsConfig,
    state: Mutex<Option<Running>>,
}

struct Running {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAdvertiser {
    pub fn new(cfg: MdnsConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(None),
        }
    }

    /// Register the service. Returns `true` on success; `false` (plus a
    /// warn log) if anything fails. Idempotent — a second call when
    /// already running returns `true` without re-registering.
    pub fn start(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.is_some() {
            return true;
        }
        let ip = match local_ip() {
            Some(ip) => ip,
            None => {
                tracing::warn!("mdns: no local IPv4 resolved; skipping advertisement");
                return false;
            }
        };
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("mdns: ServiceDaemon init failed: {e}");
                return false;
            }
        };
        let mut props: Vec<(&str, String)> = Vec::new();
        let gw = self.cfg.gateway_port.to_string();
        let version = self.cfg.version.clone();
        props.push(("gateway", gw));
        props.push(("version", version));
        props.push(("service", "wylde-link".to_string()));

        let info = match ServiceInfo::new(
            &self.cfg.service_type,
            &self.cfg.instance_name,
            &format!("{}.local.", self.cfg.hostname),
            IpAddr::V4(ip),
            self.cfg.port,
            &props[..],
        ) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("mdns: ServiceInfo build failed: {e}");
                return false;
            }
        };

        let fullname = info.get_fullname().to_string();
        if let Err(e) = daemon.register(info) {
            tracing::warn!("mdns: register failed: {e}");
            return false;
        }
        tracing::info!("mdns: registered {} on port {}", fullname, self.cfg.port);
        *state = Some(Running { daemon, fullname });
        true
    }

    /// Unregister + tear down the daemon. Idempotent.
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(running) = state.take() {
            let _ = running.daemon.unregister(&running.fullname);
            let _ = running.daemon.shutdown();
        }
    }

    /// Whether [`start`] has been called and the daemon is currently
    /// registered.
    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }
}

/// Probe the local IPv4 the same way `MdnsAdvertiser._local_ip` does:
/// open a connect-style UDP socket toward a public address (no packets
/// actually leave the host) and read `getsockname()` to learn the
/// kernel's chosen source IP. Falls back to `127.0.0.1` if offline.
pub fn local_ip() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    let target: SocketAddr = "8.8.8.8:80".parse().ok()?;
    match sock.connect(target).and_then(|_| sock.local_addr()) {
        Ok(addr) => match addr.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        },
        Err(_) => Some(Ipv4Addr::LOCALHOST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_config_defaults_match_python_constants() {
        let c = MdnsConfig::default();
        assert_eq!(c.service_type, "_wylde-link._udp.local.");
        assert_eq!(c.instance_name, "Wylde Desktop");
        assert_eq!(c.port, 51821);
        assert_eq!(c.gateway_port, 8021);
        assert_eq!(c.version, "1.0");
    }

    #[test]
    fn local_ip_returns_a_value() {
        // Doesn't need a route to 8.8.8.8 — the kernel just resolves
        // the source IP from its routing table without sending. CI on
        // an air-gapped box still picks 127.0.0.1.
        let ip = local_ip();
        assert!(ip.is_some(), "local_ip should always resolve to something");
    }

    #[test]
    fn advertiser_construction_does_not_start() {
        let adv = MdnsAdvertiser::new(MdnsConfig::default());
        assert!(!adv.is_running());
    }

    // start() / stop() round-trip is exercised in integration tests
    // (lib unit-test runs would race UDP 5353 binding on hosts where a
    // system mDNSResponder is already running).
}
