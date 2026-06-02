//! Egress — Gateway-pipe-first with a loud direct-`reqwest` fallback.
//!
//! Port of the Python handler's `_fetch_via_gateway_or_fallback`. The
//! canonical path forwards through the Rust Gateway's `egress.forward` action
//! over the pipe (`wylde-gateway`), so the Gateway's allowlist, kill switch,
//! and audit log apply. If the Gateway pipe is unreachable (e.g. not running
//! in dev) we fall back to a direct `reqwest` GET — logging at WARNING each
//! time so the bypass can't go unnoticed in production.
//!
//! Policy rejections (`egress_blocked`, `egress_denied`) and upstream failures
//! reported by a *reachable* Gateway must **not** fall back — they surface as
//! errors, exactly as the Python re-raises `GatewayBlocked`/`GatewayDenied`.

use std::time::Duration;

use serde_json::{json, Value};
use wylde_shared::ipc;

use crate::config::Config;

/// Normalised result of a single GET. Mirrors the Python helper's
/// `{ok, status, content, headers}` dict.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub ok: bool,
    pub status: u16,
    pub content: String,
    pub headers: Value,
}

/// Why a Gateway forward did not yield a usable response.
enum GatewayCallError {
    /// Policy denial from a reachable Gateway — surface, do NOT fall back.
    Policy(String),
    /// Upstream/target failure from a reachable Gateway — surface, do NOT
    /// fall back (a direct request would hit the same dead target).
    Upstream(String),
    /// The Gateway itself was unreachable (pipe down / transport error) —
    /// fall back to a direct request.
    Transport(String),
}

/// Fetch `url` via the Gateway, falling back to a direct request only when the
/// Gateway is unreachable. Returns the body + status on success, or a short
/// error string on failure (the caller wraps it as the tool's error result).
pub async fn fetch_via_gateway_or_fallback(
    url: &str,
    timeout_secs: f64,
) -> Result<FetchOutcome, String> {
    match gateway_forward(url, timeout_secs).await {
        Ok(outcome) => Ok(outcome),
        Err(GatewayCallError::Policy(msg)) => Err(msg),
        Err(GatewayCallError::Upstream(msg)) => Err(msg),
        Err(GatewayCallError::Transport(msg)) => {
            tracing::warn!(
                "webcrawler: Gateway egress failed ({msg}); falling back to \
                 direct reqwest. TODO: remove fallback once the Rust Gateway \
                 egress is always reachable in this deployment."
            );
            direct_get(url, timeout_secs).await
        }
    }
}

/// Single GET through the Gateway's `egress.forward` action.
async fn gateway_forward(url: &str, timeout_secs: f64) -> Result<FetchOutcome, GatewayCallError> {
    let cfg = Config::get();
    // The `web` destination is a wildcard (`url_prefix: "https://"`), so the
    // Gateway expects the **full URL** as `path` and enforces only the scheme.
    let payload = json!({
        "caller": cfg.egress_caller,
        "dest": cfg.egress_dest,
        "method": "GET",
        "path": url,
        "headers": { "User-Agent": cfg.user_agent },
        "timeout": timeout_secs,
    });

    match ipc::call_action(&cfg.gateway_service, "egress.forward", payload).await {
        Ok(data) => {
            let status = data.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
            let content = body_to_string(data.get("body").unwrap_or(&Value::Null));
            let headers = data
                .get("headers")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(FetchOutcome {
                ok: (200..300).contains(&status),
                status,
                content,
                headers,
            })
        }
        Err(e) => Err(classify_ipc_error(&e.code, &e.message)),
    }
}

/// Map an `ipc` error code to the fall-back decision. Policy / upstream codes
/// come from a *reachable* Gateway and must surface; everything else means the
/// pipe/transport failed and we fall back.
fn classify_ipc_error(code: &str, message: &str) -> GatewayCallError {
    match code {
        "egress_blocked" | "egress_denied" => {
            GatewayCallError::Policy(format!("{code}: {message}"))
        }
        "egress_upstream_error" => GatewayCallError::Upstream(message.to_owned()),
        // pipe_connect / pipe_unavailable / pipe_timeout / handshake_* /
        // decode / encode / ipc_disabled / no_http_backend / unknown …
        other => GatewayCallError::Transport(format!("{other}: {message}")),
    }
}

/// Decode the Gateway reply `body` (a JSON value) to a content string, exactly
/// as the Python helper does: strings pass through, objects/arrays are
/// re-serialised, null becomes empty.
fn body_to_string(body: &Value) -> String {
    match body {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Direct `reqwest` GET — the dev fallback used only when the Gateway pipe is
/// unreachable. The caller has already run the SSRF guard, so this cannot be
/// pointed at a private address.
async fn direct_get(url: &str, timeout_secs: f64) -> Result<FetchOutcome, String> {
    let cfg = Config::get();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_secs.max(0.001)))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;

    let resp = client
        .get(url)
        .header("User-Agent", &cfg.user_agent)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    let headers: serde_json::Map<String, Value> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_owned(), Value::String(s.to_owned()))))
        .collect();
    let content = resp.text().await.map_err(|e| e.to_string())?;

    Ok(FetchOutcome {
        ok: (200..300).contains(&status),
        status,
        content,
        headers: Value::Object(headers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_to_string_passthrough_for_string() {
        assert_eq!(body_to_string(&json!("<html>")), "<html>");
    }

    #[test]
    fn body_to_string_empty_for_null() {
        assert_eq!(body_to_string(&Value::Null), "");
    }

    #[test]
    fn body_to_string_serialises_object() {
        assert_eq!(body_to_string(&json!({"a": 1})), "{\"a\":1}");
    }

    #[test]
    fn classify_policy_codes_do_not_fall_back() {
        assert!(matches!(
            classify_ipc_error("egress_blocked", "x"),
            GatewayCallError::Policy(_)
        ));
        assert!(matches!(
            classify_ipc_error("egress_denied", "x"),
            GatewayCallError::Policy(_)
        ));
    }

    #[test]
    fn classify_upstream_is_surfaced() {
        assert!(matches!(
            classify_ipc_error("egress_upstream_error", "x"),
            GatewayCallError::Upstream(_)
        ));
    }

    #[test]
    fn classify_transport_codes_fall_back() {
        for code in ["pipe_connect", "pipe_unavailable", "pipe_timeout", "decode", "unknown"] {
            assert!(
                matches!(classify_ipc_error(code, "x"), GatewayCallError::Transport(_)),
                "{code} should fall back"
            );
        }
    }
}
