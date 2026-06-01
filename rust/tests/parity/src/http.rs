//! HTTP capture for Gateway parity.
//!
//! Both gateways are spun up on different ports (`WYLDE_GATEWAY_PORT`); the
//! same request is fired at each with [`fire`] and the captured response is
//! rendered as a JSON value for [`crate::diff::assert_parity`].
//!
//! A capture is `{ status, content_type, body }`. The body is parsed as
//! JSON when possible, parsed into an SSE event list for `text/event-stream`
//! responses, and otherwise kept as a raw string.

use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde_json::{json, Value};

/// One request to fire at both gateways.
pub struct HttpCase {
    /// Stable case name used in diff failure messages.
    pub name: &'static str,
    /// HTTP method: `"GET"`, `"POST"`, `"PUT"`, `"DELETE"`.
    pub method: &'static str,
    /// Request path including any query string, e.g. `"/health"`.
    pub path: &'static str,
    /// JSON request body, or `None`.
    pub body: Option<Value>,
    /// Extra request headers (e.g. an `Authorization` bearer).
    pub headers: &'static [(&'static str, &'static str)],
}

/// A captured HTTP response, reduced to the parts that should be identical
/// across implementations.
pub struct CapturedHttp {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Value,
}

impl CapturedHttp {
    /// Render as a single JSON value for diffing.
    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "content_type": self.content_type,
            "body": self.body,
        })
    }
}

/// Build a blocking client suitable for loopback parity calls.
pub fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest blocking client")
}

/// Fire `case` at `base_url` (e.g. `http://127.0.0.1:18101`) and capture the
/// response. A transport error is itself captured (status 0) so a crashed
/// implementation diffs as a divergence rather than aborting the run.
pub fn fire(client: &Client, base_url: &str, case: &HttpCase) -> CapturedHttp {
    let url = format!("{base_url}{}", case.path);
    let mut req = match case.method.to_ascii_uppercase().as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        other => panic!("unsupported HTTP method in parity case: {other}"),
    };
    for (key, value) in case.headers {
        req = req.header(*key, *value);
    }
    if let Some(body) = &case.body {
        req = req.json(body);
    }

    match req.send() {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Keep only the media type — a `; charset=utf-8` parameter is an
            // env/framework default, not a parity-relevant difference.
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
            let text = resp.text().unwrap_or_default();
            let body = decode_body(content_type.as_deref(), &text);
            CapturedHttp {
                status,
                content_type,
                body,
            }
        }
        Err(err) => CapturedHttp {
            status: 0,
            content_type: None,
            body: json!({ "<transport-error>": err.to_string() }),
        },
    }
}

/// Interpret a response body by content type: JSON, an SSE event list, or
/// raw text.
fn decode_body(content_type: Option<&str>, text: &str) -> Value {
    let ct = content_type.unwrap_or("");
    if ct.contains("text/event-stream") {
        return json!({ "<sse-events>": parse_sse(text) });
    }
    if ct.contains("application/json") || ct.contains("application/x-ndjson") {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return value;
        }
    }
    // Fall back to a best-effort JSON parse, then raw text.
    serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// Parse a Server-Sent Events stream into an ordered event list. Each event
/// is `{ "event": <name|null>, "data": <parsed json | string> }`. Used for
/// the streaming chat / model-pull surfaces.
pub fn parse_sse(text: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    let flush = |name: &mut Option<String>, data: &mut Vec<String>, out: &mut Vec<Value>| {
        if name.is_none() && data.is_empty() {
            return;
        }
        let joined = data.join("\n");
        let parsed = serde_json::from_str::<Value>(&joined)
            .unwrap_or_else(|_| Value::String(joined.clone()));
        out.push(json!({ "event": name.clone(), "data": parsed }));
        *name = None;
        data.clear();
    };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut event_name, &mut data_lines, &mut events);
        } else if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        }
    }
    flush(&mut event_name, &mut data_lines, &mut events);
    events
}

/// Poll `GET {base_url}/health` until the gateway answers, or `timeout`
/// elapses. Returns `false` on timeout so the caller can fail with context.
pub fn wait_ready(client: &Client, base_url: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let health = format!("{base_url}/health");
    while Instant::now() < deadline {
        if client.get(&health).send().is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_event_sequence() {
        let stream = "event: token\ndata: {\"t\":\"hi\"}\n\nevent: done\ndata: {\"done\":true}\n\n";
        let events = parse_sse(stream);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], json!("token"));
        assert_eq!(events[0]["data"]["t"], json!("hi"));
        assert_eq!(events[1]["event"], json!("done"));
    }
}
