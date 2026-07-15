//! Auth tiers — Rust port of `Gateway/auth/`.
//!
//! Per Wylde Design Principle #16 (single auth boundary at the WyldeLink
//! VPN tunnel) the Gateway has exactly two ingress tiers:
//!
//! * **public** — no check at all. Only `/health`.
//! * **local**  — the caller IP must fall inside a configured CIDR
//!   block. The default block list covers loopback (`127.0.0.1/32`,
//!   `::1/128`), the legacy Wylde mesh range (`172.16.0.0/12`), and the
//!   WyldeLink CGNAT range (`100.64.0.0/10`) — so a remote peer tunneled
//!   in via WyldeLink appears as a `100.64/10` caller and passes the
//!   tier without a per-route credential.
//!
//! `require_device` is a finer-grained check layered on top of `local`
//! for the mobile-bound routes: it resolves the
//! `Authorization: Bearer <token>` header to a verified [`Device`] via
//! the device-gate pipe (results cached for 60s — see [`token_cache`])
//! and attaches the record to the request extensions so the handler can
//! read the device's tier. Mirrors `Gateway/auth/device.py`.
//!
//! All three are [`axum::middleware::from_fn`]-shaped — `async fn(Request,
//! Next) -> Response` — so a route declares its tier by layering the
//! matching function onto its router.

pub mod token_cache;

pub use token_cache::Device;

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use ipnet::IpNet;
use serde_json::Value;

use crate::envelopes::failure;
use crate::services::device_gate;
use crate::settings::{get_settings, GatewaySettings};
use token_cache::global as token_cache_global;

// ── require_public ─────────────────────────────────────────────────────

/// `public` tier — no check. Kept as an explicit, mountable middleware so
/// a route declares its tier the same way it would declare `local`.
/// Mirrors Python's no-op `require_public`.
pub async fn require_public(req: Request, next: Next) -> Response {
    next.run(req).await
}

// ── require_local ──────────────────────────────────────────────────────

/// `local` tier — pass only when the caller IP is inside the configured
/// local CIDR set. Rust port of `Gateway/auth/__init__.py::require_local`.
///
/// On rejection returns `403 auth_local_denied` with the canonical
/// `{ok: false, error: {code, message}}` envelope.
pub async fn require_local(req: Request, next: Next) -> Response {
    let settings = get_settings();
    let ip = client_ip(&req, &settings);
    if is_local_ip(&ip, &settings) {
        next.run(req).await
    } else {
        failure(
            "auth_local_denied",
            &format!("caller {ip} not in local CIDR allowlist"),
            StatusCode::FORBIDDEN,
        )
    }
}

/// Best-effort client IP. Honours `X-Forwarded-For` only when
/// `trust_forwarded_for` is set AND the direct peer is itself local —
/// matches `Gateway/auth/__init__.py::_client_ip`.
///
/// A missing `ConnectInfo` (e.g. a unit test driving the router with
/// `oneshot` and no connect info) defaults to `127.0.0.1`, matching
/// Python's `request.client.host if request.client else "127.0.0.1"`.
fn client_ip(req: &Request, settings: &GatewaySettings) -> String {
    let direct = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    if !settings.trust_forwarded_for {
        return direct;
    }
    let xff = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if xff.is_empty() {
        return direct;
    }
    // Only trust the forwarded chain when the direct hop is itself a
    // local peer (a known reverse proxy), never an arbitrary internet
    // caller spoofing the header.
    if !is_local_ip(&direct, settings) {
        return direct;
    }
    let first = xff.split(',').next().unwrap_or("").trim();
    if first.is_empty() {
        direct
    } else {
        first.to_owned()
    }
}

/// Return true when `ip` parses and falls inside any configured local
/// CIDR block. Port of `Gateway/auth/__init__.py::is_local_ip` — an
/// unparseable address is reported as non-local rather than raising.
pub fn is_local_ip(ip: &str, settings: &GatewaySettings) -> bool {
    match ip.parse::<IpAddr>() {
        Ok(addr) => local_networks(settings)
            .iter()
            .any(|net| net.contains(&addr)),
        Err(_) => false,
    }
}

/// Parse the comma-separated CIDR list into [`IpNet`] blocks, skipping
/// (with a warning) any entry that doesn't parse — mirrors Python's
/// `_local_networks_cached` "ignoring invalid CIDR" branch.
fn local_networks(settings: &GatewaySettings) -> Vec<IpNet> {
    settings
        .local_cidrs()
        .iter()
        .filter_map(|raw| match raw.parse::<IpNet>() {
            Ok(net) => Some(net),
            Err(_) => {
                tracing::warn!("auth: ignoring invalid CIDR {raw:?}");
                None
            }
        })
        .collect()
}

// ── require_device ─────────────────────────────────────────────────────

/// `device` tier — resolve the Bearer token to a verified [`Device`] and
/// attach it to the request extensions. Rust port of
/// `Gateway/auth/device.py::require_device`.
///
/// On success the verified [`Device`] is inserted into the request
/// extensions (the handler reads it via `Extension<Device>`) — the same
/// record Python attaches to `request.state.device_auth`.
pub async fn require_device(mut req: Request, next: Next) -> Response {
    match verify_bearer(req.headers()).await {
        Ok(device) => {
            req.extensions_mut().insert(device);
            next.run(req).await
        }
        Err(resp) => resp,
    }
}

/// Resolve `Authorization: Bearer <token>` to a [`Device`].
///
/// Checks the 60s [`token_cache`] first; on a miss it calls the
/// device-gate pipe and caches the verified record. Error mapping
/// mirrors `Gateway/auth/device.py`: a missing/garbled header is
/// `401 missing_token`; a token device-gate rejects (`400`/`404`) is
/// `401 invalid_token`; device-gate being unreachable is `503` (so the
/// mobile app retries rather than clearing a still-valid token).
async fn verify_bearer(headers: &HeaderMap) -> Result<Device, Response> {
    let token = match extract_bearer(headers) {
        Some(t) => t,
        None => {
            return Err(failure(
                "missing_token",
                "Bearer token required (Authorization: Bearer <token>)",
                StatusCode::UNAUTHORIZED,
            ));
        }
    };

    if let Some(device) = token_cache_global().get(&token).await {
        return Ok(device);
    }

    match device_gate::verify(&token).await {
        Ok(data) => {
            let device_id = data
                .get("device_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let tier = data
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if device_id.is_empty() || tier.is_empty() {
                return Err(failure(
                    "invalid_token",
                    "device-gate returned an empty record",
                    StatusCode::UNAUTHORIZED,
                ));
            }
            let device = Device { device_id, tier };
            token_cache_global().insert(token, device.clone()).await;
            Ok(device)
        }
        Err((status, _)) => {
            if status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST {
                Err(failure(
                    "invalid_token",
                    "device token is not recognised",
                    StatusCode::UNAUTHORIZED,
                ))
            } else {
                Err(failure(
                    "device_gate_unavailable",
                    &format!("device-gate returned {}", status.as_u16()),
                    StatusCode::SERVICE_UNAVAILABLE,
                ))
            }
        }
    }
}

/// Extract a Bearer token from the `Authorization` header.
///
/// Rust port of `Gateway/auth/device.py::_extract_bearer`: `None` when
/// the header is absent, the scheme isn't `bearer` (case-insensitive),
/// or the token segment is empty.
pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let (scheme, token) = raw.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let t = token.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use axum::{Extension, Router};
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// Build a `GatewaySettings` with an explicit CIDR set — lets the
    /// CIDR-logic tests run without touching the process settings cache.
    fn settings_with_cidrs(csv: &str) -> GatewaySettings {
        GatewaySettings {
            host: "127.0.0.1".into(),
            port: 8005,
            workers: 1,
            local_cidrs_csv: csv.into(),
            trust_forwarded_for: false,
            rate_limit_per_minute: 1000,
            audit_log_dir: PathBuf::from("."),
            audit_log_enabled: false,
            cors_origins_csv: String::new(),
            secrets_provider: "file".into(),
            secrets_strict_mode: false,
            egress_kill_switch_init: false,
        }
    }

    const DEFAULT_CIDRS: &str = "127.0.0.1/32,::1/128,172.16.0.0/12,100.64.0.0/10";

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", value.parse().unwrap());
        h
    }

    // ── is_local_ip — the CIDR allowlist logic ─────────────────────────

    #[test]
    fn is_local_ip_accepts_loopback() {
        let s = settings_with_cidrs(DEFAULT_CIDRS);
        assert!(is_local_ip("127.0.0.1", &s));
        assert!(is_local_ip("::1", &s));
    }

    #[test]
    fn is_local_ip_accepts_wyldelink_cgnat_range() {
        // Tunneled remote peers appear as 100.64.0.0/10 callers.
        let s = settings_with_cidrs(DEFAULT_CIDRS);
        assert!(is_local_ip("100.64.0.1", &s));
        assert!(is_local_ip("100.127.255.254", &s));
    }

    #[test]
    fn is_local_ip_accepts_legacy_mesh_range() {
        let s = settings_with_cidrs(DEFAULT_CIDRS);
        assert!(is_local_ip("172.16.0.5", &s));
        assert!(is_local_ip("172.31.255.1", &s));
    }

    #[test]
    fn is_local_ip_rejects_internet_addresses() {
        let s = settings_with_cidrs(DEFAULT_CIDRS);
        assert!(!is_local_ip("8.8.8.8", &s));
        assert!(!is_local_ip("203.0.113.7", &s));
        // Just outside the CGNAT block.
        assert!(!is_local_ip("100.128.0.1", &s));
    }

    #[test]
    fn is_local_ip_rejects_unparseable_address() {
        let s = settings_with_cidrs(DEFAULT_CIDRS);
        assert!(!is_local_ip("not-an-ip", &s));
        assert!(!is_local_ip("", &s));
    }

    #[test]
    fn local_networks_skips_invalid_cidr() {
        // The good entry parses, the junk one is dropped with a warning.
        let s = settings_with_cidrs("127.0.0.1/32,garbage,10.0.0.0/8");
        let nets = local_networks(&s);
        assert_eq!(nets.len(), 2);
    }

    // ── extract_bearer ─────────────────────────────────────────────────

    #[test]
    fn extract_bearer_happy_path() {
        let h = headers_with_auth("Bearer abc.def");
        assert_eq!(extract_bearer(&h), Some("abc.def".to_owned()));
    }

    #[test]
    fn extract_bearer_is_scheme_case_insensitive() {
        let h = headers_with_auth("bEaReR xyz");
        assert_eq!(extract_bearer(&h), Some("xyz".to_owned()));
    }

    #[test]
    fn extract_bearer_rejects_other_schemes_and_empties() {
        assert_eq!(extract_bearer(&headers_with_auth("Basic xyz")), None);
        assert_eq!(extract_bearer(&headers_with_auth("Bearer ")), None);
        assert_eq!(extract_bearer(&HeaderMap::new()), None);
    }

    // ── middleware integration ─────────────────────────────────────────

    async fn ok_handler() -> &'static str {
        "reached"
    }

    #[tokio::test]
    async fn require_public_lets_unauthenticated_requests_through() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(require_public));
        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_local_rejects_an_internet_caller() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(require_local));
        let mut req = Request::builder().uri("/x").body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 51000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "auth_local_denied");
    }

    #[tokio::test]
    async fn require_local_admits_a_cgnat_caller() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(require_local));
        let mut req = Request::builder().uri("/x").body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([100, 64, 0, 9], 4000))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_local_admits_loopback_when_connect_info_is_absent() {
        // No ConnectInfo => client_ip defaults to 127.0.0.1 => local.
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(require_local));
        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_device_rejects_a_request_with_no_bearer() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn(require_device));
        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "missing_token");
    }

    /// Echo the verified device id back so the test can prove the
    /// `require_device` middleware populated the request extension.
    async fn echo_device(Extension(device): Extension<Device>) -> String {
        device.device_id
    }

    #[tokio::test]
    async fn require_device_populates_the_request_device_extension() {
        // Pre-seed the token cache so verification resolves without a
        // live device-gate pipe — exercises the cache-hit path and lets
        // the handler observe the populated extension.
        let token = "auth-mod-test-token-extn";
        token_cache_global()
            .insert(
                token.to_owned(),
                Device {
                    device_id: "dev-extn-99".to_owned(),
                    tier: "tool_use".to_owned(),
                },
            )
            .await;

        let app = Router::new()
            .route("/x", get(echo_device))
            .route_layer(from_fn(require_device));
        let req = Request::builder()
            .uri("/x")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(
            &body[..],
            b"dev-extn-99",
            "handler must read the Device the middleware inserted"
        );
    }
}
