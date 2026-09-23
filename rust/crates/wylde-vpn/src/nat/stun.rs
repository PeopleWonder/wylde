//! STUN client — RFC 5389 binding requests + RFC-5780-lite NAT
//! classification. Port of `Wylde/VPN/nat/stun.py`.
//!
//! Behaviour matches the Python implementation byte-for-byte:
//!
//! * [`discover_endpoint`] sends one Binding Request per server, returns
//!   the first parsed XOR-MAPPED-ADDRESS.
//! * [`classify`] runs the four-test sequence and returns the same
//!   `{nat_type, mappings, recommend, public_endpoint}` shape Python
//!   emits. The four tests in order:
//!     * Test I  — bare binding request (mapped == local? → `open`).
//!     * Test II — `CHANGE-REQUEST 0x06` (response from different IP +
//!       port? → `full-cone`).
//!     * Test III — query a secondary server, compare mappings (mapping
//!       differs? → `symmetric`).
//!     * Test IV — `CHANGE-REQUEST 0x02` (response from same IP,
//!       different port? → `restricted-cone`; else `port-restricted`).
//! * [`punch_hole`] — one-sided UDP datagram burst (richer two-sided
//!   variant lives in [`super::hole_puncher`]).
//!
//! The on-wire codec is hand-rolled so the test matrix can swap out the
//! UDP transport entirely (see [`Transport`] + [`UdpTransport`]).

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAGIC: u32 = 0x2112_A442;
const BIND_REQUEST: u16 = 0x0001;
const BIND_RESPONSE: u16 = 0x0101;

const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;
const ATTR_CHANGE_REQUEST: u16 = 0x0003;
const ATTR_OTHER_ADDRESS: u16 = 0x802C;

const FAMILY_IPV4: u8 = 0x01;

/// Result of one Binding Request — the mapped address the STUN server
/// observed, plus a measured round-trip and (optionally) the OTHER-ADDRESS
/// the server published.
#[derive(Debug, Clone, PartialEq)]
pub struct StunResult {
    pub server: String,
    pub mapped_ip: String,
    pub mapped_port: u16,
    pub other_address: Option<(String, u16)>,
    pub rtt_ms: f64,
}

/// One mapping returned in the `classify()` response. Mirrors the Python
/// dict shape so the JSON serialisation matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mapping {
    pub server: String,
    pub ip: String,
    pub port: u16,
    pub rtt_ms: f64,
}

/// NAT classification categories — every variant has a string form that
/// matches Python's `nat_type` value verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatType {
    Open,
    FullCone,
    RestrictedCone,
    PortRestricted,
    Symmetric,
    Blocked,
    Unknown,
}

impl NatType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NatType::Open => "open",
            NatType::FullCone => "full-cone",
            NatType::RestrictedCone => "restricted-cone",
            NatType::PortRestricted => "port-restricted",
            NatType::Symmetric => "symmetric",
            NatType::Blocked => "blocked",
            NatType::Unknown => "unknown",
        }
    }
}

/// Recommended traversal strategy — matches Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recommend {
    Direct,
    HolePunch,
    Relay,
}

impl Recommend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Recommend::Direct => "direct",
            Recommend::HolePunch => "hole-punch",
            Recommend::Relay => "relay",
        }
    }
}

/// Classification result. `public_endpoint` is `None` only on `blocked`.
#[derive(Debug, Clone)]
pub struct Classification {
    pub nat_type: NatType,
    pub mappings: Vec<Mapping>,
    pub recommend: Recommend,
    pub public_endpoint: Option<String>,
}

impl Classification {
    /// JSON view matching `VPN/nat/stun.py::classify` exactly. Used as
    /// the body of the `link.stun` action payload.
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "nat_type": self.nat_type.as_str(),
            "mappings": self.mappings,
            "recommend": self.recommend.as_str(),
        });
        if let Some(ep) = &self.public_endpoint {
            obj.as_object_mut()
                .unwrap()
                .insert("public_endpoint".into(), Value::String(ep.clone()));
        }
        obj
    }
}

/// Transport abstraction — production wires this to [`UdpTransport`];
/// tests inject a mock that returns canned responses keyed by the
/// `(server, change_flags, local_port)` triple.
pub trait Transport: Send + Sync {
    fn probe(
        &self,
        server: &str,
        change_flags: u32,
        local_port: u16,
        timeout: Duration,
    ) -> Option<StunResult>;

    /// Best-effort lookup of the local egress IP. Returns `None` if
    /// the host has no usable route (offline test environment).
    fn local_ip(&self) -> Option<String> {
        None
    }
}

/// Real UDP transport. Uses a fresh `UdpSocket` per probe — same as the
/// Python client, which opens a socket inside `_probe()`.
pub struct UdpTransport;

impl Transport for UdpTransport {
    fn probe(
        &self,
        server: &str,
        change_flags: u32,
        local_port: u16,
        timeout: Duration,
    ) -> Option<StunResult> {
        udp_probe(server, change_flags, local_port, timeout)
    }

    fn local_ip(&self) -> Option<String> {
        local_egress_ip()
    }
}

/// Cheap mapped-address lookup. Returns the first successful response
/// across the supplied server list. Used by the periodic endpoint poll.
pub fn discover_endpoint(
    stun_servers: &[String],
    local_port: u16,
    timeout: Duration,
) -> Option<Value> {
    discover_endpoint_with(&UdpTransport, stun_servers, local_port, timeout)
}

pub fn discover_endpoint_with(
    transport: &dyn Transport,
    stun_servers: &[String],
    local_port: u16,
    timeout: Duration,
) -> Option<Value> {
    for server in stun_servers {
        if let Some(result) = transport.probe(server, 0, local_port, timeout) {
            return Some(json!({
                "ip": result.mapped_ip,
                "port": result.mapped_port,
                "server": server,
                "rtt_ms": result.rtt_ms,
                "via": server,
                "type": "xor-mapped",
            }));
        }
    }
    None
}

/// Run the four-test NAT classification. Matches `VPN/nat/stun.py::classify`
/// branch-for-branch.
pub fn classify(stun_servers: &[String], local_port: u16, timeout: Duration) -> Classification {
    classify_with(&UdpTransport, stun_servers, local_port, timeout)
}

pub fn classify_with(
    transport: &dyn Transport,
    stun_servers: &[String],
    local_port: u16,
    timeout: Duration,
) -> Classification {
    if stun_servers.is_empty() {
        return Classification {
            nat_type: NatType::Unknown,
            mappings: Vec::new(),
            recommend: Recommend::Relay,
            public_endpoint: None,
        };
    }
    let primary = &stun_servers[0];
    let secondary = stun_servers.get(1).unwrap_or(primary);

    // Test I — basic Binding Request.
    let Some(test_i) = transport.probe(primary, 0, local_port, timeout) else {
        return Classification {
            nat_type: NatType::Blocked,
            mappings: Vec::new(),
            recommend: Recommend::Relay,
            public_endpoint: None,
        };
    };

    let mut mappings = vec![Mapping {
        server: primary.clone(),
        ip: test_i.mapped_ip.clone(),
        port: test_i.mapped_port,
        rtt_ms: test_i.rtt_ms,
    }];
    let public_ep = format!("{}:{}", test_i.mapped_ip, test_i.mapped_port);

    // Local egress matches mapped → no NAT.
    if let Some(local) = transport.local_ip() {
        if local == test_i.mapped_ip {
            return Classification {
                nat_type: NatType::Open,
                mappings,
                recommend: Recommend::Direct,
                public_endpoint: Some(public_ep),
            };
        }
    }

    // Test II — CHANGE-REQUEST 0x06 (different IP + different port).
    // Python passes mapped_port as local_port only when caller supplied
    // a local_port. Replicate that conditional.
    let test_ii_local = if local_port != 0 {
        test_i.mapped_port
    } else {
        0
    };
    let test_ii = transport.probe(primary, 0x06, test_ii_local, timeout);
    if test_ii.is_some() {
        return Classification {
            nat_type: NatType::FullCone,
            mappings,
            recommend: Recommend::Direct,
            public_endpoint: Some(public_ep),
        };
    }

    // Test III — different server, compare mapping.
    if let Some(test_iii) = transport.probe(secondary, 0, local_port, timeout) {
        mappings.push(Mapping {
            server: secondary.clone(),
            ip: test_iii.mapped_ip.clone(),
            port: test_iii.mapped_port,
            rtt_ms: test_iii.rtt_ms,
        });
        if (&test_iii.mapped_ip, test_iii.mapped_port) != (&test_i.mapped_ip, test_i.mapped_port) {
            return Classification {
                nat_type: NatType::Symmetric,
                mappings,
                recommend: Recommend::Relay,
                public_endpoint: Some(public_ep),
            };
        }
    }

    // Test IV — CHANGE-REQUEST 0x02 (same IP, different port).
    let test_iv = transport.probe(primary, 0x02, local_port, timeout);
    if test_iv.is_some() {
        Classification {
            nat_type: NatType::RestrictedCone,
            mappings,
            recommend: Recommend::HolePunch,
            public_endpoint: Some(public_ep),
        }
    } else {
        Classification {
            nat_type: NatType::PortRestricted,
            mappings,
            recommend: Recommend::HolePunch,
            public_endpoint: Some(public_ep),
        }
    }
}

/// One-sided UDP datagram burst. The richer coordinated punch lives in
/// [`super::hole_puncher::punch`].
pub fn punch_hole(peer_endpoint: &str, local_port: u16, attempts: u32, interval: Duration) {
    let Some((host, port)) = parse_endpoint(peer_endpoint) else {
        tracing::debug!("stun::punch_hole: skipping malformed endpoint {peer_endpoint:?}");
        return;
    };
    let Ok(sock) = std::net::UdpSocket::bind(("0.0.0.0", local_port)) else {
        tracing::debug!("stun::punch_hole: bind on local_port {local_port} failed");
        return;
    };
    let target = format!("{host}:{port}");
    for _ in 0..attempts {
        let _ = sock.send_to(b"", &target);
        std::thread::sleep(interval);
    }
    tracing::debug!("stun::punch_hole sent {attempts} datagrams → {host}:{port}");
}

// ── wire helpers ──────────────────────────────────────────────────────────────

pub(crate) fn encode_request(transaction_id: [u8; 12], change_flags: u32) -> Vec<u8> {
    let mut attrs: Vec<u8> = Vec::new();
    if change_flags != 0 {
        // Attr: CHANGE-REQUEST, len 4, value u32 big-endian.
        attrs.extend_from_slice(&ATTR_CHANGE_REQUEST.to_be_bytes());
        attrs.extend_from_slice(&4u16.to_be_bytes());
        attrs.extend_from_slice(&change_flags.to_be_bytes());
    }
    let mut out = Vec::with_capacity(20 + attrs.len());
    out.extend_from_slice(&BIND_REQUEST.to_be_bytes());
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&transaction_id);
    out.extend(attrs);
    out
}

pub(crate) fn decode_response(data: &[u8], transaction_id: [u8; 12]) -> Option<StunResult> {
    if data.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&data[8..20]);
    if msg_type != BIND_RESPONSE || magic != MAGIC || txid != transaction_id {
        return None;
    }
    let body_end = 20usize.saturating_add(msg_len).min(data.len());
    let body = &data[20..body_end];

    let mut pos = 0usize;
    let mut mapped_ip = String::new();
    let mut mapped_port: u16 = 0;
    let mut other: Option<(String, u16)> = None;
    while pos + 4 <= body.len() {
        let attr_type = u16::from_be_bytes([body[pos], body[pos + 1]]);
        let attr_len = u16::from_be_bytes([body[pos + 2], body[pos + 3]]) as usize;
        let val_start = pos + 4;
        let val_end = val_start.saturating_add(attr_len).min(body.len());
        let val = &body[val_start..val_end];

        match attr_type {
            ATTR_MAPPED | ATTR_XOR_MAPPED => {
                if val.len() >= 8 {
                    let family = val[1];
                    let mut port = u16::from_be_bytes([val[2], val[3]]);
                    let mut ip_bytes = [val[4], val[5], val[6], val[7]];
                    if attr_type == ATTR_XOR_MAPPED {
                        port ^= ((MAGIC >> 16) & 0xFFFF) as u16;
                        let ip_int = u32::from_be_bytes(ip_bytes) ^ MAGIC;
                        ip_bytes = ip_int.to_be_bytes();
                    }
                    if family == FAMILY_IPV4 {
                        mapped_ip = std::net::Ipv4Addr::from(ip_bytes).to_string();
                        mapped_port = port;
                    }
                }
            }
            ATTR_OTHER_ADDRESS if val.len() >= 8 => {
                let port = u16::from_be_bytes([val[2], val[3]]);
                let ip = std::net::Ipv4Addr::new(val[4], val[5], val[6], val[7]).to_string();
                other = Some((ip, port));
            }
            _ => {}
        }
        // Attribute padding to 4-byte boundary (matches `(attr_len + 3) & ~3`).
        let padded = (attr_len + 3) & !3;
        pos = pos.saturating_add(4 + padded);
    }

    if mapped_ip.is_empty() {
        return None;
    }
    Some(StunResult {
        server: String::new(),
        mapped_ip,
        mapped_port,
        other_address: other,
        rtt_ms: 0.0,
    })
}

fn udp_probe(
    server: &str,
    change_flags: u32,
    local_port: u16,
    timeout: Duration,
) -> Option<StunResult> {
    let (host, port) = parse_endpoint_with_default(server, 3478)?;
    let txid: [u8; 12] = rand_txid();
    let payload = encode_request(txid, change_flags);

    let bind_addr: SocketAddr = format!("0.0.0.0:{local_port}").parse().ok()?;
    let sock = UdpSocket::bind(bind_addr).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;

    // Resolve the host before sending so DNS failures show up cleanly.
    let target = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let started = Instant::now();
    sock.send_to(&payload, target).ok()?;
    let mut buf = [0u8; 2048];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    let rtt_ms = ((started.elapsed().as_secs_f64() * 1000.0) * 100.0).round() / 100.0;

    let mut result = decode_response(&buf[..n], txid)?;
    result.server = server.to_string();
    result.rtt_ms = rtt_ms;
    Some(result)
}

fn parse_endpoint(server: &str) -> Option<(String, u16)> {
    let (h, p) = server.rsplit_once(':')?;
    let port: u16 = p.parse().ok()?;
    Some((h.to_string(), port))
}

fn parse_endpoint_with_default(server: &str, default_port: u16) -> Option<(String, u16)> {
    match server.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().ok()?;
            Some((h.to_string(), port))
        }
        None => Some((server.to_string(), default_port)),
    }
}

fn rand_txid() -> [u8; 12] {
    use rand_core::RngCore;
    let mut txid = [0u8; 12];
    rand_core::OsRng.fill_bytes(&mut txid);
    txid
}

fn local_egress_ip() -> Option<String> {
    // Best-effort: connect (UDP, no actual packets sent) to a public-ish
    // address and read back the kernel-assigned local endpoint. Matches
    // `_local_ip()` in `VPN/nat/stun.py`.
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let local = sock.local_addr().ok()?;
    Some(local.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock transport keyed by `(server, change_flags, local_port)`.
    /// Probes that don't match a registered key return `None` (mirrors
    /// a real timeout).
    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<std::collections::HashMap<(String, u32, u16), Option<StunResult>>>,
        local: Mutex<Option<String>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self::default()
        }
        fn with_response(
            self,
            server: &str,
            change_flags: u32,
            local_port: u16,
            result: Option<StunResult>,
        ) -> Self {
            self.responses
                .lock()
                .unwrap()
                .insert((server.to_string(), change_flags, local_port), result);
            self
        }
        fn with_local_ip(self, ip: &str) -> Self {
            *self.local.lock().unwrap() = Some(ip.to_string());
            self
        }
    }

    impl Transport for MockTransport {
        fn probe(
            &self,
            server: &str,
            change_flags: u32,
            local_port: u16,
            _timeout: Duration,
        ) -> Option<StunResult> {
            self.responses
                .lock()
                .unwrap()
                .get(&(server.to_string(), change_flags, local_port))
                .cloned()
                .unwrap_or(None)
        }

        fn local_ip(&self) -> Option<String> {
            self.local.lock().unwrap().clone()
        }
    }

    fn mock_result(server: &str, ip: &str, port: u16) -> StunResult {
        StunResult {
            server: server.to_string(),
            mapped_ip: ip.to_string(),
            mapped_port: port,
            other_address: None,
            rtt_ms: 1.0,
        }
    }

    #[test]
    fn encode_decode_round_trip_no_attrs() {
        let txid = [1u8; 12];
        let req = encode_request(txid, 0);
        assert_eq!(req.len(), 20);
        // Verify header decoding.
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BIND_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0); // no attrs
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), MAGIC);
        assert_eq!(&req[8..20], &txid);
    }

    #[test]
    fn encode_with_change_request_includes_attribute() {
        let txid = [2u8; 12];
        let req = encode_request(txid, 0x06);
        assert_eq!(req.len(), 28); // 20 hdr + 8 attr
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 8);
        assert_eq!(u16::from_be_bytes([req[20], req[21]]), ATTR_CHANGE_REQUEST);
        assert_eq!(u16::from_be_bytes([req[22], req[23]]), 4);
        assert_eq!(u32::from_be_bytes([req[24], req[25], req[26], req[27]]), 6);
    }

    #[test]
    fn decode_rejects_wrong_magic() {
        let txid = [3u8; 12];
        let mut resp = vec![0u8; 20];
        resp[0..2].copy_from_slice(&BIND_RESPONSE.to_be_bytes());
        resp[2..4].copy_from_slice(&0u16.to_be_bytes());
        resp[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        resp[8..20].copy_from_slice(&txid);
        assert!(decode_response(&resp, txid).is_none());
    }

    #[test]
    fn decode_xor_mapped_address() {
        // Construct a Binding Response with XOR-MAPPED 1.2.3.4:1234.
        let txid = [4u8; 12];
        let ip: u32 = 0x0102_0304;
        let port: u16 = 1234;
        let xor_port = port ^ ((MAGIC >> 16) & 0xFFFF) as u16;
        let xor_ip = ip ^ MAGIC;

        let mut resp = Vec::new();
        resp.extend_from_slice(&BIND_RESPONSE.to_be_bytes());
        resp.extend_from_slice(&12u16.to_be_bytes());
        resp.extend_from_slice(&MAGIC.to_be_bytes());
        resp.extend_from_slice(&txid);
        // Attr: XOR-MAPPED-ADDRESS
        resp.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0); // reserved
        resp.push(FAMILY_IPV4);
        resp.extend_from_slice(&xor_port.to_be_bytes());
        resp.extend_from_slice(&xor_ip.to_be_bytes());

        let r = decode_response(&resp, txid).unwrap();
        assert_eq!(r.mapped_ip, "1.2.3.4");
        assert_eq!(r.mapped_port, 1234);
    }

    #[test]
    fn classify_blocked_when_test_i_times_out() {
        let mock = MockTransport::new();
        let servers = vec!["primary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::Blocked);
        assert_eq!(c.recommend, Recommend::Relay);
        assert!(c.public_endpoint.is_none());
        assert!(c.mappings.is_empty());
    }

    #[test]
    fn classify_open_when_local_matches_mapped() {
        let mock = MockTransport::new().with_local_ip("9.9.9.9").with_response(
            "primary:3478",
            0,
            0,
            Some(mock_result("primary:3478", "9.9.9.9", 51820)),
        );
        let servers = vec!["primary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::Open);
        assert_eq!(c.recommend, Recommend::Direct);
        assert_eq!(c.public_endpoint.as_deref(), Some("9.9.9.9:51820"));
        assert_eq!(c.mappings.len(), 1);
    }

    #[test]
    fn classify_full_cone_when_change_request_responds() {
        let mock = MockTransport::new()
            .with_response(
                "primary:3478",
                0,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 1111)),
            )
            .with_response(
                "primary:3478",
                0x06,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 1111)),
            );
        let servers = vec!["primary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::FullCone);
        assert_eq!(c.recommend, Recommend::Direct);
        assert_eq!(c.public_endpoint.as_deref(), Some("1.2.3.4:1111"));
    }

    #[test]
    fn classify_symmetric_when_secondary_mapping_differs() {
        let mock = MockTransport::new()
            .with_response(
                "primary:3478",
                0,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 5000)),
            )
            // Test II: no response (not full-cone).
            .with_response(
                "secondary:3478",
                0,
                0,
                Some(mock_result("secondary:3478", "1.2.3.4", 6000)),
            );
        let servers = vec!["primary:3478".to_string(), "secondary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::Symmetric);
        assert_eq!(c.recommend, Recommend::Relay);
        assert_eq!(c.mappings.len(), 2);
    }

    #[test]
    fn classify_restricted_cone_when_change_port_responds() {
        let mock = MockTransport::new()
            .with_response(
                "primary:3478",
                0,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 7000)),
            )
            // Test II: no response.
            // Test III: secondary mapping matches (so not symmetric).
            .with_response(
                "secondary:3478",
                0,
                0,
                Some(mock_result("secondary:3478", "1.2.3.4", 7000)),
            )
            // Test IV: CHANGE-REQUEST 0x02 (same IP, different port) responds.
            .with_response(
                "primary:3478",
                0x02,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 7000)),
            );
        let servers = vec!["primary:3478".to_string(), "secondary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::RestrictedCone);
        assert_eq!(c.recommend, Recommend::HolePunch);
    }

    #[test]
    fn classify_port_restricted_when_all_change_tests_fail() {
        let mock = MockTransport::new()
            .with_response(
                "primary:3478",
                0,
                0,
                Some(mock_result("primary:3478", "1.2.3.4", 8000)),
            )
            // Test III: secondary mapping matches (so not symmetric).
            .with_response(
                "secondary:3478",
                0,
                0,
                Some(mock_result("secondary:3478", "1.2.3.4", 8000)),
            );
        // Tests II + IV return None (no entries registered).
        let servers = vec!["primary:3478".to_string(), "secondary:3478".to_string()];
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::PortRestricted);
        assert_eq!(c.recommend, Recommend::HolePunch);
    }

    #[test]
    fn classify_unknown_when_no_servers() {
        let mock = MockTransport::new();
        let servers: Vec<String> = Vec::new();
        let c = classify_with(&mock, &servers, 0, Duration::from_millis(1));
        assert_eq!(c.nat_type, NatType::Unknown);
        assert_eq!(c.recommend, Recommend::Relay);
    }

    #[test]
    fn discover_endpoint_returns_first_success() {
        let mock = MockTransport::new().with_response(
            "primary:3478",
            0,
            0,
            Some(mock_result("primary:3478", "5.6.7.8", 4444)),
        );
        let servers = vec!["deadbeef:3478".to_string(), "primary:3478".to_string()];
        let v = discover_endpoint_with(&mock, &servers, 0, Duration::from_millis(1)).unwrap();
        assert_eq!(v["ip"], "5.6.7.8");
        assert_eq!(v["port"], 4444);
        assert_eq!(v["server"], "primary:3478");
        assert_eq!(v["type"], "xor-mapped");
    }

    #[test]
    fn discover_endpoint_none_when_all_fail() {
        let mock = MockTransport::new();
        let servers = vec!["none:3478".to_string()];
        assert!(discover_endpoint_with(&mock, &servers, 0, Duration::from_millis(1)).is_none());
    }

    #[test]
    fn nat_type_strings_match_python() {
        // Hard-coded so a rename of any variant is caught by a test
        // failure — the Python pairing UX depends on these exact strings.
        assert_eq!(NatType::Open.as_str(), "open");
        assert_eq!(NatType::FullCone.as_str(), "full-cone");
        assert_eq!(NatType::RestrictedCone.as_str(), "restricted-cone");
        assert_eq!(NatType::PortRestricted.as_str(), "port-restricted");
        assert_eq!(NatType::Symmetric.as_str(), "symmetric");
        assert_eq!(NatType::Blocked.as_str(), "blocked");
    }

    #[test]
    fn classification_to_json_includes_public_endpoint() {
        let c = Classification {
            nat_type: NatType::FullCone,
            mappings: vec![Mapping {
                server: "s:3478".into(),
                ip: "1.1.1.1".into(),
                port: 100,
                rtt_ms: 1.0,
            }],
            recommend: Recommend::Direct,
            public_endpoint: Some("1.1.1.1:100".into()),
        };
        let v = c.to_json();
        assert_eq!(v["nat_type"], "full-cone");
        assert_eq!(v["recommend"], "direct");
        assert_eq!(v["public_endpoint"], "1.1.1.1:100");
        assert_eq!(v["mappings"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn classification_to_json_omits_public_endpoint_on_blocked() {
        let c = Classification {
            nat_type: NatType::Blocked,
            mappings: Vec::new(),
            recommend: Recommend::Relay,
            public_endpoint: None,
        };
        let v = c.to_json();
        assert!(v.get("public_endpoint").is_none());
    }

    #[test]
    fn udp_probe_round_trip_against_local_mock_server() {
        // Spin up a tiny synchronous STUN responder on a free local port.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            // Echo back a Binding Response with XOR-MAPPED 5.6.7.8:9999.
            let mut txid = [0u8; 12];
            txid.copy_from_slice(&buf[8..20]);
            let ip: u32 = 0x0506_0708;
            let port: u16 = 9999;
            let xor_port = port ^ ((MAGIC >> 16) & 0xFFFF) as u16;
            let xor_ip = ip ^ MAGIC;

            let mut resp = Vec::new();
            resp.extend_from_slice(&BIND_RESPONSE.to_be_bytes());
            resp.extend_from_slice(&12u16.to_be_bytes());
            resp.extend_from_slice(&MAGIC.to_be_bytes());
            resp.extend_from_slice(&txid);
            resp.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.push(0);
            resp.push(FAMILY_IPV4);
            resp.extend_from_slice(&xor_port.to_be_bytes());
            resp.extend_from_slice(&xor_ip.to_be_bytes());
            server.send_to(&resp, peer).unwrap();
            let _ = n;
        });

        let result = udp_probe(&addr.to_string(), 0, 0, Duration::from_secs(2)).unwrap();
        server_thread.join().unwrap();
        assert_eq!(result.mapped_ip, "5.6.7.8");
        assert_eq!(result.mapped_port, 9999);
        assert!(result.rtt_ms >= 0.0);
    }
}
