//! Egress — Gateway-only. **No direct-`reqwest` bypass.**
//!
//! Every outbound fetch forwards through the Rust Gateway's `egress.forward`
//! action over the pipe (`wylde-gateway`), so the Gateway's allowlist, kill
//! switch, and audit log are the single chokepoint. There is deliberately **no
//! fallback** to a direct request: a fallback would let the extension reach the
//! network whenever the Gateway was denied or simply unreachable, exactly the
//! hole the security boundary (WyldeStudy P3) closed elsewhere. If the Gateway
//! says no — or isn't there — the fetch hard-fails.
//!
//! Failure taxonomy (all terminal, none bypassable):
//!   * `egress_blocked` / `egress_denied` — policy rejection from a reachable
//!     Gateway.
//!   * `egress_upstream_error` — the Gateway reached the target and it failed.
//!   * anything else (`pipe_unavailable`, `pipe_timeout`, …) — the Gateway
//!     itself was unreachable; the fetch still fails rather than going direct.

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

/// Why a Gateway forward did not yield a usable response. Every variant is
/// terminal — there is no direct-request bypass.
enum GatewayCallError {
    /// Policy denial from a reachable Gateway.
    Policy(String),
    /// Upstream/target failure from a reachable Gateway.
    Upstream(String),
    /// The Gateway itself was unreachable (pipe down / transport error).
    Transport(String),
}

impl GatewayCallError {
    /// Collapse to the terminal error string the tool surfaces. All variants
    /// are terminal: the Gateway is the only egress path, so a denial or an
    /// unreachable Gateway both mean "no fetch".
    fn into_error(self) -> String {
        match self {
            GatewayCallError::Policy(msg)
            | GatewayCallError::Upstream(msg)
            | GatewayCallError::Transport(msg) => msg,
        }
    }
}

/// Fetch `url` through the Gateway. Returns the body + status on success, or a
/// terminal error string on any failure (policy denial, upstream error, or an
/// unreachable Gateway). There is **no** direct-request fallback: the Gateway
/// is the sole egress chokepoint.
pub async fn fetch_via_gateway(url: &str, timeout_secs: f64) -> Result<FetchOutcome, String> {
    gateway_forward(url, timeout_secs)
        .await
        .map_err(GatewayCallError::into_error)
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
            let headers = data.get("headers").cloned().unwrap_or_else(|| json!({}));
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

/// Classify an `ipc` error code. The variant only shapes the error *message*
/// now — all of them are terminal (no fall-back path exists). Policy / upstream
/// codes come from a reachable Gateway; everything else means the pipe/transport
/// itself failed and the fetch fails with it.
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
    fn classify_policy_codes_are_policy() {
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
    fn classify_transport_codes_are_transport() {
        for code in [
            "pipe_connect",
            "pipe_unavailable",
            "pipe_timeout",
            "decode",
            "unknown",
        ] {
            assert!(
                matches!(
                    classify_ipc_error(code, "x"),
                    GatewayCallError::Transport(_)
                ),
                "{code} should classify as transport"
            );
        }
    }

    // ── no direct bypass: EVERY failure is terminal ─────────────────────────

    #[test]
    fn denied_egress_is_terminal() {
        // A reachable Gateway that denies the request must surface as a
        // terminal error — there is no direct-reqwest path to fall through to.
        let err = classify_ipc_error("egress_denied", "blocked by allowlist").into_error();
        assert!(err.contains("egress_denied"), "got: {err}");
    }

    #[test]
    fn unreachable_gateway_is_terminal_not_bypassed() {
        // The old hole: a pipe-down Gateway used to fall back to a direct GET,
        // bypassing the allowlist entirely. Now it is just as terminal as a
        // denial — the only egress path is the Gateway.
        for code in ["pipe_unavailable", "pipe_timeout", "pipe_connect"] {
            let err = classify_ipc_error(code, "gateway down").into_error();
            assert!(err.contains(code), "{code} must be terminal, got: {err}");
        }
    }
}
