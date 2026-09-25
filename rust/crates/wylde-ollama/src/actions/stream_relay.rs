//! Shared NDJSON → IPC-stream relay for the streaming inference actions
//! (`ollama.chat_stream`, `ollama.generate_stream`), plus the
//! evict-on-cancel policy they share.
//!
//! ## Cancellation propagation (design doc Q2)
//!
//! Whether dropping a `reqwest` body stream propagates "stop generating"
//! upstream to Ollama is still unproven (the cancellation spike hasn't
//! run against a live daemon). The conservative default: rely on
//! body-stream drop first, and on a confirmed client disconnect ALSO fire a
//! `POST /api/generate {model, keep_alive: 0}` to evict the model, which
//! forces Ollama to drop any in-flight generation.
//!
//! That eviction is wrong for autocomplete, which cancels on almost every
//! keystroke and would unload the model each time. So it sits behind the
//! `evict_on_cancel` payload knob: default `true` (today's behaviour), and
//! a caller passes `false` to keep the model resident on disconnect. If the
//! spike confirms body-drop suffices, the eviction path can be deleted.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::time::sleep;
use wylde_shared::ipc::{IpcError, StreamSender};

use crate::actions::error::ollama_unreachable_err;
use crate::upstream::Upstream;

/// Payload knob controlling the evict-on-cancel path (default `true`).
pub const EVICT_KNOB: &str = "evict_on_cancel";

/// How a relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayEnd {
    /// Upstream finished (or its body errored and the error was emitted).
    Completed,
    /// The client dropped the IPC stream mid-relay.
    Cancelled,
    /// Ollama sent an inline `{"error": …}` line; it was emitted as an
    /// `ollama_stream_error` frame and the relay stopped.
    Failed,
}

/// Remove [`EVICT_KNOB`] from `body`, returning its value (default `true`).
pub fn take_evict_flag(body: &mut Value) -> bool {
    body.as_object_mut()
        .and_then(|m| m.remove(EVICT_KNOB))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Relay Ollama's NDJSON response body to `sender`, one frame per line.
///
/// Inline `{"error": …}` lines become an `ollama_stream_error` frame and
/// end the relay ([`RelayEnd::Failed`]). A failed `send` means the client
/// dropped the stream ([`RelayEnd::Cancelled`]). A trailing line without a
/// newline is still parsed and emitted when the relay wasn't cancelled.
pub async fn relay_ndjson(resp: reqwest::Response, sender: &StreamSender) -> RelayEnd {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut cancelled = false;

    while let Some(chunk) = stream.next().await {
        // Every sender.send() returns Err on a closed receiver, so a
        // client cancel is observed on the next emit without a select!.
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = sender.send(Err(ollama_unreachable_err(&e))).await; // wylde-check: discard-result-ok
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        // Split on newline — Ollama emits one JSON object per line.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = &line[..line.len() - 1]; // strip the trailing \n
            if line.is_empty() {
                continue;
            }
            let trimmed = trim_cr(line);
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Value>(trimmed) {
                Ok(v) => {
                    // Surface stream-level errors as Err frames.
                    if let Some(err) = v.get("error").and_then(Value::as_str) {
                        let _ = sender // wylde-check: discard-result-ok
                            .send(Err(IpcError::new(
                                "ollama_stream_error",
                                err.to_string(),
                            )))
                            .await;
                        return RelayEnd::Failed;
                    }
                    if sender.send(Ok(v)).await.is_err() {
                        // Client dropped the stream — observe + bail.
                        cancelled = true;
                        break;
                    }
                }
                // Non-JSON line — Ollama doesn't emit these but be
                // robust: skip silently rather than killing the stream.
                Err(_) => continue,
            }
        }
        if cancelled {
            break;
        }
    }

    if cancelled {
        return RelayEnd::Cancelled;
    }
    // A partial line left at end-of-stream (last line without a newline).
    if !buf.is_empty() {
        let trimmed = trim_cr(&buf);
        if let Ok(v) = serde_json::from_slice::<Value>(trimmed) {
            let _ = sender.send(Ok(v)).await; // wylde-check: discard-result-ok
        }
    }
    RelayEnd::Completed
}

/// Fire-and-forget `keep_alive: 0` eviction of `model` after a client
/// cancel. Best-effort: if Ollama is gone or busy, its own keep_alive
/// timer evicts the model eventually.
pub fn spawn_evict(up: Arc<Upstream>, model: String) {
    tokio::spawn(async move {
        // Tiny delay so any in-flight final tokens land first.
        sleep(Duration::from_millis(200)).await;
        let body = json!({"model": model, "keep_alive": 0});
        let _ = up // wylde-check: discard-result-ok
            .client
            .post(format!("{}/api/generate", up.base_url))
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    });
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if let Some(&last) = line.last() {
        if last == b'\r' {
            return &line[..line.len() - 1];
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_flag_defaults_true_and_is_stripped() {
        let mut body = json!({"model": "m"});
        assert!(take_evict_flag(&mut body));
        let mut body = json!({"model": "m", "evict_on_cancel": false});
        assert!(!take_evict_flag(&mut body));
        assert!(body.get(EVICT_KNOB).is_none(), "knob must not reach Ollama");
        let mut body = json!({"model": "m", "evict_on_cancel": true});
        assert!(take_evict_flag(&mut body));
    }

    #[test]
    fn trim_cr_strips_only_a_trailing_cr() {
        assert_eq!(trim_cr(b"abc\r"), b"abc");
        assert_eq!(trim_cr(b"abc"), b"abc");
        assert_eq!(trim_cr(b""), b"");
    }
}
