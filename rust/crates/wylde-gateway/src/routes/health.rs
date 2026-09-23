//! `/health` — public, no auth.
//!
//! Rust port of `Gateway/routes/health.py`. Matches the Python wire
//! format exactly: `{"ok": true, "data": {"status": "healthy", "ts":
//! "<iso8601-Z>"}}` with HTTP 200.
//!
//! Python deleted the `/live` and `/ready` Kubernetes-style probes a
//! while ago — the daemon already heartbeats every service through the
//! named-pipe manifests, which is the canonical liveness signal. So
//! wave 1 ports just `/health`.

use axum::middleware::from_fn;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use chrono::Utc;
use serde_json::json;

use crate::auth::require_public;
use crate::envelopes::success;

const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

fn utc_now_iso() -> String {
    Utc::now().format(TIME_FORMAT).to_string()
}

pub async fn health() -> Response {
    success(json!({
        "status": "healthy",
        "ts": utc_now_iso(),
    }))
}

pub fn router() -> Router {
    Router::new().route("/health", get(health).route_layer(from_fn(require_public)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_matches_python_shape() {
        let app = router();
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
        let body_bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let parsed: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["status"], "healthy");
        assert!(parsed["data"]["ts"].is_string());
        let ts = parsed["data"]["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z'), "ts should end with Z: {ts}");
        assert_eq!(ts.len(), 20, "ts should be 20 chars (YYYY-MM-DDTHH:MM:SSZ)");
    }
}
