//! SSRF guard for the egress client.
//!
//! The `web` destination is a wildcard (`url_prefix: "https://"`), so any
//! caller can ask `egress.forward` to fetch an arbitrary URL. Without this
//! guard the gateway would happily reach loopback, RFC1918 private ranges,
//! the cloud-metadata endpoint (`169.254.169.254`), and other internal
//! hosts — a classic Server-Side Request Forgery hole.
//!
//! This module closes it in two moves:
//!
//! 1. **Resolve + classify.** Extract the host from the (already
//!    path-validated) URL. Resolve it *here* — the guard owns the DNS
//!    lookup so it controls the time-of-check/time-of-use window — then
//!    classify **every** returned address against a fail-closed deny-list
//!    (loopback / private / link-local / ULA / unspecified / multicast /
//!    reserved, plus the IPv4-mapped IPv6 forms of all of those).
//!
//! 2. **Pin.** Return the validated addresses so [`super::client`] can hand
//!    them to `reqwest::ClientBuilder::resolve_to_addrs`. `reqwest` then
//!    connects only to the address the guard approved — a re-resolve can't
//!    swap in an internal IP between the check and the connect (DNS
//!    rebinding).
//!
//! Per-destination knobs (read off the manifest, see [`super::destinations`]):
//!   * `host_allowlist` — when non-empty, the host must match one entry
//!     (exact, or suffix via a leading `*.`/`.`). Empty ⇒ any public host.
//!   * `allow_private`  — escape hatch for a *specific* destination that
//!     legitimately reaches a private/loopback host. Off by default, must
//!     never be set on a wildcard destination. When on, the deny-list is
//!     skipped for that destination (the host is still resolved + pinned).
//!
//! Global kill: `WYLDE_EGRESS_SSRF_BLOCK_PRIVATE=0` disables the deny-list
//! process-wide (fail-open opt-out for trusted single-host deployments).
//! Default is on (fail-closed).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    /// Host/IP is blocked by policy (deny-list or host_allowlist).
    #[error("{0}")]
    Denied(String),
    /// DNS resolution failed — treated as denied (fail-closed), never as a
    /// transport error that a caller might retry around.
    #[error("could not resolve host {0:?}: {1}")]
    Resolve(String, String),
}

/// A host that passed the guard, plus the resolved addresses to pin the
/// connection to.
#[derive(Debug, Clone)]
pub struct PinnedHost {
    pub host: String,
    pub addrs: Vec<SocketAddr>,
}

/// Is the deny-list enabled? Default yes; `WYLDE_EGRESS_SSRF_BLOCK_PRIVATE=0`
/// (or `false`/`off`/`no`) turns it off.
fn block_private_enabled() -> bool {
    match std::env::var("WYLDE_EGRESS_SSRF_BLOCK_PRIVATE") {
        Ok(v) => {
            let t = v.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("off")
                || t.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// Validate `url_str` against the deny-list + per-destination allowlist,
/// resolving the host and returning the pinned addresses on success.
///
/// `allow_private` skips the deny-list for this destination only (still
/// resolves + pins). The global env switch can also disable the deny-list.
pub async fn guard_target(
    url_str: &str,
    host_allowlist: &[String],
    allow_private: bool,
) -> Result<PinnedHost, SsrfError> {
    let url = reqwest::Url::parse(url_str)
        .map_err(|e| SsrfError::Denied(format!("invalid URL {url_str:?}: {e}")))?;

    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::Denied(format!("URL {url_str:?} has no host")))?
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(0);

    // 1. Per-destination host allowlist (cheapest, narrows before resolve).
    if !host_allowlist.is_empty() && !host_matches_allowlist(&host, host_allowlist) {
        return Err(SsrfError::Denied(format!(
            "host {host:?} is not in the destination host_allowlist"
        )));
    }

    let enforce = block_private_enabled() && !allow_private;

    // 2. Block internal-looking *names* up front (`.localhost`, bare
    //    `localhost`, single-label hosts that won't leave the box). This is
    //    belt-and-braces — the resolution check below is the real guard, but
    //    some of these never resolve to anything classifiable.
    if enforce {
        if let Some(reason) = internal_hostname_reason(&host) {
            return Err(SsrfError::Denied(format!(
                "host {host:?} is an internal name ({reason})"
            )));
        }
    }

    // 3. Resolve the host (the guard owns the lookup → controls TOCTOU).
    //    `lookup_host` handles IP-literal hosts too (returns them verbatim),
    //    so loopback/metadata literals flow through the same classifier.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| SsrfError::Resolve(host.clone(), e.to_string()))?
        .collect();

    if addrs.is_empty() {
        return Err(SsrfError::Resolve(
            host.clone(),
            "resolved to no addresses".into(),
        ));
    }

    // 4. Classify every resolved address. A single blocked address denies
    //    the whole request — fail-closed, no "some addresses are fine".
    if enforce {
        for sa in &addrs {
            if let Some(reason) = blocked_ip_reason(sa.ip()) {
                return Err(SsrfError::Denied(format!(
                    "host {host:?} resolves to {} address {} ({reason} is blocked)",
                    reason,
                    sa.ip()
                )));
            }
        }
    }

    Ok(PinnedHost { host, addrs })
}

/// Suffix/exact match for a `host_allowlist` entry.
///
/// * `example.com`   → exact host only.
/// * `*.example.com` → `example.com` and any sub-domain.
/// * `.example.com`  → same as `*.` form.
fn host_matches_allowlist(host: &str, allowlist: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    for raw in allowlist {
        let entry = raw.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if let Some(suffix) = entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')) {
            if host == suffix || host.ends_with(&format!(".{suffix}")) {
                return true;
            }
        } else if host == entry {
            return true;
        }
    }
    false
}

/// Reason a *hostname* (not yet resolved) is internal, or `None`.
fn internal_hostname_reason(host: &str) -> Option<&'static str> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return Some("empty");
    }
    if h == "localhost" || h.ends_with(".localhost") {
        return Some("localhost");
    }
    // Windows / mDNS internal suffixes that resolve to LAN hosts.
    if h.ends_with(".local") || h.ends_with(".internal") || h.ends_with(".lan") {
        return Some("internal-suffix");
    }
    None
}

/// Reason an IP is in a blocked class, or `None` if it is a routable public
/// address. Centralises the deny-list so v4 and the v4-in-v6 forms agree.
pub(crate) fn blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4_reason(v4),
        IpAddr::V6(v6) => blocked_v6_reason(v6),
    }
}

fn blocked_v4_reason(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback"); // 127.0.0.0/8
    }
    if ip.is_private() {
        return Some("private"); // 10/8, 172.16/12, 192.168/16
    }
    if ip.is_link_local() {
        return Some("link-local"); // 169.254.0.0/16 incl. 169.254.169.254 metadata
    }
    if ip.is_unspecified() {
        return Some("unspecified"); // 0.0.0.0
    }
    if ip.is_broadcast() {
        return Some("broadcast"); // 255.255.255.255
    }
    if ip.is_multicast() {
        return Some("multicast"); // 224.0.0.0/4
    }
    if ip.is_documentation() {
        return Some("documentation"); // 192.0.2/24, 198.51.100/24, 203.0.113/24
    }
    let o = ip.octets();
    if o[0] == 0 {
        return Some("this-network"); // 0.0.0.0/8
    }
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return Some("cgnat"); // 100.64.0.0/10 (RFC 6598 shared address space)
    }
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return Some("ietf-protocol"); // 192.0.0.0/24
    }
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return Some("benchmarking"); // 198.18.0.0/15
    }
    if o[0] >= 240 {
        return Some("reserved"); // 240.0.0.0/4 (incl. 255.255.255.255)
    }
    None
}

fn blocked_v6_reason(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback"); // ::1
    }
    if ip.is_unspecified() {
        return Some("unspecified"); // ::
    }
    if ip.is_multicast() {
        return Some("multicast"); // ff00::/8
    }
    // IPv4-mapped (::ffff:a.b.c.d) and the deprecated IPv4-compatible
    // (::a.b.c.d) forms can smuggle a private v4 through a v6 literal —
    // re-classify the embedded address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        if let Some(r) = blocked_v4_reason(v4) {
            return Some(r);
        }
    }
    if let Some(v4) = ip.to_ipv4() {
        if let Some(r) = blocked_v4_reason(v4) {
            return Some(r);
        }
    }
    let seg = ip.segments();
    if (seg[0] & 0xfe00) == 0xfc00 {
        return Some("unique-local"); // fc00::/7 (ULA)
    }
    if (seg[0] & 0xffc0) == 0xfe80 {
        return Some("link-local"); // fe80::/10
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> Option<&'static str> {
        blocked_ip_reason(s.parse().unwrap())
    }

    #[test]
    fn loopback_blocked() {
        assert_eq!(blocked("127.0.0.1"), Some("loopback"));
        assert_eq!(blocked("127.1.2.3"), Some("loopback"));
        assert_eq!(blocked("::1"), Some("loopback"));
    }

    #[test]
    fn cloud_metadata_blocked() {
        // The reason headline is the class — link-local — which covers
        // 169.254.169.254 (AWS/GCP/Azure IMDS).
        assert_eq!(blocked("169.254.169.254"), Some("link-local"));
        assert_eq!(blocked("169.254.0.1"), Some("link-local"));
    }

    #[test]
    fn private_ranges_blocked() {
        assert_eq!(blocked("10.0.0.1"), Some("private"));
        assert_eq!(blocked("10.255.255.255"), Some("private"));
        assert_eq!(blocked("172.16.0.1"), Some("private"));
        assert_eq!(blocked("172.31.255.1"), Some("private"));
        assert_eq!(blocked("192.168.1.1"), Some("private"));
    }

    #[test]
    fn unspecified_and_reserved_blocked() {
        assert_eq!(blocked("0.0.0.0"), Some("unspecified"));
        assert_eq!(blocked("0.1.2.3"), Some("this-network"));
        assert_eq!(blocked("100.64.0.1"), Some("cgnat"));
        assert_eq!(blocked("240.0.0.1"), Some("reserved"));
        assert_eq!(blocked("255.255.255.255"), Some("broadcast"));
    }

    #[test]
    fn v6_internal_blocked() {
        assert_eq!(blocked("::"), Some("unspecified"));
        assert_eq!(blocked("fc00::1"), Some("unique-local"));
        assert_eq!(blocked("fd12:3456::1"), Some("unique-local"));
        assert_eq!(blocked("fe80::1"), Some("link-local"));
    }

    #[test]
    fn v6_mapped_private_blocked() {
        // ::ffff:127.0.0.1 must not slip past as a "v6" address.
        assert_eq!(blocked("::ffff:127.0.0.1"), Some("loopback"));
        assert_eq!(blocked("::ffff:10.0.0.1"), Some("private"));
        assert_eq!(blocked("::ffff:169.254.169.254"), Some("link-local"));
    }

    #[test]
    fn public_ips_allowed() {
        assert_eq!(blocked("8.8.8.8"), None);
        assert_eq!(blocked("1.1.1.1"), None);
        assert_eq!(blocked("93.184.216.34"), None); // example.com
        assert_eq!(blocked("2606:4700:4700::1111"), None); // cloudflare v6
    }

    #[test]
    fn internal_names_flagged() {
        assert_eq!(internal_hostname_reason("localhost"), Some("localhost"));
        assert_eq!(internal_hostname_reason("foo.localhost"), Some("localhost"));
        assert_eq!(
            internal_hostname_reason("db.internal"),
            Some("internal-suffix")
        );
        assert_eq!(
            internal_hostname_reason("printer.local"),
            Some("internal-suffix")
        );
        assert_eq!(internal_hostname_reason("example.com"), None);
        assert_eq!(internal_hostname_reason("en.wikipedia.org"), None);
    }

    #[test]
    fn host_allowlist_exact_and_suffix() {
        let allow = vec!["api.example.com".to_string(), "*.wikipedia.org".to_string()];
        assert!(host_matches_allowlist("api.example.com", &allow));
        assert!(host_matches_allowlist("en.wikipedia.org", &allow));
        assert!(host_matches_allowlist("wikipedia.org", &allow));
        // exact entry must not match a sub-domain
        assert!(!host_matches_allowlist("evil.api.example.com", &allow));
        // unrelated host denied
        assert!(!host_matches_allowlist("example.org", &allow));
    }

    #[test]
    fn host_allowlist_dot_prefix_form() {
        let allow = vec![".example.com".to_string()];
        assert!(host_matches_allowlist("a.example.com", &allow));
        assert!(host_matches_allowlist("example.com", &allow));
        assert!(!host_matches_allowlist("notexample.com", &allow));
    }

    // ── Resolution-level guard tests (use IP literals → no real DNS) ──────

    #[tokio::test]
    async fn guard_blocks_loopback_literal() {
        let err = guard_target("https://127.0.0.1/secret", &[], false)
            .await
            .expect_err("loopback must be denied");
        assert!(matches!(err, SsrfError::Denied(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn guard_blocks_metadata_literal() {
        let err = guard_target("https://169.254.169.254/latest/meta-data/", &[], false)
            .await
            .expect_err("metadata must be denied");
        assert!(matches!(err, SsrfError::Denied(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn guard_blocks_private_literal() {
        let err = guard_target("https://192.168.1.1/", &[], false)
            .await
            .expect_err("private must be denied");
        assert!(matches!(err, SsrfError::Denied(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn guard_blocks_localhost_name() {
        let err = guard_target("https://localhost/admin", &[], false)
            .await
            .expect_err("localhost name must be denied");
        assert!(matches!(err, SsrfError::Denied(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn guard_allows_public_literal_and_pins_it() {
        // A public IP literal needs no DNS and must pass, pinned to itself.
        let pin = guard_target("https://93.184.216.34/", &[], false)
            .await
            .expect("public literal must pass");
        assert_eq!(pin.host, "93.184.216.34");
        assert!(pin
            .addrs
            .iter()
            .any(|a| a.ip().to_string() == "93.184.216.34"));
    }

    #[tokio::test]
    async fn guard_allow_private_escape_hatch() {
        // allow_private=true skips the deny-list for this destination.
        let pin = guard_target("https://127.0.0.1/", &[], true)
            .await
            .expect("allow_private must permit loopback");
        assert_eq!(pin.host, "127.0.0.1");
    }

    #[tokio::test]
    async fn guard_host_allowlist_denies_offlist_literal() {
        let allow = vec!["example.com".to_string()];
        let err = guard_target("https://93.184.216.34/", &allow, false)
            .await
            .expect_err("off-allowlist host must be denied");
        assert!(matches!(err, SsrfError::Denied(_)), "got {err:?}");
    }
}
