//! `ollama.pull` — POST /api/pull (NDJSON stream).
//!
//! Streaming action. Port of Python `ollama_client.pull_model`
//! including the retry-on-transient-error loop (the regex match on
//! "context deadline exceeded|EOF|...", the 6-attempt budget, the
//! retry-event-as-progress-chunk UX).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::time::sleep;
use wylde_shared::ipc::{IpcError, StreamSender};

use crate::actions::error::excerpt;
use crate::upstream::Upstream;

const MAX_PULL_ATTEMPTS: usize = 6;
const RETRY_BASE_DELAY_S: u64 = 3;
const BODY_EXCERPT_CAP: usize = 300;

/// Regex-equivalent set of substrings considered transient. Lowercased
/// match against the error message. We use plain `str::contains` instead
/// of pulling in `regex` — the original Python regex is also literal
/// alternation.
const TRANSIENT_SUBSTRINGS: &[&str] = &[
    "context deadline exceeded",
    "deadline exceeded",
    "eof",
    "connection reset",
    "socket hang up",
    "econnreset",
    "etimedout",
    "stream ended before reporting success",
    "network",
    "fetch failed",
];

fn is_transient(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    TRANSIENT_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

/// Resolve user-facing model names to the form Ollama's /api/pull
/// expects. Port of `ollama_client.normalize_pull_name`.
fn normalize_pull_name(name: &str) -> String {
    if name.is_empty()
        || name.starts_with("hf.co/")
        || name.starts_with("library/")
        || !name.contains('/')
    {
        return name.to_owned();
    }
    format!("hf.co/{name}")
}

pub async fn handle_pull(payload: Value, sender: StreamSender, up: Arc<Upstream>) {
    // Accept either `{"name": ...}` or `{"model": ...}` for ergonomic
    // parity with Python's pull_model(name).
    let raw_name = match payload
        .get("name")
        .or_else(|| payload.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    {
        Some(n) => n,
        None => {
            let _ = sender // wylde-check: discard-result-ok
                .send(Err(IpcError::new(
                    "invalid_request",
                    "payload requires 'name' or 'model'",
                )))
                .await;
            return;
        }
    };

    let resolved = normalize_pull_name(&raw_name);
    let mut last_err: Option<String> = None;

    for attempt in 1..=MAX_PULL_ATTEMPTS {
        match pull_once(&resolved, &sender, up.clone()).await {
            Ok(()) => return,
            Err(msg) => {
                last_err = Some(msg.clone());
                if !is_transient(&msg) || attempt >= MAX_PULL_ATTEMPTS {
                    let _ = sender // wylde-check: discard-result-ok
                        .send(Err(IpcError::new("pull_failed", msg)))
                        .await;
                    return;
                }
                let delay = RETRY_BASE_DELAY_S * attempt as u64;
                let evt = json!({
                    "status": format!(
                        "retry {attempt}/{}: {msg} — resuming in {delay}s",
                        MAX_PULL_ATTEMPTS - 1,
                    ),
                });
                if sender.send(Ok(evt)).await.is_err() {
                    // Client gave up while we were retrying — abandon.
                    return;
                }
                sleep(Duration::from_secs(delay)).await;
            }
        }
    }

    // Unreachable in the loop above, but keep the closing path explicit.
    if let Some(msg) = last_err {
        let _ = sender.send(Err(IpcError::new("pull_failed", msg))).await; // wylde-check: discard-result-ok
    }
}

/// One attempt at a streaming pull. Returns `Err(msg)` to trigger the
/// retry loop; returns `Ok(())` on a stream that saw a `status: success`.
async fn pull_once(
    resolved: &str,
    sender: &StreamSender,
    up: Arc<Upstream>,
) -> Result<(), String> {
    let body = json!({"name": resolved, "stream": true});
    let url = format!("{}/api/pull", up.base_url);
    let resp = match up
        .client
        .post(&url)
        .json(&body)
        // Connect timeout only — pull body has no bounded duration.
        .timeout(Duration::from_secs(60 * 60))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(format!("Cannot reach Ollama at {}: {e}", up.base_url)),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "{status} {} [{url}] [model: {resolved}]",
            excerpt(&body_text, BODY_EXCERPT_CAP)
        ));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut saw_success = false;
    let mut saw_any_line = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Err(format!("network: {e}")),
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let trimmed = if line.last() == Some(&b'\r') {
                &line[..line.len() - 1]
            } else {
                line
            };
            if trimmed.is_empty() {
                continue;
            }
            saw_any_line = true;
            let v: Value = match serde_json::from_slice(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // skip un-parseable lines
            };
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                return Err(err.to_owned());
            }
            if v.get("status").and_then(Value::as_str) == Some("success") {
                saw_success = true;
            }
            if sender.send(Ok(v)).await.is_err() {
                // Client disconnected mid-pull — propagate as "cancelled".
                return Ok(());
            }
        }
    }

    if !saw_success {
        let detail = if saw_any_line {
            "Ollama stream ended before reporting success — the pull may have been interrupted."
        } else {
            "Ollama returned an empty response — pull did not start."
        };
        return Err(detail.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn fake_upstream() -> (MockServer, Arc<Upstream>) {
        let server = MockServer::start().await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    #[test]
    fn normalize_passes_through_known_prefixes() {
        assert_eq!(normalize_pull_name("qwen2.5:0.5b"), "qwen2.5:0.5b");
        assert_eq!(normalize_pull_name("library/qwen"), "library/qwen");
        assert_eq!(normalize_pull_name("hf.co/foo/bar"), "hf.co/foo/bar");
        // Slashes without scheme get the hf.co/ prefix added.
        assert_eq!(normalize_pull_name("foo/bar:Q4"), "hf.co/foo/bar:Q4");
    }

    #[test]
    fn transient_detector_matches_ollama_messages() {
        assert!(is_transient("context deadline exceeded"));
        assert!(is_transient("EOF reading body"));
        assert!(is_transient("ECONNRESET while pulling"));
        assert!(is_transient("Network is unreachable"));
        assert!(!is_transient("invalid model name"));
        assert!(!is_transient("manifest not found"));
    }

    #[tokio::test]
    async fn pull_happy_path_emits_progress_and_success() {
        let (server, up) = fake_upstream().await;
        let ndjson = "\
            {\"status\":\"pulling manifest\"}\n\
            {\"status\":\"downloading\",\"completed\":50,\"total\":100}\n\
            {\"status\":\"success\"}\n";
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(8);
        handle_pull(json!({"name": "qwen"}), tx, up).await;

        let mut chunks: Vec<Value> = Vec::new();
        while let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            match item {
                Ok(v) => chunks.push(v),
                Err(e) => panic!("pull errored: {e:?}"),
            }
        }
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0]["status"], "pulling manifest");
        assert_eq!(chunks[1]["completed"], 50);
        assert_eq!(chunks[2]["status"], "success");
    }

    #[tokio::test]
    async fn pull_non_transient_inline_error_surfaces_pull_failed() {
        let (server, up) = fake_upstream().await;
        let ndjson = "{\"error\":\"manifest not found\"}\n";
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(8);
        handle_pull(json!({"name": "ghost"}), tx, up).await;

        let mut saw_err = false;
        while let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            if let Err(e) = item {
                assert_eq!(e.code, "pull_failed");
                assert!(e.message.contains("manifest not found"));
                saw_err = true;
            }
        }
        assert!(saw_err, "expected pull_failed error frame");
    }

    #[tokio::test]
    async fn pull_requires_name() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        let (tx, mut rx) = mpsc::channel(4);
        handle_pull(json!({}), tx, up).await;
        let item = rx.recv().await.expect("a frame");
        match item {
            Err(e) => assert_eq!(e.code, "invalid_request"),
            Ok(v) => panic!("expected Err, got {v:?}"),
        }
    }

    #[tokio::test]
    async fn pull_empty_response_is_pull_failed() {
        let (server, up) = fake_upstream().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let (tx, mut rx) = mpsc::channel(4);
        handle_pull(json!({"name": "qwen"}), tx, up).await;
        let item = rx.recv().await.expect("a frame");
        match item {
            Err(e) => {
                assert_eq!(e.code, "pull_failed");
                assert!(
                    e.message.contains("pull did not start"),
                    "got {}",
                    e.message
                );
            }
            Ok(v) => panic!("expected Err, got {v:?}"),
        }
    }
}
