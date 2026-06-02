//! URL safety / SSRF guard — exact port of `_validate_external_url` from the
//! Python `Extensions/Webcrawler/handler.py`.
//!
//! Rejects non-http(s) schemes and any host that resolves to a private,
//! loopback, link-local, multicast, reserved, metadata, or unspecified IP
//! range. The same checks as the Python staged service so the rewrite does
//! **not** widen the attack surface (plan risk #6).
//!
//! Parity notes vs. the Python `ipaddress` checks:
//!   * Python calls `socket.getaddrinfo(host, None)` and rejects if **any**
//!     resolved address is in a disallowed range. We do the same: literal IP
//!     hosts are checked directly (no DNS); domain hosts are resolved via
//!     `ToSocketAddrs` and every resolved address is checked.
//!   * The Python OR'd `is_private | is_loopback | is_link_local |
//!     is_multicast | is_reserved | is_unspecified`. `169.254.169.254` is
//!     link-local, so the disallowed-range branch catches it *before* the
//!     dedicated metadata branch — exactly as the Python does (the metadata
//!     branch is effectively unreachable for that literal but is preserved
//!     for parity). Either way the result is: **rejected**.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Return `None` if safe to fetch, else a short error string — the exact
/// messages the Python handler returns.
pub fn validate_external_url(url: &str) -> Option<String> {
    if url.is_empty() {
        return Some("URL must be a non-empty string".to_owned());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Some("URL must start with http:// or https://".to_owned());
    }
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Some("URL could not be parsed".to_owned()),
    };
    match parsed.host() {
        None => Some("URL missing hostname".to_owned()),
        // A literal IP host needs no DNS — `getaddrinfo` on a literal just
        // echoes it back, so checking it directly is behaviourally identical.
        Some(url::Host::Ipv4(ip)) => check_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => check_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(host)) => {
            // Port is irrelevant to the address check; 0 is fine.
            let addrs = match (host, 0u16).to_socket_addrs() {
                Ok(a) => a,
                Err(_) => return Some("Hostname could not be resolved".to_owned()),
            };
            let mut resolved_any = false;
            for sa in addrs {
                resolved_any = true;
                if let Some(err) = check_ip(sa.ip()) {
                    return Some(err);
                }
            }
            if !resolved_any {
                return Some("Hostname could not be resolved".to_owned());
            }
            None
        }
    }
}

/// Check one resolved address, mirroring the Python branch order: the
/// disallowed-range check first, then the dedicated metadata-endpoint check.
fn check_ip(ip: IpAddr) -> Option<String> {
    if is_disallowed(ip) {
        return Some("URL resolves to a disallowed address range".to_owned());
    }
    if ip.to_string() == "169.254.169.254" {
        return Some("URL resolves to a metadata endpoint".to_owned());
    }
    None
}

/// True when `ip` falls in any range the Python guard rejects.
fn is_disallowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_disallowed(v4),
        IpAddr::V6(v6) => v6_disallowed(v6),
    }
}

/// IPv4 — OR of every range Python's `ipaddress` flags non-global, matching
/// `is_private | is_loopback | is_link_local | is_multicast | is_reserved |
/// is_unspecified`. Rust 1.85's stable `Ipv4Addr` predicates cover the bulk;
/// the two manual checks fill the gaps Python's `is_private` set includes
/// that no single std predicate names (`0.0.0.0/8`, `192.0.0.0/24`).
fn v4_disallowed(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // `is_shared` / `is_benchmarking` / `is_reserved` are still unstable in
    // std (the `ip` feature), so those ranges are checked by hand below.
    ip.is_unspecified()        // 0.0.0.0
        || ip.is_loopback()    // 127.0.0.0/8
        || ip.is_private()     // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()  // 169.254.0.0/16 (incl. the metadata endpoint)
        || ip.is_multicast()   // 224.0.0.0/4
        || ip.is_broadcast()   // 255.255.255.255
        || ip.is_documentation() // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        || o[0] == 0           // 0.0.0.0/8  ("this network")
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40) // 100.64.0.0/10 (CGNAT, is_shared)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)    // 198.18.0.0/15 (is_benchmarking)
        || o[0] >= 240         // 240.0.0.0/4 (is_reserved + broadcast)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 IETF protocol assignments
}

/// IPv6 — reject unspecified (`::`), loopback (`::1`), multicast (`ff00::/8`),
/// unique-local (`fc00::/7`), and link-local (`fe80::/10`). IPv4-mapped
/// addresses are re-checked through the v4 path. (`is_unique_local` /
/// `is_unicast_link_local` are still unstable in std, so the masks are
/// applied manually on the first segment.)
fn v6_disallowed(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_disallowed(v4);
    }
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }
    let first = ip.segments()[0];
    (first & 0xfe00) == 0xfc00 // fc00::/7  unique local
        || (first & 0xffc0) == 0xfe80 // fe80::/10 link local
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── scheme / parse branches ──────────────────────────────────────────

    #[test]
    fn empty_url_rejected() {
        assert_eq!(
            validate_external_url(""),
            Some("URL must be a non-empty string".to_owned())
        );
    }

    #[test]
    fn non_http_scheme_rejected() {
        assert_eq!(
            validate_external_url("ftp://example.com/x"),
            Some("URL must start with http:// or https://".to_owned())
        );
        assert!(validate_external_url("file:///etc/passwd").is_some());
        assert!(validate_external_url("gopher://x").is_some());
    }

    // ── each disallowed-range branch (literal IPs — no DNS) ───────────────

    #[test]
    fn metadata_endpoint_rejected() {
        // The canonical cloud-metadata SSRF target. Link-local, so it trips
        // the disallowed-range branch (same as Python).
        assert!(validate_external_url("http://169.254.169.254/latest/meta-data/").is_some());
    }

    #[test]
    fn private_ranges_rejected() {
        for u in [
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "https://100.64.0.1/", // CGNAT
        ] {
            assert!(validate_external_url(u).is_some(), "should reject {u}");
        }
    }

    #[test]
    fn loopback_rejected() {
        assert!(validate_external_url("http://127.0.0.1/").is_some());
        assert!(validate_external_url("http://127.255.0.1/").is_some());
        assert!(validate_external_url("http://[::1]/").is_some());
    }

    #[test]
    fn link_local_rejected() {
        assert!(validate_external_url("http://169.254.1.1/").is_some());
        assert!(validate_external_url("http://[fe80::1]/").is_some());
    }

    #[test]
    fn multicast_rejected() {
        assert!(validate_external_url("http://224.0.0.1/").is_some());
        assert!(validate_external_url("http://[ff02::1]/").is_some());
    }

    #[test]
    fn reserved_rejected() {
        assert!(validate_external_url("http://240.0.0.1/").is_some());
    }

    #[test]
    fn unspecified_rejected() {
        assert!(validate_external_url("http://0.0.0.0/").is_some());
        assert!(validate_external_url("http://[::]/").is_some());
    }

    #[test]
    fn unique_local_v6_rejected() {
        assert!(validate_external_url("http://[fc00::1]/").is_some());
        assert!(validate_external_url("http://[fd12:3456::1]/").is_some());
    }

    #[test]
    fn ipv4_mapped_loopback_rejected() {
        // ::ffff:127.0.0.1 must be caught through the v4 re-check.
        assert!(validate_external_url("http://[::ffff:127.0.0.1]/").is_some());
    }

    // ── public addresses pass ─────────────────────────────────────────────

    #[test]
    fn public_ip_allowed() {
        assert_eq!(validate_external_url("http://8.8.8.8/"), None);
        assert_eq!(validate_external_url("https://1.1.1.1/path?q=1"), None);
    }

    #[test]
    fn public_v6_allowed() {
        assert_eq!(validate_external_url("http://[2606:4700:4700::1111]/"), None);
    }
}
