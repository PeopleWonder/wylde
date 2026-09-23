//! Live availability for declared UI panels.
//!
//! A panel's *registration* (is it still declared on disk?) and its
//! *availability* (is the thing behind it actually there?) are two
//! different questions, and the GUI needs both. Registration is answered
//! by [`crate::discovery`]; this module answers availability.
//!
//! The rule the GUI enforces on top of this — "no silent dead panel"
//! (#239) — is that a panel is only ever rendered live when it is
//! *reachable*. Anything else renders as a status, never as a live panel
//! that silently fails. `Extensions/wylde-images` is the case that forced
//! this: a `transport: "none"` stub whose iframe pointed at a port whose
//! service had been extracted, so the panel rendered as though it worked
//! and failed only once the user clicked it.
//!
//! **Why a TCP connect and not an HTTP request.** The question is "is
//! anything listening on that loopback port", which a connect answers
//! exactly, with no HTTP client dependency in the bridge (the crate has
//! deliberately stayed reqwest-free). A server that accepts the
//! connection but 500s is *present*; that is a different failure and the
//! panel's own content surfaces it. The Shell keeps its richer
//! `wylde_webview::probe_url` HEAD/GET probe for the iframe it actually
//! mounts — this is the cheap, list-wide pass.
//!
//! **Loopback only.** Manifest load already refuses non-loopback panel
//! URLs (`manifest::validate_ui_panels`), but this module re-checks
//! before connecting so a regression there can never turn the bridge
//! into an outbound port scanner driven by manifest content.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a probe verdict is reused before the next read re-probes.
///
/// `extensions.list_panels` is a UI read that can fire per poll tick, so
/// an uncached probe would mean a TCP connect per panel per tick. Two
/// seconds keeps the list responsive to a service coming or going
/// (well inside the "no restart required" bar) while collapsing a burst
/// of reads into one connect.
pub const PROBE_TTL: Duration = Duration::from_secs(2);

/// Budget for one loopback connect. Loopback either answers immediately
/// or refuses immediately; this only bounds the pathological case (a
/// port in a half-open state), and it bounds the whole `list_panels`
/// read, so it stays short.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

/// Availability verdict for one declared panel. The `&'static str` forms
/// are the wire values the GUI switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Registered and something is listening — safe to render live.
    Live,
    /// Registered, but nothing is listening on its URL.
    Unreachable,
    /// Registered, but its host extension has no live process (disabled,
    /// crashed, still starting). Distinct from `Unreachable` because the
    /// user's remedy differs: enable/start the extension, not debug a port.
    NotRunning,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Live => "live",
            Availability::Unreachable => "unreachable",
            Availability::NotRunning => "not_running",
        }
    }

    /// Whether a panel in this state may be rendered as a live panel.
    /// Exactly one state qualifies — the point of the type.
    pub fn is_live(self) -> bool {
        matches!(self, Availability::Live)
    }
}

/// Decide a panel's availability from its host extension's lifecycle and
/// a reachability verdict.
///
/// Pure so the policy is testable without a socket.
///
/// `panel_only` is a `transport: "none"` extension: it has no process to
/// run, so its lifecycle status carries no information and reachability
/// is the whole answer. That is the `wylde-images` shape, and it is why
/// the status check cannot be the only gate — a panel-only extension is
/// permanently "disabled" yet its panel can be perfectly live.
pub fn classify(
    panel_only: bool,
    extension_running: bool,
    reachable: bool,
) -> (Availability, Option<String>) {
    if !panel_only && !extension_running {
        return (
            Availability::NotRunning,
            Some("its extension is not running".to_owned()),
        );
    }
    if reachable {
        (Availability::Live, None)
    } else {
        (
            Availability::Unreachable,
            Some("nothing is listening at its address".to_owned()),
        )
    }
}

/// Split an `http(s)://…` URL into `(host, port)`, applying the scheme's
/// default port. String parsing rather than the `url` crate, matching
/// [`crate::manifest`]'s deliberate choice — the failure mode is a
/// `None`, which callers treat as unreachable.
pub fn authority_of(url: &str) -> Option<(String, u16)> {
    // Only http(s) — anything else we cannot probe, so it is not live.
    let default_port = match url.split_once("://") {
        Some(("http", _)) => 80,
        Some(("https", _)) => 443,
        _ => return None,
    };
    let rest = url.split_once("://").map(|(_, r)| r)?;
    // Strip optional userinfo, then cut the host at the first path char.
    let after_userinfo = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let host_with_port = after_userinfo.split(['/', '?', '#']).next().unwrap_or("");
    if host_with_port.is_empty() {
        return None;
    }
    if let Some(tail) = host_with_port.strip_prefix('[') {
        // IPv6 literal — `[host]` or `[host]:port`.
        let (host, after) = tail.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_owned(), port));
    }
    match host_with_port.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host.to_owned(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((host_with_port.to_owned(), default_port)),
    }
}

/// Resolve a loopback host literal to an address. Returns `None` for
/// anything not on the loopback interface — the guard that keeps this
/// from connecting anywhere a manifest names.
fn loopback_addr(host: &str, port: u16) -> Option<SocketAddr> {
    match host {
        "127.0.0.1" | "localhost" => Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
        "::1" => Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)),
        _ => None,
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, (Instant, bool)>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Drop every cached verdict so the next [`reachable`] call re-probes.
/// Used by tests and after a catalog change that could move a port.
pub fn invalidate_cache() {
    if let Ok(mut guard) = CACHE.lock() {
        *guard = None;
    }
}

fn cached(url: &str) -> Option<bool> {
    let guard = CACHE.lock().ok()?;
    let cache = guard.as_ref()?;
    let (at, verdict) = cache.entries.get(url)?;
    (at.elapsed() < PROBE_TTL).then_some(*verdict)
}

fn remember(url: &str, verdict: bool) {
    if let Ok(mut guard) = CACHE.lock() {
        let cache = guard.get_or_insert_with(Cache::default);
        cache
            .entries
            .insert(url.to_owned(), (Instant::now(), verdict));
    }
}

/// Is something listening at `url`? `false` for a URL that isn't a
/// loopback http(s) address at all — we cannot verify it, so we refuse
/// to claim it is live.
pub async fn reachable(url: &str) -> bool {
    if let Some(hit) = cached(url) {
        return hit;
    }
    let verdict = match authority_of(url).and_then(|(h, p)| loopback_addr(&h, p)) {
        Some(addr) => matches!(
            tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
            Ok(Ok(_))
        ),
        None => false,
    };
    remember(url, verdict);
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parses_the_shapes_manifests_actually_use() {
        assert_eq!(
            authority_of("http://127.0.0.1:8015"),
            Some(("127.0.0.1".to_owned(), 8015))
        );
        assert_eq!(
            authority_of("http://localhost:5678/rest/"),
            Some(("localhost".to_owned(), 5678))
        );
        // Scheme default ports.
        assert_eq!(
            authority_of("http://127.0.0.1"),
            Some(("127.0.0.1".to_owned(), 80))
        );
        assert_eq!(
            authority_of("https://localhost"),
            Some(("localhost".to_owned(), 443))
        );
        // IPv6 literal, with and without a port.
        assert_eq!(
            authority_of("http://[::1]:9000"),
            Some(("::1".to_owned(), 9000))
        );
        assert_eq!(authority_of("http://[::1]/x"), Some(("::1".to_owned(), 80)));
        // Userinfo is stripped, not mistaken for the host.
        assert_eq!(
            authority_of("http://u:p@127.0.0.1:7000"),
            Some(("127.0.0.1".to_owned(), 7000))
        );
    }

    #[test]
    fn authority_rejects_what_it_cannot_parse() {
        assert_eq!(authority_of("ftp://127.0.0.1:21"), None);
        assert_eq!(authority_of("127.0.0.1:8015"), None);
        assert_eq!(authority_of("http://"), None);
        // A non-numeric port is a parse failure, not a default-port fallback.
        assert_eq!(authority_of("http://127.0.0.1:not-a-port"), None);
    }

    #[test]
    fn only_loopback_hosts_resolve_to_an_address() {
        assert!(loopback_addr("127.0.0.1", 1).is_some());
        assert!(loopback_addr("localhost", 1).is_some());
        assert!(loopback_addr("::1", 1).is_some());
        // The guard: manifest content can never aim the probe off-box.
        assert!(loopback_addr("attacker.example", 80).is_none());
        assert!(loopback_addr("10.0.0.5", 80).is_none());
        // Not fooled by a loopback-prefixed hostname.
        assert!(loopback_addr("127.0.0.1.evil.com", 80).is_none());
    }

    #[test]
    fn classify_gates_a_live_render_on_reachability_alone_for_panel_only() {
        // The wylde-images shape: transport=none, so the extension is
        // permanently "not running" — yet a reachable panel is live.
        assert_eq!(classify(true, false, true).0, Availability::Live);
        // And an unreachable one is unavailable, not live. This is the
        // dead-Images case (#239): registered, port 8015 dead.
        let (state, detail) = classify(true, false, false);
        assert_eq!(state, Availability::Unreachable);
        assert!(detail.is_some(), "an unavailable panel must carry a reason");
    }

    #[test]
    fn classify_reports_a_stopped_extension_as_not_running() {
        let (state, detail) = classify(false, false, false);
        assert_eq!(state, Availability::NotRunning);
        assert!(detail.unwrap().contains("not running"));
        // A running extension whose port is dead is still unreachable —
        // the process being up does not make the panel live.
        assert_eq!(classify(false, true, false).0, Availability::Unreachable);
        assert_eq!(classify(false, true, true).0, Availability::Live);
    }

    #[test]
    fn exactly_one_state_permits_a_live_render() {
        assert!(Availability::Live.is_live());
        assert!(!Availability::Unreachable.is_live());
        assert!(!Availability::NotRunning.is_live());
    }

    #[test]
    fn wire_values_are_stable() {
        // The GUI switches on these strings; a rename must break here.
        assert_eq!(Availability::Live.as_str(), "live");
        assert_eq!(Availability::Unreachable.as_str(), "unreachable");
        assert_eq!(Availability::NotRunning.as_str(), "not_running");
    }

    #[tokio::test]
    async fn a_dead_port_is_unreachable_and_a_live_listener_is_reachable() {
        invalidate_cache();
        // Bind a listener, then probe it: reachable.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let live_url = format!("http://127.0.0.1:{port}");
        assert!(reachable(&live_url).await, "a bound port is reachable");

        // Drop the listener and probe a fresh URL string (so the TTL
        // cache can't answer): unreachable.
        drop(listener);
        invalidate_cache();
        assert!(
            !reachable(&live_url).await,
            "the port stops being reachable once nothing is listening"
        );
    }

    #[tokio::test]
    async fn a_non_loopback_url_is_never_claimed_live() {
        invalidate_cache();
        assert!(!reachable("http://attacker.example/").await);
        assert!(!reachable("not-a-url").await);
    }
}
