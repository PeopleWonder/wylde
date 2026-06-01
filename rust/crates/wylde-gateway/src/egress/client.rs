//! Async HTTP egress client — performs the actual upstream request.
//!
//! Rust port of `Gateway/egress/client.py`. Composes [`destinations`]
//! (allowlist + path validation), [`kill_switch`] (global block), and
//! `reqwest` (async transport) into the public functions [`forward`]
//! (unary) and [`forward_stream`] (chunked).
//!
//! Call sequence — kept in one function so the audit boundaries are
//! obvious:
//!   1. Check kill switch.
//!   2. Validate method.
//!   3. Resolve destination + validate path.
//!   4. Compose headers (strip caller auth, inject env auth if declared).
//!   5. Build + send the request.
//!   6. Emit audit log line.
//!   7. Decode body, return [`EgressResult`].
//!
//! Audit failures never bubble up — losing a log line must not break a
//! request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::stream::BoxStream;
use serde_json::Value;

use super::destinations::{
    build_target_url, resolve, validate_path, Destination, EgressDestinationError,
};
use super::kill_switch::is_blocked;
use crate::middleware::audit_log::emit_egress;
use crate::secrets::get_secrets;

const FORBIDDEN_HEADERS: &[&str] = &["authorization", "cookie", "x-wylde-egress-caller"];
const SAFE_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"];

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    /// Kill switch is engaged.
    #[error("egress kill switch is engaged")]
    Blocked,
    /// Destination or path is not allowed.
    #[error("{0}")]
    Denied(String),
    /// Method or other policy rejection.
    #[error("{0}")]
    Policy(String),
    /// Transport / upstream failure.
    #[error("{0}")]
    Upstream(String),
}

#[derive(Debug, Clone)]
pub struct EgressResult {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Value,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub duration_ms: f64,
}

// ── Public API ────────────────────────────────────────────────────────

/// Single round-trip outbound call. Returns an [`EgressResult`] for any
/// upstream status (including >= 400). Returns [`EgressError`] on
/// policy / transport failure.
pub async fn forward(
    caller: &str,
    dest_key: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: Option<&HashMap<String, String>>,
    timeout: Duration,
) -> Result<EgressResult, EgressError> {
    if is_blocked() {
        emit_egress(serde_json::json!({
            "caller": caller,
            "dest": dest_key,
            "method": method,
            "path": path,
            "blocked": true,
            "reason": "kill_switch",
        }));
        return Err(EgressError::Blocked);
    }

    let method_norm = method.to_ascii_uppercase();
    if !SAFE_METHODS.contains(&method_norm.as_str()) {
        return Err(EgressError::Policy(format!(
            "method {method:?} not permitted"
        )));
    }

    let dest = resolve(caller, dest_key).map_err(into_denied)?;
    let safe_path = validate_path(&dest, path).map_err(into_denied)?;
    let target = build_target_url(&dest, &safe_path);
    let out_headers = compose_headers(&dest, headers);
    let payload_bytes = coerce_body_size(body);

    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(!dest.verify_tls)
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(EgressError::Upstream(format!("client build failed: {e}")));
        }
    };

    let method_for_req = match method_norm.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        _ => unreachable!("guarded by SAFE_METHODS above"),
    };

    let mut req = client.request(method_for_req, &target);
    for (k, v) in &out_headers {
        req = req.header(k, v);
    }
    if method_norm == "GET" {
        if let Some(Value::Object(map)) = body {
            let mut params: Vec<(String, String)> = Vec::with_capacity(map.len());
            for (k, v) in map {
                params.push((k.clone(), value_to_string(v)));
            }
            req = req.query(&params);
        }
    } else if let Some(b) = body {
        req = req.json(b);
    }

    let t0 = Instant::now();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let dur = elapsed_ms(t0);
            emit_egress(serde_json::json!({
                "caller": caller,
                "dest": dest.key,
                "method": method_norm,
                "path": safe_path,
                "ok": false,
                "error": format!("{e}"),
                "dur_ms": round3(dur),
                "bytes_in": payload_bytes,
            }));
            return Err(EgressError::Upstream(format!(
                "upstream {} unreachable: {e}",
                dest.key
            )));
        }
    };

    let status = resp.status().as_u16();
    let resp_headers: HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_owned(), s.to_owned()))
        })
        .collect();
    let content_type = resp_headers
        .get("content-type")
        .cloned()
        .or_else(|| resp_headers.get("Content-Type").cloned())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            emit_egress(serde_json::json!({
                "caller": caller,
                "dest": dest.key,
                "method": method_norm,
                "path": safe_path,
                "ok": false,
                "error": format!("read body: {e}"),
                "dur_ms": round3(elapsed_ms(t0)),
                "bytes_in": payload_bytes,
            }));
            return Err(EgressError::Upstream(format!(
                "upstream {} read failed: {e}",
                dest.key
            )));
        }
    };
    let bytes_out = raw.len();
    let body_value = decode_body(&raw, &content_type);

    let dur = elapsed_ms(t0);
    emit_egress(serde_json::json!({
        "caller": caller,
        "dest": dest.key,
        "method": method_norm,
        "path": safe_path,
        "status": status,
        "bytes_in": payload_bytes,
        "bytes_out": bytes_out,
        "dur_ms": round3(dur),
    }));

    Ok(EgressResult {
        status,
        headers: resp_headers,
        body: body_value,
        bytes_in: payload_bytes,
        bytes_out,
        duration_ms: dur,
    })
}

/// Streaming variant. Returns `(status, headers, byte stream)`.
pub async fn forward_stream(
    caller: &str,
    dest_key: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: Option<&HashMap<String, String>>,
    connect_timeout: Duration,
) -> Result<
    (
        u16,
        HashMap<String, String>,
        BoxStream<'static, Result<Bytes, std::io::Error>>,
    ),
    EgressError,
> {
    if is_blocked() {
        emit_egress(serde_json::json!({
            "caller": caller,
            "dest": dest_key,
            "method": method,
            "path": path,
            "stream": true,
            "blocked": true,
            "reason": "kill_switch",
        }));
        return Err(EgressError::Blocked);
    }

    let method_norm = method.to_ascii_uppercase();
    if !SAFE_METHODS.contains(&method_norm.as_str()) {
        return Err(EgressError::Policy(format!(
            "method {method:?} not permitted"
        )));
    }

    let dest = resolve(caller, dest_key).map_err(into_denied)?;
    let safe_path = validate_path(&dest, path).map_err(into_denied)?;
    let target = build_target_url(&dest, &safe_path);
    let out_headers = compose_headers(&dest, headers);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(!dest.verify_tls)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|e| EgressError::Upstream(format!("client build failed: {e}")))?;

    let method_for_req = match method_norm.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        _ => unreachable!("guarded by SAFE_METHODS above"),
    };

    let mut req = client.request(method_for_req, &target);
    for (k, v) in &out_headers {
        req = req.header(k, v);
    }
    if let Some(b) = body {
        if method_norm != "GET" {
            req = req.json(b);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            emit_egress(serde_json::json!({
                "caller": caller,
                "dest": dest.key,
                "method": method_norm,
                "path": safe_path,
                "stream": true,
                "ok": false,
                "error": format!("{e}"),
            }));
            return Err(EgressError::Upstream(format!(
                "upstream {} unreachable: {e}",
                dest.key
            )));
        }
    };

    let status = resp.status().as_u16();
    let resp_headers: HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_owned(), s.to_owned()))
        })
        .collect();

    use futures::StreamExt;
    let dest_key_owned = dest.key.clone();
    let method_for_log = method_norm.clone();
    let path_for_log = safe_path.clone();
    let caller_owned = caller.to_owned();
    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    let logged = futures::stream::unfold(
        (
            Box::pin(stream)
                as std::pin::Pin<
                    Box<dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
                >,
            Some((
                caller_owned,
                dest_key_owned,
                method_for_log,
                path_for_log,
                status,
            )),
        ),
        |(mut s, mut sentinel)| async move {
            match s.next().await {
                Some(item) => Some((item, (s, sentinel))),
                None => {
                    if let Some((caller, dest, method, path, st)) = sentinel.take() {
                        emit_egress(serde_json::json!({
                            "caller": caller,
                            "dest": dest,
                            "method": method,
                            "path": path,
                            "stream": true,
                            "status": st,
                        }));
                    }
                    None
                }
            }
        },
    );
    Ok((status, resp_headers, Box::pin(logged)))
}

// ── Header composition ────────────────────────────────────────────────

fn compose_headers(
    dest: &Destination,
    caller_headers: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(h) = caller_headers {
        for (k, v) in h {
            if FORBIDDEN_HEADERS.iter().any(|f| k.eq_ignore_ascii_case(f)) {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
    }
    if !dest.auth_header_env.is_empty() {
        if let Some(token) = resolve_secret(dest) {
            out.insert("Authorization".into(), format!("Bearer {token}"));
        }
    }
    out
}

fn resolve_secret(dest: &Destination) -> Option<String> {
    if dest.auth_header_env.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(&dest.auth_header_env) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    get_secrets().get(&dest.auth_header_env, None)
}

// ── Helpers ───────────────────────────────────────────────────────────

fn into_denied(e: EgressDestinationError) -> EgressError {
    EgressError::Denied(e.to_string())
}

fn coerce_body_size(body: Option<&Value>) -> usize {
    match body {
        None | Some(Value::Null) => 0,
        Some(v) => serde_json::to_string(v).map(|s| s.len()).unwrap_or(0),
    }
}

fn decode_body(raw: &[u8], content_type: &str) -> Value {
    if raw.is_empty() {
        return Value::Null;
    }
    if content_type.contains("application/json") {
        if let Ok(v) = serde_json::from_slice::<Value>(raw) {
            return v;
        }
    }
    if content_type.starts_with("text/") || content_type.contains("x-ndjson") {
        return Value::String(String::from_utf8_lossy(raw).into_owned());
    }
    // Binary — let the caller decide what to do; surface a marker so the
    // service layer can re-encode if needed.
    Value::String(String::from_utf8_lossy(raw).into_owned())
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn elapsed_ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::destinations;
    use crate::egress::kill_switch::{self, EGRESS_TEST_LOCK};

    #[test]
    fn compose_headers_strips_forbidden() {
        let mut h = HashMap::new();
        h.insert("authorization".into(), "Bearer secret".into());
        h.insert("Cookie".into(), "session=x".into());
        h.insert("Accept".into(), "application/json".into());
        let d = Destination {
            key: "k".into(),
            component: "c".into(),
            url_prefix: "https://".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
        };
        let out = compose_headers(&d, Some(&h));
        assert!(out.contains_key("Accept"));
        assert!(!out.keys().any(|k| k.eq_ignore_ascii_case("authorization")));
        assert!(!out.keys().any(|k| k.eq_ignore_ascii_case("cookie")));
    }

    #[test]
    fn coerce_body_size_zero_for_none() {
        assert_eq!(coerce_body_size(None), 0);
        assert_eq!(coerce_body_size(Some(&Value::Null)), 0);
    }

    #[test]
    fn coerce_body_size_counts_json_bytes() {
        let v = serde_json::json!({"a": 1});
        assert_eq!(
            coerce_body_size(Some(&v)),
            serde_json::to_string(&v).unwrap().len()
        );
    }

    #[test]
    fn decode_body_parses_json() {
        let v = decode_body(br#"{"a":1}"#, "application/json");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn decode_body_returns_text_for_text_content_type() {
        let v = decode_body(b"plain", "text/plain");
        assert_eq!(v, Value::String("plain".into()));
    }

    #[test]
    fn decode_body_empty_is_null() {
        assert_eq!(decode_body(b"", "application/json"), Value::Null);
    }

    #[tokio::test]
    async fn forward_blocked_when_kill_switch_engaged() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        kill_switch::set_blocked(true);
        destinations::reset_for_test();
        let err = forward(
            "Caller",
            "k",
            "GET",
            "/",
            None,
            None,
            Duration::from_secs(1),
        )
        .await
        .expect_err("must block");
        assert!(matches!(err, EgressError::Blocked));
        kill_switch::set_blocked(false);
    }

    #[tokio::test]
    async fn forward_denied_for_unscoped_caller() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        kill_switch::set_blocked(false);
        destinations::reset_for_test();
        let err = forward(
            "GhostCaller",
            "k",
            "GET",
            "/",
            None,
            None,
            Duration::from_secs(1),
        )
        .await
        .expect_err("must deny");
        assert!(matches!(err, EgressError::Denied(_)));
    }

    #[tokio::test]
    async fn forward_rejects_unsupported_method() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        kill_switch::set_blocked(false);
        destinations::reset_for_test();
        let err = forward(
            "Caller",
            "k",
            "TRACE",
            "/",
            None,
            None,
            Duration::from_secs(1),
        )
        .await
        .expect_err("must reject method");
        assert!(matches!(err, EgressError::Policy(_)));
    }
}
