//! Router builder + middleware wiring.
//!
//! Rust port of `Gateway/app.py::create_app`. Wave 1 mounted `/health`;
//! wave 2a adds the chat-adjacent surface (`chat.run_turn`,
//! conversations CRUD, prompts CRUD). The rag / voice / models /
//! memory / workspaces / training / push / link routes plus the Ollama
//! SSE proxy are queued for wave 2a.1 / 2b+.
//!
//! Middleware order (outer → inner):
//! `CORS → Trace → AuditLog → RateLimit → routes`. Tower composes layers
//! in reverse application order, so the layer applied LAST in
//! [`build_router`] is the OUTERMOST on the wire: `.layer(rate_limit)` is
//! innermost, `.layer(audit)` wraps it, `.layer(trace)` wraps that, and
//! `.layer(cors)` wraps all of it. The audit middleware can read the
//! request id from the request extensions because trace runs first on
//! the inbound path; rate-limit sits just outside the routes so an
//! over-limit request is still trace-stamped and audit-logged.
//!
//! The events middleware (`Gateway/events.py`) is NOT a global layer
//! here — it is a per-route `from_fn` layer mounted inner to
//! `require_device` in the `chat` / `devices` route modules.
//!
//! What's deliberately NOT wired here:
//!   * Lifespan setup — async_loop, egress destinations reload, secrets
//!     warm-up. The Rust [`crate::run`] does the wave-1 subset directly.

use axum::http::{HeaderName, HeaderValue, Method};
use axum::middleware::from_fn_with_state;
use axum::Router;
use tower_http::cors::CorsLayer;

use crate::middleware::rate_limit::{rate_limit, RateLimiter};
use crate::middleware::{AuditLogLayer, RequestTraceLayer, REQUEST_ID_HEADER};
use crate::routes;
use crate::settings::GatewaySettings;

/// Build the axum router with the wave-1 middleware stack mounted on top
/// of the wave-1 route surface.
pub fn build_router(settings: GatewaySettings) -> Router {
    let cors = build_cors_layer(&settings);
    let audit = AuditLogLayer::new(settings.audit_log_enabled, settings.audit_log_dir.clone());
    let limiter = RateLimiter::new(settings.rate_limit_per_minute);

    let base = Router::new();
    let with_routes = routes::include_all(base);

    with_routes
        .layer(from_fn_with_state(limiter, rate_limit))
        .layer(audit)
        .layer(RequestTraceLayer::new())
        .layer(cors)
}

fn build_cors_layer(settings: &GatewaySettings) -> CorsLayer {
    let origins: Vec<HeaderValue> = settings
        .cors_origins()
        .into_iter()
        .filter_map(|o| HeaderValue::from_str(&o).ok())
        .collect();

    // tower-http enforces the CORS spec: `Allow-Credentials: true`
    // forbids `*` for headers or methods. Python (Starlette) doesn't
    // enforce this so its config wildcards everything, but the browser
    // would reject the response anyway. We list the methods + headers
    // explicitly; the surface stays equivalent for any real caller.
    let allowed_headers: Vec<HeaderName> = [
        "accept",
        "authorization",
        "content-type",
        "x-requested-with",
        REQUEST_ID_HEADER,
    ]
    .iter()
    .filter_map(|h| HeaderName::from_bytes(h.to_ascii_lowercase().as_bytes()).ok())
    .collect();

    let mut layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_credentials(true)
        .allow_headers(allowed_headers)
        .expose_headers([HeaderName::from_static("x-wylde-request-id")]);
    if !origins.is_empty() {
        layer = layer.allow_origin(origins);
    }
    layer
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    fn make_settings() -> GatewaySettings {
        // Construct settings directly so this test never touches the
        // process-wide settings cache.
        GatewaySettings {
            host: "127.0.0.1".into(),
            port: 8005,
            workers: 1,
            local_cidrs_csv: "127.0.0.1/32".into(),
            trust_forwarded_for: false,
            rate_limit_per_minute: 1000,
            audit_log_dir: std::env::temp_dir(),
            audit_log_enabled: false,
            cors_origins_csv: "http://localhost".into(),
            secrets_provider: "file".into(),
            secrets_strict_mode: false,
            egress_kill_switch_init: false,
        }
    }

    #[tokio::test]
    async fn router_serves_health() {
        let app = build_router(make_settings());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["status"], "healthy");
    }

    #[tokio::test]
    async fn router_stamps_request_id_header() {
        let app = build_router(make_settings());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let header = response.headers().get("x-wylde-request-id");
        assert!(
            header.is_some(),
            "trace middleware should stamp the response"
        );
    }
}
