//! SSE (Server-Sent Events) helpers — Rust port of `Gateway/streaming.py`.
//!
//! Two upstream stream shapes are re-emitted as SSE so the mobile /
//! desktop / GUI client only has one parser to maintain:
//!
//! * **NDJSON** (Ollama chat + model pull) — one JSON object per line.
//! * **SSE passthrough** (orchestrator workflow streams) — already SSE.
//!
//! Wave 2c lands the NDJSON variant only — the sole live consumer is
//! `POST /api/models/pull`. The passthrough variant joins when an
//! actual SSE-emitting upstream comes online.
//!
//! Event vocabulary (matches Python byte-for-byte):
//!
//! * `event: token`    — partial inference chunk
//! * `event: progress` — long-running status update (download, training)
//! * `event: error`    — upstream failed mid-stream
//! * `event: done`     — clean close
//!
//! Frame format:
//!
//! ```text
//! event: <name>\n
//! data: <compact-JSON>\n
//! \n
//! ```
//!
//! Heartbeat is the SSE-spec comment `: keepalive\n\n`, emitted on each
//! blank upstream line so proxies don't idle-kill the connection.

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::stream::StreamExt;
use serde_json::{json, Map, Value};

use crate::proxy_core::{streaming_client, HttpMethod};

/// Default per-chunk read timeout. Mirrors Python's
/// `DEFAULT_CHUNK_TIMEOUT = 30.0`.
pub const DEFAULT_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Apply the standard SSE header set to a response. Same headers Python
/// writes via `StreamingResponse(..., media_type="text/event-stream",
/// headers=...)`.
fn apply_sse_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
}

/// Encode one SSE frame: `event: <name>\ndata: <json>\n\n`.
pub fn encode(event_name: &str, payload: &Value) -> Bytes {
    let mut s = String::with_capacity(64);
    s.push_str("event: ");
    s.push_str(event_name);
    s.push('\n');
    s.push_str("data: ");
    // serde_json::to_string is the equivalent of Python's
    // `json.dumps(..., separators=(',', ':'))`: no spaces between
    // tokens, so the wire bytes match.
    s.push_str(&serde_json::to_string(payload).unwrap_or_else(|_| "null".to_owned()));
    s.push_str("\n\n");
    Bytes::from(s)
}

/// The SSE-spec comment frame the client treats as a keep-alive.
pub fn heartbeat() -> Bytes {
    Bytes::from_static(b": keepalive\n\n")
}

/// Stream an NDJSON-emitting upstream as SSE.
///
/// Equivalent of Python's `ndjson_to_sse`. Opens a streaming POST to
/// `url` with `payload` as the JSON body, reads NDJSON lines, and
/// re-emits each as an `event: <event_name>` SSE frame. The final line
/// (any line whose JSON object has the `done_field` set truthy) is
/// re-emitted as `event: <done_event>`. Errors mid-stream become
/// `event: error` frames; the connection then closes cleanly.
pub async fn ndjson_to_sse(
    url: &str,
    payload: Value,
    method: HttpMethod,
    event_name: &'static str,
    done_event: &'static str,
    done_field: &'static str,
    chunk_timeout: Duration,
) -> Response {
    let client = streaming_client();
    let mut req = client.request(method.into_reqwest(), url);
    if matches!(method, HttpMethod::Post | HttpMethod::Put) {
        req = req.json(&payload);
    }
    // Apply a connect timeout, but let read-side time out per chunk
    // inside the loop — long pulls keep the stream open for minutes.
    req = req.timeout(Duration::from_secs(60 * 60));

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return single_error_sse(
                "timeout",
                &format!(
                    "{url} did not respond within {}s",
                    chunk_timeout.as_secs_f64()
                ),
            );
        }
        Err(e) => {
            return single_error_sse("transport", &format!("{e}"));
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.bytes().await.unwrap_or_default();
        let msg = String::from_utf8_lossy(&body);
        let mut truncated: String = msg.chars().take(300).collect();
        if truncated.is_empty() {
            truncated = status
                .canonical_reason()
                .unwrap_or("upstream error")
                .to_owned();
        }
        return single_error_sse(&format!("http_{}", status.as_u16()), &truncated);
    }

    // Convert the chunked byte stream into a line stream. We accumulate
    // bytes in `buf` and yield SSE frames every time a `\n` lands.
    let byte_stream = resp.bytes_stream();
    let event_name_owned = event_name.to_owned();
    let done_event_owned = done_event.to_owned();
    let done_field_owned = done_field.to_owned();
    let stream = async_stream::stream! {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut stream = byte_stream;
        loop {
            // Wrap each `next()` in a timeout so a hung upstream
            // doesn't pin a connection open forever.
            let chunk = tokio::time::timeout(chunk_timeout, stream.next()).await;
            let next = match chunk {
                Ok(Some(Ok(b))) => b,
                Ok(Some(Err(e))) => {
                    yield Ok::<Bytes, std::io::Error>(encode(
                        "error",
                        &json!({
                            "ok": false,
                            "error": "transport",
                            "message": format!("{e}"),
                        }),
                    ));
                    return;
                }
                Ok(None) => {
                    // Upstream closed without a `done`-flagged line —
                    // synthesize one so the client's parser unblocks.
                    yield Ok(encode(
                        done_event_owned.as_str(),
                        &json!({ "ok": true }),
                    ));
                    return;
                }
                Err(_) => {
                    yield Ok(encode(
                        "error",
                        &json!({
                            "ok": false,
                            "error": "timeout",
                            "message": format!(
                                "upstream did not emit a chunk within {}s",
                                chunk_timeout.as_secs_f64()
                            ),
                        }),
                    ));
                    return;
                }
            };

            buf.extend_from_slice(&next);
            // Drain whole lines.
            while let Some(idx) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=idx).collect();
                let line_str = match std::str::from_utf8(&line[..line.len().saturating_sub(1)]) {
                    Ok(s) => s.trim_end_matches('\r').trim(),
                    Err(_) => continue,
                };
                if line_str.is_empty() {
                    yield Ok(heartbeat());
                    continue;
                }
                let parsed: Value = match serde_json::from_str(line_str) {
                    Ok(v) => v,
                    Err(_) => {
                        yield Ok(encode(
                            "error",
                            &json!({
                                "ok": false,
                                "error": "parse",
                                "message": "upstream emitted invalid JSON",
                            }),
                        ));
                        continue;
                    }
                };

                let is_done = match &parsed {
                    Value::Object(m) => match m.get(done_field_owned.as_str()) {
                        Some(Value::Bool(b)) => *b,
                        Some(Value::String(s)) => !s.is_empty(),
                        Some(v) => !v.is_null(),
                        None => false,
                    },
                    _ => false,
                };

                let envelope = build_event_payload(&parsed);
                if is_done {
                    yield Ok(encode(done_event_owned.as_str(), &envelope));
                    return;
                }
                yield Ok(encode(event_name_owned.as_str(), &envelope));
            }
        }
    };

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(stream))
        .expect("static SSE response shape");
    apply_sse_headers(&mut response);
    response
}

/// Merge `{ok: true, ...}` into a JSON object, or wrap a scalar as
/// `{ok: true, value: <scalar>}`. Mirrors the Python wrapper.
fn build_event_payload(parsed: &Value) -> Value {
    match parsed {
        Value::Object(map) => {
            let mut out: Map<String, Value> = Map::with_capacity(map.len() + 1);
            out.insert("ok".to_owned(), Value::Bool(true));
            for (k, v) in map {
                out.insert(k.clone(), v.clone());
            }
            Value::Object(out)
        }
        other => json!({ "ok": true, "value": other.clone() }),
    }
}

/// Emit a single `event: error` frame and close. Used when the upstream
/// connect itself fails before any NDJSON line arrives.
fn single_error_sse(code: &str, message: &str) -> Response {
    let frame = encode(
        "error",
        &json!({
            "ok": false,
            "error": code,
            "message": message,
        }),
    );
    let body = Body::from(frame);
    let mut response = (StatusCode::OK, body).into_response();
    apply_sse_headers(&mut response);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_emits_event_then_data_then_blank_line() {
        let frame = encode("token", &json!({"ok": true, "text": "hi"}));
        let s = std::str::from_utf8(&frame).unwrap();
        assert!(s.starts_with("event: token\n"), "frame was {s:?}");
        assert!(s.contains("data: {"), "frame was {s:?}");
        assert!(s.ends_with("\n\n"), "frame was {s:?}");
    }

    #[test]
    fn encode_compact_json_matches_python_separators() {
        let frame = encode("progress", &json!({"a": 1, "b": "x"}));
        let s = std::str::from_utf8(&frame).unwrap();
        // No spaces between tokens — Python uses separators=(',', ':').
        assert!(s.contains(r#"{"a":1,"b":"x"}"#), "frame was {s:?}");
    }

    #[test]
    fn heartbeat_is_sse_comment() {
        let h = heartbeat();
        assert_eq!(h, Bytes::from_static(b": keepalive\n\n"));
    }

    #[test]
    fn build_event_payload_merges_ok_flag() {
        let v = build_event_payload(&json!({"status": "downloading", "completed": 12}));
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "downloading");
        assert_eq!(v["completed"], 12);
    }

    #[test]
    fn build_event_payload_wraps_scalars() {
        let v = build_event_payload(&json!("plain string"));
        assert_eq!(v["ok"], true);
        assert_eq!(v["value"], "plain string");
    }

    #[tokio::test]
    async fn ndjson_to_sse_handles_unreachable_upstream() {
        // Same trick as the proxy_core test — bind+drop yields a port
        // that's certain to refuse the connect. Generous chunk-timeout
        // so a Windows SYN retry doesn't trip the timeout branch.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/api/pull");
        let resp = ndjson_to_sse(
            &url,
            json!({"name": "llama3"}),
            HttpMethod::Post,
            "progress",
            "done",
            "status",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("text/event-stream"),
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024)
            .await
            .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: error\n"), "got: {s:?}");
        assert!(s.contains("\"error\":\"transport\""), "got: {s:?}");
    }
}
