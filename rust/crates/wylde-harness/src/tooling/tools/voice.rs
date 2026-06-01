//! `voice.*` — active model-callable tools.
//!
//! Thin wrappers over the matching `wylde-voice` IPC actions. The
//! model-callable surface stays unary by design.
//!
//! ## Slice 11.D — mic + wake-word (start/stop)
//!
//! `voice.mic.start` / `voice.mic.stop` / `voice.wakeword.start` /
//! `voice.wakeword.stop` map directly to unary IPC actions. The matching
//! streaming primitives (`voice.mic.chunks`, `voice.wakeword.events`)
//! live in `wylde-voice` and are consumed by the orchestrator + GUI via
//! `send_action_stream`; they are catalogued as deferred in
//! [`super::deferred`] because there is no useful unary collapse for
//! continuous PCM / detection events.
//!
//! ## Slice 11.E — transcribe + synthesize (unary + aggregated streaming)
//!
//! Slice 11.E cutover (2026-05-26) promoted the four transcribe/
//! synthesize entries out of `deferred`. The unary pair
//! (`voice.transcribe`, `voice.synthesize`) are simple bridges. The
//! streaming pair (`voice.transcribe_stream`, `voice.synthesize_stream`)
//! are **aggregator bridges** — internally they call
//! `send_action_stream`, collect every chunk into a JSON array, and
//! return the aggregate plus the natural "final" chunk's text or audio.
//! This lets a model invoke the streaming endpoint by name without
//! having to consume a stream — the actual streaming path remains
//! available for the orchestrator and GUI to call directly.

use futures::StreamExt;
use serde_json::{json, Value};
use wylde_shared::ipc::{send_action_stream, IpcError};

use crate::tooling::registry::{entry_active, param, param_default, Registry};

const VOICE_SERVICE: &str = "wylde-voice";

pub fn register(reg: &mut Registry) {
    // ── Slice 11.E — transcribe / synthesize (unary + aggregated stream) ─

    reg.insert(entry_active(
        "voice_transcribe",
        "voice.transcribe",
        "voice",
        "Speech-to-text via Whisper. WAV bytes or audio_path → transcript. \
         Returns the final transcript plus latency breakdown.",
        vec![
            param("audio_path", "string", false, "Path to WAV file on disk."),
            param("audio_b64", "string", false, "Base64-encoded WAV bytes."),
            param_default("language", "string", "ISO 639-1 language code", json!("en")),
        ],
        false,
        |args, _| async move { run_transcribe(args).await },
    ));

    reg.insert(entry_active(
        "voice_synthesize",
        "voice.synthesize",
        "voice",
        "Text-to-speech via Kokoro. Phonemes → 16-bit PCM WAV (base64). \
         Slice 11.B+ is phoneme-only; text path defers pending espeak-ng.",
        vec![
            param("phonemes", "string", true, "IPA phoneme string (espeak-ng en-us)."),
            param_default("voice", "string", "Kokoro voice name", json!("af_heart")),
            param_default("speed", "number", "Playback rate multiplier [0.5, 2.0]", json!(1.0)),
        ],
        false,
        |args, _| async move { run_synthesize(args).await },
    ));

    reg.insert(entry_active(
        "voice_transcribe_stream",
        "voice.transcribe_stream",
        "voice",
        "Streaming Whisper STT. Same payload as voice.transcribe. \
         Aggregator bridge — collects encoder_complete + token chunks + \
         transcript_complete into one reply.",
        vec![
            param("audio_path", "string", false, "Path to WAV file on disk."),
            param("audio_b64", "string", false, "Base64-encoded WAV bytes."),
            param_default("language", "string", "ISO 639-1 language code", json!("en")),
        ],
        false,
        |args, _| async move { run_transcribe_stream(args).await },
    ));

    reg.insert(entry_active(
        "voice_synthesize_stream",
        "voice.synthesize_stream",
        "voice",
        "Streaming Kokoro TTS. Same payload as voice.synthesize. \
         Aggregator bridge — collects synthesize_start + audio_chunks + \
         synthesize_complete into one reply.",
        vec![
            param("phonemes", "string", true, "IPA phoneme string (espeak-ng en-us)."),
            param_default("voice", "string", "Kokoro voice name", json!("af_heart")),
            param_default("speed", "number", "Playback rate multiplier [0.5, 2.0]", json!(1.0)),
        ],
        false,
        |args, _| async move { run_synthesize_stream(args).await },
    ));

    // ── Slice 11.D — mic + wake-word ─────────────────────────────────

    reg.insert(entry_active(
        "voice_mic_start",
        "voice.mic.start",
        "voice",
        "Open the default microphone (cpal). Singleton — second call \
         returns the running capture's metadata. Audio is downmixed + \
         resampled to 16 kHz mono i16 inside the worker thread.",
        vec![param_default(
            "chunk_samples",
            "number",
            "Frame size in samples (1280 = 80 ms for wake-word; \
             800 = 50 ms for general capture)",
            json!(800),
        )],
        true,
        |args, _| async move { run_mic_start(args).await },
    ));

    reg.insert(entry_active(
        "voice_mic_stop",
        "voice.mic.stop",
        "voice",
        "Stop the active mic capture. No-op when nothing is running.",
        vec![],
        true,
        |_args, _| async move { run_mic_stop().await },
    ));

    reg.insert(entry_active(
        "voice_wakeword_start",
        "voice.wakeword.start",
        "voice",
        "Start the openWakeWord listener. Auto-creates the mic capture \
         at the 1280-sample frame size openWakeWord expects.",
        vec![
            param_default(
                "model_name",
                "string",
                "openWakeWord model name (defaults to the configured \
                 wakeword_model, e.g. 'openWakeWord/hey-jarvis')",
                json!(null),
            ),
            param_default(
                "threshold",
                "number",
                "Detection threshold in [0.0, 1.0] (default 0.5)",
                json!(null),
            ),
            param_default(
                "cooldown_ms",
                "number",
                "Cooldown after a detection before the next can fire (default 1500)",
                json!(null),
            ),
        ],
        true,
        |args, _| async move { run_wakeword_start(args).await },
    ));

    reg.insert(entry_active(
        "voice_wakeword_stop",
        "voice.wakeword.stop",
        "voice",
        "Stop the active wake-word listener and release its mic. \
         No-op when nothing is running.",
        vec![],
        true,
        |_args, _| async move { run_wakeword_stop().await },
    ));
}

// ── Handlers ─────────────────────────────────────────────────────────

/// Forward a unary call to wylde-voice and return the envelope.
async fn forward_unary(action: &'static str, payload: Value, op: &'static str) -> Result<Value, IpcError> {
    let reply = wylde_shared::ipc::send_action(VOICE_SERVICE, action, payload).await;
    Ok(envelope_from_reply(&reply, op))
}

/// Open a streaming action and aggregate every chunk into one JSON
/// reply. Final shape: `{status, chunks: [...], final: <last chunk>}`
/// — the model gets the full stream as a single value while the
/// underlying primitive remains streaming for orchestrator/GUI use.
async fn run_aggregated_stream(
    action: &'static str,
    payload: Value,
    op: &'static str,
) -> Result<Value, IpcError> {
    let mut stream = send_action_stream(VOICE_SERVICE, action, payload);
    let mut chunks: Vec<Value> = Vec::new();
    let mut stream_err: Option<IpcError> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(v) => chunks.push(v),
            Err(e) => {
                stream_err = Some(e);
                break;
            }
        }
    }
    if let Some(err) = stream_err {
        return Ok(json!({
            "status": "error",
            "error": format!("voice.{op} stream failed: {}: {}", err.code, err.message),
            "chunks": chunks,
        }));
    }
    let final_chunk = chunks.last().cloned().unwrap_or(Value::Null);
    Ok(json!({
        "status": "success",
        "chunks": chunks,
        "final": final_chunk,
    }))
}

async fn run_transcribe(args: Value) -> Result<Value, IpcError> {
    forward_unary("voice.transcribe", args, "transcribe").await
}

async fn run_synthesize(args: Value) -> Result<Value, IpcError> {
    forward_unary("voice.synthesize", args, "synthesize").await
}

async fn run_transcribe_stream(args: Value) -> Result<Value, IpcError> {
    run_aggregated_stream("voice.transcribe_stream", args, "transcribe_stream").await
}

async fn run_synthesize_stream(args: Value) -> Result<Value, IpcError> {
    run_aggregated_stream("voice.synthesize_stream", args, "synthesize_stream").await
}

async fn run_mic_start(args: Value) -> Result<Value, IpcError> {
    let mut payload = json!({});
    if let Some(v) = args.get("chunk_samples") {
        payload["chunk_samples"] = v.clone();
    }
    let reply = wylde_shared::ipc::send_action(VOICE_SERVICE, "voice.mic.start", payload).await;
    Ok(envelope_from_reply(&reply, "mic.start"))
}

async fn run_mic_stop() -> Result<Value, IpcError> {
    let reply = wylde_shared::ipc::send_action(VOICE_SERVICE, "voice.mic.stop", json!({})).await;
    Ok(envelope_from_reply(&reply, "mic.stop"))
}

async fn run_wakeword_start(args: Value) -> Result<Value, IpcError> {
    let mut payload = json!({});
    for key in ["model_name", "models_dir", "threshold", "cooldown_ms"] {
        if let Some(v) = args.get(key) {
            if !v.is_null() {
                payload[key] = v.clone();
            }
        }
    }
    let reply =
        wylde_shared::ipc::send_action(VOICE_SERVICE, "voice.wakeword.start", payload).await;
    Ok(envelope_from_reply(&reply, "wakeword.start"))
}

async fn run_wakeword_stop() -> Result<Value, IpcError> {
    let reply =
        wylde_shared::ipc::send_action(VOICE_SERVICE, "voice.wakeword.stop", json!({})).await;
    Ok(envelope_from_reply(&reply, "wakeword.stop"))
}

fn envelope_from_reply(reply: &wylde_shared::ipc::Reply, op: &str) -> Value {
    if reply.ok {
        let mut out = reply.data.clone();
        if let Value::Object(ref mut map) = out {
            map.entry("status".to_owned()).or_insert(json!("success"));
        }
        out
    } else {
        let detail = reply
            .error
            .as_ref()
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| format!("voice.{op} pipe unreachable"));
        json!({
            "status": "error",
            "error": format!("voice.{op} failed: {detail}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::registry::HandlerKind;

    #[test]
    fn register_promotes_all_active_tools() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            // Slice 11.E (2026-05-26)
            "voice_transcribe",
            "voice_synthesize",
            "voice_transcribe_stream",
            "voice_synthesize_stream",
            // Slice 11.D
            "voice_mic_start",
            "voice_mic_stop",
            "voice_wakeword_start",
            "voice_wakeword_stop",
        ] {
            let entry = reg.lookup(id).unwrap_or_else(|| panic!("missing {id}"));
            assert!(
                matches!(entry.kind, HandlerKind::Active(_)),
                "{id} not active"
            );
        }
    }

    #[test]
    fn dotted_aliases_resolve_correctly() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for (dotted, id) in [
            ("voice.transcribe", "voice_transcribe"),
            ("voice.synthesize", "voice_synthesize"),
            ("voice.transcribe_stream", "voice_transcribe_stream"),
            ("voice.synthesize_stream", "voice_synthesize_stream"),
            ("voice.mic.start", "voice_mic_start"),
            ("voice.mic.stop", "voice_mic_stop"),
            ("voice.wakeword.start", "voice_wakeword_start"),
            ("voice.wakeword.stop", "voice_wakeword_stop"),
        ] {
            assert_eq!(
                reg.lookup(dotted).unwrap().id,
                id,
                "alias {dotted} should resolve to {id}",
            );
        }
    }

    #[test]
    fn destructive_flags_match_action_semantics() {
        // Read-only: transcribe / synthesize (pure inference, no device).
        // Destructive: mic / wake-word (open OS audio device + threads).
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "voice_transcribe",
            "voice_synthesize",
            "voice_transcribe_stream",
            "voice_synthesize_stream",
        ] {
            assert!(
                !reg.lookup(id).unwrap().destructive,
                "{id} should NOT be destructive (pure inference)"
            );
        }
        for id in [
            "voice_mic_start",
            "voice_mic_stop",
            "voice_wakeword_start",
            "voice_wakeword_stop",
        ] {
            assert!(
                reg.lookup(id).unwrap().destructive,
                "{id} should be destructive (opens OS audio device)"
            );
        }
    }

    #[test]
    fn envelope_from_failed_reply_returns_error_status() {
        let reply = wylde_shared::ipc::Reply::err(IpcError::new(
            "pipe_connect",
            "wylde-voice not running",
        ));
        let env = envelope_from_reply(&reply, "mic.start");
        assert_eq!(env["status"], "error");
        let msg = env["error"].as_str().unwrap();
        assert!(msg.contains("pipe_connect"));
        assert!(msg.contains("mic.start"));
    }

    #[test]
    fn envelope_from_ok_reply_preserves_payload_and_adds_status() {
        let reply = wylde_shared::ipc::Reply::ok(json!({
            "already_running": false,
            "chunk_samples": 800,
        }));
        let env = envelope_from_reply(&reply, "mic.start");
        assert_eq!(env["status"], "success");
        assert_eq!(env["already_running"], false);
        assert_eq!(env["chunk_samples"], 800);
    }
}
