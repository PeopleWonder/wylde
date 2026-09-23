//! `voice.wakeword.*` — openWakeWord listener (Slice 11.D).
//!
//! Three verbs:
//!
//! * `voice.wakeword.start` — unary. Boots the mic + pipeline + listener
//!   thread. Idempotent — a second start while running returns
//!   `already_running: true`.
//! * `voice.wakeword.stop` — unary. Tears down the listener and frees the
//!   mic.
//! * `voice.wakeword.events` — streaming. Emits one `event` chunk per
//!   detection plus an `events_complete` summary when the listener
//!   stops or the client closes the stream.
//!
//! Wire payloads:
//!
//! ```jsonc
//! // voice.wakeword.start
//! {
//!   "model_name":     "openWakeWord/hey-jarvis",   // optional, defaults to cfg.wakeword_model
//!   "models_dir":     "C:/.../openwakeword",       // optional, defaults to cfg.wakeword_models_dir
//!   "threshold":      0.5,                          // optional
//!   "cooldown_ms":    1500                          // optional
//! }
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply, StreamSender};

use crate::actions::error::{invalid_request, model_not_loaded};
use crate::config::Config;
use crate::mic::{MicCapture, WAKEWORD_FRAME_SAMPLES};
use crate::state;
use crate::wakeword::{WakeWordConfig, WakeWordListener, WakeWordLoadError, WakeWordPipeline};

/// `voice.wakeword.start` — load the openWakeWord ONNX bundle, open the
/// default mic, spin up the listener thread. Idempotent.
pub async fn handle_wakeword_start(payload: Value) -> Reply {
    if let Some(listener) = state::wakeword_listener() {
        return Reply::ok(json!({
            "already_running": true,
            "model": listener.model_name(),
            "threshold": listener.threshold(),
            "cooldown_ms": listener.cooldown_ms(),
        }));
    }

    let cfg = Config::get();
    let model_name = payload
        .get("model_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.wakeword_model.clone());

    let models_dir = payload
        .get("models_dir")
        .and_then(Value::as_str)
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| cfg.wakeword_models_dir.clone());

    let mut pipeline_cfg = WakeWordConfig::from_layout(&models_dir, &model_name);
    if let Some(t) = read_threshold(&payload) {
        let t = match t {
            Ok(v) => v,
            Err(e) => return Reply::err(e),
        };
        pipeline_cfg.threshold = t;
    }
    if let Some(c) = read_cooldown_ms(&payload) {
        let c = match c {
            Ok(v) => v,
            Err(e) => return Reply::err(e),
        };
        pipeline_cfg.cooldown_ms = c;
    }

    // Cache lookup so a hot-restart skips the 3× ONNX load cost.
    let pipeline = match state::wakeword_pipeline() {
        Some(p) if p.config().mel_model_path == pipeline_cfg.mel_model_path => p,
        _ => {
            let loaded = match WakeWordPipeline::load(pipeline_cfg.clone()) {
                Ok(p) => p,
                Err(WakeWordLoadError::MissingFile(p)) => {
                    return Reply::err(model_not_loaded(format!(
                        "wake-word model file missing: {} \
                         (expected {}/<model_name>/{{melspectrogram,embedding_model,<model_name>}}.onnx)",
                        p.display(),
                        models_dir.display()
                    )));
                }
                Err(WakeWordLoadError::SessionBuild(path, msg)) => {
                    return Reply::err(IpcError::new(
                        "inference_failed",
                        format!("wake-word ort session build for {}: {msg}", path.display()),
                    ));
                }
            };
            let arc = Arc::new(loaded);
            state::set_wakeword_pipeline(Arc::clone(&arc));
            arc
        }
    };

    let mic: Arc<MicCapture> = match state::mic_capture() {
        Some(existing) if existing.chunk_samples() == WAKEWORD_FRAME_SAMPLES => existing,
        Some(other) => {
            return Reply::err(IpcError::new(
                "mic_busy",
                format!(
                    "mic capture already running with chunk_samples={} — wake-word needs {}; \
                     stop the existing capture first",
                    other.chunk_samples(),
                    WAKEWORD_FRAME_SAMPLES
                ),
            ));
        }
        None => {
            let cap = match MicCapture::start(WAKEWORD_FRAME_SAMPLES) {
                Ok(c) => Arc::new(c),
                Err(e) => return Reply::err(crate::actions::mic::mic_error_to_ipc(e)),
            };
            state::set_mic_capture(Arc::clone(&cap));
            cap
        }
    };

    let listener = match WakeWordListener::start(mic, pipeline, model_name.clone()) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            return Reply::err(IpcError::new(
                "inference_failed",
                format!("wake-word listener thread spawn: {e}"),
            ));
        }
    };
    state::set_wakeword_listener(Arc::clone(&listener));

    Reply::ok(json!({
        "already_running": false,
        "model": listener.model_name(),
        "threshold": listener.threshold(),
        "cooldown_ms": listener.cooldown_ms(),
    }))
}

/// `voice.wakeword.stop` — stop the listener and release the mic.
pub async fn handle_wakeword_stop(_payload: Value) -> Reply {
    let was_running = state::take_wakeword_listener().is_some();
    // The listener owned the mic; clearing the listener already
    // released it via Drop. Clear the singleton slot too.
    let _ = state::take_mic_capture(); // wylde-check: discard-result-ok
    Reply::ok(json!({
        "stopped": was_running,
        "was_running": was_running,
    }))
}

/// `voice.wakeword.events` — subscribe to detection events.
pub async fn handle_wakeword_events(_payload: Value, sender: StreamSender) {
    let Some(listener) = state::wakeword_listener() else {
        let _ = sender // wylde-check: discard-result-ok
            .send(Err(invalid_request(
                "no active wake-word listener — call voice.wakeword.start first",
            )))
            .await;
        return;
    };

    let mut rx = listener.subscribe();
    let mut emitted: u64 = 0;
    let mut dropped: u64 = 0;
    let model = listener.model_name().to_owned();

    loop {
        if sender.is_closed() {
            break;
        }
        match rx.recv().await {
            Ok(event) => {
                let payload = json!({
                    "type": "event",
                    "seq": emitted,
                    "elapsed_ms": event.elapsed_ms,
                    "score": event.score,
                    "threshold": event.threshold,
                    "model": event.model,
                });
                if sender.send(Ok(payload)).await.is_err() {
                    break;
                }
                emitted += 1;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                dropped += n;
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    let _ = sender // wylde-check: discard-result-ok
        .send(Ok(json!({
            "type": "events_complete",
            "emitted": emitted,
            "dropped": dropped,
            "model": model,
        })))
        .await;
}

fn read_threshold(payload: &Value) -> Option<Result<f32, IpcError>> {
    let v = payload.get("threshold")?;
    if v.is_null() {
        return None;
    }
    let f = match v.as_f64() {
        Some(f) => f as f32,
        None => return Some(Err(invalid_request("threshold must be a number"))),
    };
    if !(0.0..=1.0).contains(&f) {
        return Some(Err(invalid_request("threshold must be in [0.0, 1.0]")));
    }
    Some(Ok(f))
}

fn read_cooldown_ms(payload: &Value) -> Option<Result<u64, IpcError>> {
    let v = payload.get("cooldown_ms")?;
    if v.is_null() {
        return None;
    }
    let n = match v.as_u64() {
        Some(n) => n,
        None => {
            return Some(Err(invalid_request(
                "cooldown_ms must be a non-negative integer",
            )))
        }
    };
    if n > 60_000 {
        return Some(Err(invalid_request("cooldown_ms must be ≤ 60000 (60 s)")));
    }
    Some(Ok(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn wakeword_stop_when_not_running_returns_false() {
        crate::state::reset_for_tests();
        let r = handle_wakeword_stop(json!({})).await;
        assert!(r.ok);
        assert_eq!(r.data["stopped"], false);
        assert_eq!(r.data["was_running"], false);
    }

    #[tokio::test]
    async fn wakeword_events_without_listener_errors() {
        crate::state::reset_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        handle_wakeword_events(json!({}), tx).await;
        let chunk = rx.recv().await.expect("error chunk emitted");
        let err = chunk.expect_err("missing listener → stream-level error");
        assert_eq!(err.code, "invalid_request");
        assert!(err.message.contains("voice.wakeword.start"));
    }

    #[tokio::test]
    async fn wakeword_start_with_missing_models_returns_model_not_loaded() {
        crate::state::reset_for_tests();
        // Force a models_dir we know doesn't exist so no ONNX files
        // resolve.
        let r = handle_wakeword_start(json!({
            "model_name": "openwakeword-fake-fixture",
            "models_dir": "/no/such/dir/openwakeword-fixture",
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "model_not_loaded");
    }

    #[test]
    fn read_threshold_validates_range() {
        assert!(read_threshold(&json!({})).is_none());
        assert!(read_threshold(&json!({"threshold": null})).is_none());
        assert_eq!(
            read_threshold(&json!({"threshold": 0.5})).unwrap().unwrap(),
            0.5
        );
        let err = read_threshold(&json!({"threshold": 1.5}))
            .unwrap()
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
        let err = read_threshold(&json!({"threshold": -0.1}))
            .unwrap()
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
        let err = read_threshold(&json!({"threshold": "high"}))
            .unwrap()
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn read_cooldown_validates_bounds() {
        assert!(read_cooldown_ms(&json!({})).is_none());
        assert_eq!(
            read_cooldown_ms(&json!({"cooldown_ms": 0}))
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            read_cooldown_ms(&json!({"cooldown_ms": 1_500}))
                .unwrap()
                .unwrap(),
            1_500
        );
        let err = read_cooldown_ms(&json!({"cooldown_ms": 99_999}))
            .unwrap()
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }
}
