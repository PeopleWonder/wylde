//! Rate-limit middleware — fixed-window request cap, per device or client IP.
//!
//! Rust port of `Gateway/middleware/rate_limit.py`. `docs/r3_gateway_deferred.md`
//! billed this as a "sliding window", but the Python docstring is precise
//! and the window is *not* a true rolling one: it is a single 60-second
//! bucket per key, keyed on the calendar minute (`unix_secs / 60`) and
//! reset lazily on the first write of a new minute. This port mirrors
//! that exact math — a fixed window, not a sliding one.
//!
//! Bucket-key resolution order matches Python's `_bucket_key`:
//!
//!   1. A verified [`Device`] in the request extensions (attached
//!      upstream by [`crate::auth::require_device`]) — keyed
//!      `dev:<device_id>` so one misbehaving device can't starve another.
//!      Python keys the equivalent branch on `request.state.api_key_match`.
//!   2. The client IP — `ip:<addr>` — for everything else. A request with
//!      no `ConnectInfo` keys `ip:unknown`, matching Python's
//!      `request.client.host if request.client else "unknown"`.
//!
//! `/health` is exempt, matching Python's default `exempt_paths`.
//!
//! Mounted as the innermost global layer in [`crate::app::build_router`]
//! (via `axum::middleware::from_fn_with_state`), so it runs after trace +
//! audit and just before the route surface. Like Python's global
//! `RateLimitMiddleware`, it sits *outside* the per-route auth tier — so
//! in practice it keys by IP; the `dev:` branch activates whenever a
//! `Device` is already present in the extensions when the layer runs.
//!
//! ## Per-device tier — [`per_device_rate_limit`]
//!
//! The global layer keys by client IP. WyldeLink's CGNAT collapses every
//! paired device behind one tunnel IP, so on authenticated routes that IP
//! bucket is shared by every device on the link — one runaway device can
//! starve its peers. [`per_device_rate_limit`] is the fix: a per-route
//! layer, mounted *inside* `require_device` (so the verified [`Device`]
//! extension is populated), that re-keys the cap by `dev:<device_id>`.
//! Same fixed-window [`RateLimiter`] math, a separate process-wide
//! instance ([`device_limiter`]) sized by `WYLDE_RATE_LIMIT_DEVICE_PER_MIN`
//! (default 60). The global layer stays mounted for pre-auth traffic
//! (`/health`, pairing); the two layers are independent.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use crate::auth::Device;
use crate::envelopes::failure;

/// Paths that bypass the limiter entirely. Mirrors Python's default
/// `exempt_paths=("/health",)`.
const EXEMPT_PATHS: &[&str] = &["/health"];

/// Compact the bucket map once it grows past this many keys — bounds
/// memory for a process churning through distinct clients. Mirrors
/// Python's `len(self._buckets) > 4096` guard.
const COMPACT_THRESHOLD: usize = 4096;

/// One per-key counter: how many requests have landed in `minute`.
struct Bucket {
    count: u32,
    minute: i64,
}

impl Bucket {
    /// A fresh bucket. `minute = -1` can never equal a real calendar
    /// minute, so the first request always resets it to the live count
    /// — the same sentinel as Python's `_Bucket.minute = -1`.
    fn new() -> Self {
        Self {
            count: 0,
            minute: -1,
        }
    }
}

/// Fixed-window rate limiter. Cloning shares the backing bucket map (it
/// is an `Arc` inside), so every clone counts against the same buckets —
/// the equivalent of Python's single `RateLimitMiddleware` instance
/// owning one `_buckets` dict.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    limit: u32,
}

impl RateLimiter {
    /// Build a limiter capping each key at `per_minute` requests. A value
    /// below 1 is floored to 1, matching Python's
    /// `max(1, int(s.rate_limit_per_minute))`.
    pub fn new(per_minute: u32) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            limit: per_minute.max(1),
        }
    }

    /// Effective per-minute cap (post-floor).
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Register one request against `key` for calendar minute `now_min`.
    /// Returns `true` when the request is within the cap. Port of
    /// `RateLimitMiddleware._allow` — `now_min` is taken as a parameter
    /// (rather than read from the clock here) so the windowing is unit
    /// testable without mocking the system clock.
    fn check(&self, key: &str, now_min: i64) -> bool {
        let mut buckets = self.buckets.lock().expect("rate-limit buckets poisoned");
        let allowed = {
            let bucket = buckets.entry(key.to_owned()).or_insert_with(Bucket::new);
            if bucket.minute != now_min {
                bucket.minute = now_min;
                bucket.count = 0;
            }
            bucket.count = bucket.count.saturating_add(1);
            bucket.count <= self.limit
        };
        if buckets.len() > COMPACT_THRESHOLD {
            // Drop buckets untouched for more than a minute. Mirrors
            // Python's `_compact`: stale when `now_min - b.minute > 1`.
            buckets.retain(|_, b| now_min - b.minute <= 1);
        }
        allowed
    }
}

/// `axum::middleware::from_fn_with_state` handler. Rejects a request with
/// `429 rate_limited` once its key exceeds the per-minute cap; otherwise
/// hands it to `next`. Port of `RateLimitMiddleware.dispatch`.
pub async fn rate_limit(State(limiter): State<RateLimiter>, req: Request, next: Next) -> Response {
    if EXEMPT_PATHS.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    let key = bucket_key(&req);
    if limiter.check(&key, current_minute()) {
        next.run(req).await
    } else {
        failure(
            "rate_limited",
            &format!(
                "more than {} requests/min for key '{key}'",
                limiter.limit()
            ),
            StatusCode::TOO_MANY_REQUESTS,
        )
    }
}

/// Resolve the bucket key for `req`. Port of
/// `RateLimitMiddleware._bucket_key`.
fn bucket_key(req: &Request) -> String {
    if let Some(device) = req.extensions().get::<Device>() {
        return format!("dev:{}", device.device_id);
    }
    match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ci) => format!("ip:{}", ci.0.ip()),
        None => "ip:unknown".to_owned(),
    }
}

/// Current calendar minute — `unix_secs / 60`. Port of Python's
/// `int(time.time() // 60)`.
fn current_minute() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (secs / 60) as i64
}

// ── Per-device tier ────────────────────────────────────────────────────

/// Default per-device cap when [`DEVICE_RATE_LIMIT_ENV`] is unset.
const DEFAULT_DEVICE_PER_MIN: u32 = 60;

/// Env var overriding the per-device cap. Deliberately *not* a
/// `WYLDE_GATEWAY_*` settings key — the per-device tier is wired
/// independently of [`crate::settings::GatewaySettings`] so this port and
/// the Python one read the identical variable name.
const DEVICE_RATE_LIMIT_ENV: &str = "WYLDE_RATE_LIMIT_DEVICE_PER_MIN";

/// Read the per-device cap from the environment. A missing or
/// unparseable value falls back to [`DEFAULT_DEVICE_PER_MIN`].
fn device_rate_limit_per_min() -> u32 {
    std::env::var(DEVICE_RATE_LIMIT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DEVICE_PER_MIN)
}

/// Process-wide per-device limiter. One shared instance so a device's
/// request count is pooled across every per-device-tier route — the
/// `dev:<device_id>` bucket is the same for `chat/run_turn` and
/// `devices/me`, making the cap a true per-device tier rather than a
/// per-route budget a device could multiply by spreading its traffic.
pub fn device_limiter() -> RateLimiter {
    static LIMITER: OnceLock<RateLimiter> = OnceLock::new();
    LIMITER
        .get_or_init(|| RateLimiter::new(device_rate_limit_per_min()))
        .clone()
}

/// Per-route `axum::middleware::from_fn_with_state` handler — caps
/// requests per verified device.
///
/// Mount **inner to `require_device`** so the [`Device`] extension is
/// populated, with the same `.route_layer` shape as
/// [`crate::middleware::events`]. A request keyed `dev:<device_id>` past
/// the per-minute cap is rejected with the canonical `429 rate_limited`
/// envelope — byte-identical to the global layer's.
///
/// On a request with no [`Device`] extension (a route that mounts this
/// layer without `require_device` ahead of it) the layer is a graceful
/// no-op: the request passes through untouched rather than 500-ing.
pub async fn per_device_rate_limit(
    State(limiter): State<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    let key = match req.extensions().get::<Device>() {
        Some(device) => format!("dev:{}", device.device_id),
        // No verified device — nothing to key on. Pass through.
        None => return next.run(req).await,
    };
    if limiter.check(&key, current_minute()) {
        next.run(req).await
    } else {
        failure(
            "rate_limited",
            &format!(
                "more than {} requests/min for key '{key}'",
                limiter.limit()
            ),
            StatusCode::TOO_MANY_REQUESTS,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use serde_json::Value;
    use tower::ServiceExt;

    fn req(uri: &str) -> Request {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    async fn ok_handler() -> &'static str {
        "reached"
    }

    // ── RateLimiter::check — the fixed-window counter ───────────────────

    #[test]
    fn within_window_requests_pass() {
        let rl = RateLimiter::new(3);
        // Three requests inside the same minute, all within the cap of 3.
        assert!(rl.check("dev:phone-a", 100));
        assert!(rl.check("dev:phone-a", 100));
        assert!(rl.check("dev:phone-a", 100));
    }

    #[test]
    fn exceeding_the_limit_is_denied() {
        let rl = RateLimiter::new(2);
        assert!(rl.check("ip:10.0.0.1", 100));
        assert!(rl.check("ip:10.0.0.1", 100));
        // The third request in the same minute trips the cap.
        assert!(!rl.check("ip:10.0.0.1", 100));
        assert!(!rl.check("ip:10.0.0.1", 100));
    }

    #[test]
    fn window_slide_resets_the_count() {
        let rl = RateLimiter::new(2);
        // Exhaust the cap inside minute 100.
        assert!(rl.check("dev:x", 100));
        assert!(rl.check("dev:x", 100));
        assert!(!rl.check("dev:x", 100));
        // Minute 101 is a fresh bucket — the minute-100 requests do not
        // count against it, so the first request of the new window passes.
        assert!(rl.check("dev:x", 101));
        assert!(rl.check("dev:x", 101));
        assert!(!rl.check("dev:x", 101));
    }

    #[test]
    fn distinct_keys_get_independent_buckets() {
        let rl = RateLimiter::new(1);
        // One device exhausting its bucket must not affect another.
        assert!(rl.check("dev:a", 100));
        assert!(!rl.check("dev:a", 100));
        assert!(rl.check("dev:b", 100));
    }

    #[test]
    fn limit_below_one_is_floored_to_one() {
        // Mirrors Python's `max(1, int(s.rate_limit_per_minute))`.
        let rl = RateLimiter::new(0);
        assert_eq!(rl.limit(), 1);
        assert!(rl.check("k", 100));
        assert!(!rl.check("k", 100));
    }

    // ── bucket_key — Device extension vs client IP ──────────────────────

    #[test]
    fn bucket_key_prefers_the_device_extension() {
        let mut r = req("/api/chat/run_turn");
        r.extensions_mut().insert(Device {
            device_id: "phone-77".to_owned(),
            tier: "tool_use".to_owned(),
        });
        assert_eq!(bucket_key(&r), "dev:phone-77");
    }

    #[test]
    fn bucket_key_falls_back_to_unknown_without_connect_info() {
        // No Device, no ConnectInfo — matches Python's `ip:unknown`.
        assert_eq!(bucket_key(&req("/api/models")), "ip:unknown");
    }

    // ── middleware integration ──────────────────────────────────────────

    #[tokio::test]
    async fn allowed_request_reaches_the_handler() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .layer(from_fn_with_state(RateLimiter::new(5), rate_limit));
        let resp = app.oneshot(req("/x")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn exceeding_the_limit_returns_the_deny_envelope() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .layer(from_fn_with_state(RateLimiter::new(1), rate_limit));
        // The single slot is consumed by the first request.
        let first = app.clone().oneshot(req("/x")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        // The second request in the same minute is rejected with the
        // canonical `{ok:false, error:{code,message}}` envelope at 429.
        let second = app.oneshot(req("/x")).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(second.into_body(), 4096).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "rate_limited");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requests/min"));
    }

    #[tokio::test]
    async fn exempt_path_bypasses_the_limiter() {
        // `/health` is exempt: even with a cap of 1, repeated hits pass.
        let app = Router::new()
            .route("/health", get(ok_handler))
            .layer(from_fn_with_state(RateLimiter::new(1), rate_limit));
        for _ in 0..5 {
            let resp = app.clone().oneshot(req("/health")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    // ── per_device_rate_limit — the per-route per-device tier ───────────

    /// Build a request to `/x` carrying a verified [`Device`] extension —
    /// the state `require_device` leaves behind for the per-device layer.
    fn device_req(device_id: &str) -> Request {
        let mut r = req("/x");
        r.extensions_mut().insert(Device {
            device_id: device_id.to_owned(),
            tier: "tool_use".to_owned(),
        });
        r
    }

    /// One device spamming itself to the cap returns 429; a *second*
    /// device is untouched — distinct `dev:` buckets. This is the case
    /// the global IP-keyed layer cannot serve once WyldeLink's CGNAT
    /// collapses both devices onto one tunnel IP.
    #[tokio::test]
    async fn per_device_one_device_does_not_starve_another() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn_with_state(RateLimiter::new(1), per_device_rate_limit));

        // Device A: the single slot is consumed by the first request.
        let first = app.clone().oneshot(device_req("phone-a")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        // A's second request in the same minute trips its bucket.
        let denied = app.clone().oneshot(device_req("phone-a")).await.unwrap();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(denied.into_body(), 4096).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "rate_limited");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("key 'dev:phone-a'"),
            "the deny envelope must name the device key: {v}"
        );

        // Device B has its own bucket — A exhausting itself is irrelevant.
        let other = app.oneshot(device_req("phone-b")).await.unwrap();
        assert_eq!(other.status(), StatusCode::OK);
    }

    /// No [`Device`] extension on the request (a route that mounts the
    /// per-device layer without `require_device` ahead of it). Even with
    /// a cap of 1, every request passes — the layer is a graceful no-op,
    /// never a 500 and never a spurious 429.
    #[tokio::test]
    async fn per_device_layer_is_a_noop_without_a_device_extension() {
        let app = Router::new()
            .route("/x", get(ok_handler))
            .route_layer(from_fn_with_state(RateLimiter::new(1), per_device_rate_limit));
        for _ in 0..5 {
            let resp = app.clone().oneshot(req("/x")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// Regression: mounting the per-device `route_layer` must not disturb
    /// the global IP-keyed layer. `/health` stays exempt past the cap,
    /// and a non-exempt route is still IP-limited by the global layer.
    #[tokio::test]
    async fn global_ip_layer_unchanged_alongside_the_per_device_layer() {
        let app = Router::new()
            .route("/health", get(ok_handler))
            .route(
                "/api/chat/run_turn",
                get(ok_handler)
                    .route_layer(from_fn_with_state(RateLimiter::new(60), per_device_rate_limit)),
            )
            .layer(from_fn_with_state(RateLimiter::new(1), rate_limit));

        // `/health` is still exempt from the global limiter — repeated
        // hits pass even with the global cap at 1.
        for _ in 0..3 {
            let resp = app.clone().oneshot(req("/health")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        // The non-exempt route still hits the global IP cap: no device
        // extension, so the per-device layer no-ops and the global layer
        // keys `ip:unknown` — first request passes, the second is 429.
        let first = app.clone().oneshot(req("/api/chat/run_turn")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let denied = app.oneshot(req("/api/chat/run_turn")).await.unwrap();
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
