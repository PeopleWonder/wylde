//! `voice.health` — service liveness + backend report.
//!
//! Quick reply suitable for dashboards and `wylde_check`. Does NOT load
//! any model — that's `voice.list_models` (still cheap) or `voice.transcribe`
//! (full warm-up). Reports the configured backend so an operator can see
//! at a glance whether NPU is requested.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::config::Config;
use crate::state;

pub async fn handle_health(_payload: Value) -> Reply {
    let cfg = Config::get();
    let whisper_loaded = state::whisper_encoder().is_some();
    let (whisper_device, whisper_path) = match state::whisper_encoder() {
        Some(enc) => (Some(enc.device().to_owned()), Some(enc.encoder_path().display().to_string())),
        None => (None, None),
    };
    let tts_loaded = state::kokoro_synth().is_some();
    let tts_voices_loaded = state::kokoro_voices().is_some();
    let mic_running = state::mic_capture().is_some();
    let wakeword_running = state::wakeword_listener().is_some();
    let wakeword_model = state::wakeword_listener().map(|l| l.model_name().to_owned());
    Reply::ok(json!({
        "ok": true,
        "service": "wylde-voice",
        "stt_backend": cfg.stt_backend.as_str(),
        "stt_model": cfg.stt_model.clone(),
        "stt_loaded": whisper_loaded,
        "stt_device": whisper_device,
        "stt_encoder_path": whisper_path,
        "tts_voice": cfg.tts_voice.clone(),
        "tts_loaded": tts_loaded,
        "tts_voices_loaded": tts_voices_loaded,
        "mic_running": mic_running,
        "wakeword_running": wakeword_running,
        "wakeword_model": wakeword_model,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_replies_ok_without_loading_anything() {
        let r = handle_health(json!({})).await;
        assert!(r.ok);
        assert_eq!(r.data["service"], "wylde-voice");
        assert_eq!(r.data["ok"], true);
        // Backend field must always be present so dashboards never
        // null-dereference.
        assert!(r.data["stt_backend"].is_string());
        assert!(r.data["stt_model"].is_string());
        // Default state: nothing loaded.
        assert_eq!(r.data["tts_loaded"], false);
        assert_eq!(r.data["wakeword_running"], false);
        assert_eq!(r.data["mic_running"], false);
    }
}
