//! Service entrypoint: register the `voice.*` actions on the shared
//! IPC registry. Same shape as `wylde-ollama::service`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{
    register_action_with_meta, register_streaming_action_with_meta, unregister_action, StreamSender,
};

use crate::actions::{health, mic, models, session, synthesize, transcribe, wakeword};

const ALL_ACTIONS: [&str; 25] = [
    "voice.health",
    "voice.list_models",
    "voice.download_models",
    "voice.download_status",
    "voice.transcribe",
    "voice.transcribe_stream",
    "voice.synthesize",
    "voice.synthesize_stream",
    "voice.mic.start",
    "voice.mic.stop",
    "voice.mic.chunks",
    "voice.wakeword.start",
    "voice.wakeword.stop",
    "voice.wakeword.events",
    // Slice 11.E+ — GUI-facing surface ported from Voice/pipe.py.
    "voice.toggle",
    "voice.start_session",
    "voice.end_session",
    "voice.set_mode",
    "voice.get_mode",
    "voice.set_active_conversation",
    "voice.get_status",
    "voice.check_wake_word_model",
    "voice.pull_wake_word_model",
    "voice.wake_word_pull_status",
    "voice.subscribe_status",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `voice.*` action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        "voice.health",
        |payload: Value| async move { health::handle_health(payload).await },
        "Liveness probe + backend report. Reply: \
         {ok, service, stt_backend, stt_model, stt_loaded, stt_device, \
          stt_encoder_path, tts_voice, tts_loaded, wakeword_running}. \
         Does NOT load any model.",
        "wylde_voice::actions::health",
    );

    register_action_with_meta(
        "voice.list_models",
        |payload: Value| async move { models::handle_list_models(payload).await },
        "Enumerate Whisper / Kokoro snapshots present in the HF cache. \
         Reports OpenVINO IR sibling presence too. Cheap — does NOT load weights.",
        "wylde_voice::actions::models",
    );

    register_action_with_meta(
        "voice.download_models",
        |payload: Value| async move { models::handle_download_models(payload).await },
        "Rust-native model bootstrap (Slice 4): fetch Whisper STT + Kokoro \
         TTS files into the HF cache, verifying git-LFS SHA-256s and \
         assembling voices.npz. Replaces Voice/download_models.py. Returns \
         immediately {job_id, stt_model, kokoro_model}; poll \
         voice.download_status. Idempotent — present files are skipped.",
        "wylde_voice::actions::models",
    );

    register_action_with_meta(
        "voice.download_status",
        |payload: Value| async move { models::handle_download_status(payload).await },
        "Poll a voice.download_models job. Payload: {job_id}. Reply: \
         {job_id, state: in_progress|done|failed, done?, total?, \
         whisper_dir?, kokoro_dir?, error?}.",
        "wylde_voice::actions::models",
    );

    register_action_with_meta(
        "voice.transcribe",
        |payload: Value| async move { transcribe::handle_transcribe(payload).await },
        "WAV bytes → Whisper STT text. Payload: \
         {audio_path | audio_b64, language?, max_new_tokens?}. Reply: \
         {audio_seconds, device, backend, encoder_output_shape, \
          encoder_inference_ms, decoder_inference_ms, total_inference_ms, \
          text, language, token_count}. Encoder runs on the configured \
          backend (CPU EP default, OpenVINO NPU EP when enabled); decoder \
          runs on the CPU EP regardless.",
        "wylde_voice::actions::transcribe",
    );

    register_action_with_meta(
        "voice.synthesize",
        |payload: Value| async move { synthesize::handle_synthesize(payload).await },
        "Phoneme string → Kokoro TTS audio. Payload: \
         {phonemes, voice?, speed?}. Reply: \
         {audio_seconds, device, backend, inference_ms, sample_rate, format, \
          voice, speed, phoneme_token_count, truncated, audio_samples, audio}. \
          Audio is base64-encoded 16-bit PCM WAV at 24 kHz. CPU EP only \
          (Kokoro's dynamic-shape inputs preclude OpenVINO VPUX). Slice 11.B \
          is phoneme-only — text-path phonemisation defers to 11.B+.",
        "wylde_voice::actions::synthesize",
    );

    register_streaming_action_with_meta(
        "voice.transcribe_stream",
        |payload: Value, sender: StreamSender| async move {
            transcribe::handle_transcribe_stream(payload, sender).await;
        },
        "Streaming. Same payload shape as voice.transcribe \
         ({audio_path|audio_b64, language?, max_new_tokens?}). Emits one \
         `encoder_complete` chunk after the Whisper encoder finishes, then \
         one `token` chunk per decoder step carrying the cumulative-delta \
         text, then one final `transcript_complete` chunk with the full \
         transcript + latency breakdown. Slice 11.C.",
        "wylde_voice::actions::transcribe",
    );

    register_streaming_action_with_meta(
        "voice.synthesize_stream",
        |payload: Value, sender: StreamSender| async move {
            synthesize::handle_synthesize_stream(payload, sender).await;
        },
        "Streaming. Same payload shape as voice.synthesize \
         ({phonemes, voice?, speed?}). Splits the phoneme string at \
         sentence-shaped terminators (./?/!/;/newline) and emits one \
         `synthesize_start` chunk, one `audio_chunk` per sub-utterance \
         (each an independently playable base64 WAV), and a final \
         `synthesize_complete` summary. CPU EP only — same constraint as \
         voice.synthesize. Slice 11.C.",
        "wylde_voice::actions::synthesize",
    );

    // ── Slice 11.D — mic + wake-word ─────────────────────────────────

    register_action_with_meta(
        "voice.mic.start",
        |payload: Value| async move { mic::handle_mic_start(payload).await },
        "Open the default input device (cpal). Payload: \
         {chunk_samples?}. Reply: {already_running, chunk_samples, \
         sample_rate, channels, input_sample_rate, input_channels}. \
         Singleton — second call returns the running capture's metadata \
         with already_running=true. Output is 16 kHz mono i16 regardless \
         of the device's native format.",
        "wylde_voice::actions::mic",
    );

    register_action_with_meta(
        "voice.mic.stop",
        |payload: Value| async move { mic::handle_mic_stop(payload).await },
        "Stop the active mic capture (no payload). Reply: \
         {stopped, was_running}. No-op when nothing is running.",
        "wylde_voice::actions::mic",
    );

    register_streaming_action_with_meta(
        "voice.mic.chunks",
        |payload: Value, sender: StreamSender| async move {
            mic::handle_mic_chunks(payload, sender).await;
        },
        "Streaming. Subscribe to the active mic capture's PCM broadcast. \
         Emits one `chunk` payload per frame {seq, samples, sample_rate, \
         format='pcm_s16le', audio_b64} and a final `chunks_complete` \
         {emitted, dropped}. Errors with invalid_request if no capture is \
         active — call voice.mic.start first.",
        "wylde_voice::actions::mic",
    );

    register_action_with_meta(
        "voice.wakeword.start",
        |payload: Value| async move { wakeword::handle_wakeword_start(payload).await },
        "Start the openWakeWord listener (3-stage ONNX pipeline: \
         melspectrogram → embedding → classifier). Payload: \
         {model_name?, models_dir?, threshold?, cooldown_ms?}. Reply: \
         {already_running, model, threshold, cooldown_ms}. Auto-creates \
         the mic capture singleton at the 1280-sample (80 ms) frame size \
         openWakeWord requires.",
        "wylde_voice::actions::wakeword",
    );

    register_action_with_meta(
        "voice.wakeword.stop",
        |payload: Value| async move { wakeword::handle_wakeword_stop(payload).await },
        "Stop the active wake-word listener and release the mic. \
         Reply: {stopped, was_running}. No-op when nothing is running.",
        "wylde_voice::actions::wakeword",
    );

    register_streaming_action_with_meta(
        "voice.wakeword.events",
        |payload: Value, sender: StreamSender| async move {
            wakeword::handle_wakeword_events(payload, sender).await;
        },
        "Streaming. Subscribe to the active wake-word listener's \
         detection broadcast. Emits one `event` payload per detection \
         {seq, elapsed_ms, score, threshold, model} and a final \
         `events_complete` {emitted, dropped, model}. Errors with \
         invalid_request if no listener is active — call \
         voice.wakeword.start first.",
        "wylde_voice::actions::wakeword",
    );

    // ── Slice 11.E+ — GUI-facing surface (port of Voice/pipe.py) ─────

    register_action_with_meta(
        "voice.toggle",
        |payload: Value| async move { session::handle_voice_toggle(payload).await },
        "Run one full capture → STT → chat → TTS → play session. \
         Payload: {max_seconds?, model?}. Reply: SessionResult shape \
         (session_id, conversation_id, transcript, response, aborted, \
         error, timings_ms). One-at-a-time — concurrent calls return \
         `busy`.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.start_session",
        |payload: Value| async move { session::handle_voice_toggle(payload).await },
        "Alias for voice.toggle. Reserved for a future async start; \
         today identical to voice.toggle.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.end_session",
        |payload: Value| async move { session::handle_voice_end_session(payload).await },
        "Cancel the in-flight capture so the orchestrator finalises \
         on whatever audio it has so far. No-op when no session is \
         running. Reply: {ok, had_active_session, state}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.set_mode",
        |payload: Value| async move { session::handle_voice_set_mode(payload).await },
        "Switch the capture mode. Payload: {mode: 'push_to_talk' | \
         'always_on'}. Persists to voice_config.json so the choice \
         survives a restart. Reply: {mode}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.get_mode",
        |payload: Value| async move { session::handle_voice_get_mode(payload).await },
        "Return the current capture mode. Reply: {mode}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.set_active_conversation",
        |payload: Value| async move {
            session::handle_voice_set_active_conversation(payload).await
        },
        "Bind the voice service to a conversation id so transcribed \
         utterances are routed there. Payload: {conversation_id}. \
         Reply: {conversation_id}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.get_status",
        |payload: Value| async move { session::handle_voice_get_status(payload).await },
        "Full state snapshot for the dashboard. Reply: {state, mode, \
         listening, last_error, active_session, active_conversation_id, \
         wake_word_installed, wake_word_model}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.check_wake_word_model",
        |payload: Value| async move {
            session::handle_voice_check_wake_word_model(payload).await
        },
        "Check the model_registry for the openWakeWord bundle. \
         Payload: {model?} (defaults to the configured wake-word model). \
         Reply: {installed, model}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.pull_wake_word_model",
        |payload: Value| async move {
            session::handle_voice_pull_wake_word_model(payload).await
        },
        "Kick a background pull of the wake-word bundle into \
         <wakeword_models_dir>/<vendor>/<name>/. Returns immediately with \
         a job_id the GUI can poll via voice.wake_word_pull_status. \
         Payload: {model?}. Reply: {job_id, model}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.wake_word_pull_status",
        |payload: Value| async move {
            session::handle_voice_wake_word_pull_status(payload).await
        },
        "Poll the in-progress / done / failed status of a wake-word \
         pull. Payload: {job_id}. Reply: {job_id, state, bundle_dir?, \
         error?}.",
        "wylde_voice::actions::session",
    );

    register_action_with_meta(
        "voice.subscribe_status",
        |payload: Value| async move {
            session::handle_voice_subscribe_status(payload).await
        },
        "Long-poll cursor over the status event ring. Payload: \
         {cursor?, max_wait_ms?} (max_wait_ms capped at 25 s). \
         Reply: {events: [...], next_cursor}. Feed next_cursor back \
         to chain polls.",
        "wylde_voice::actions::session",
    );

    tracing::info!("wylde-voice: registered {} actions", ALL_ACTIONS.len());
}

/// Drop loaded model handles + their VRAM leases. Called from
/// the binary's shutdown path so the broker reclaims the encoder's
/// buffer footprint cleanly. Also tears down the mic + wake-word
/// listener singletons (Slice 11.D) so the cpal worker thread joins
/// and the OS device handle drops before the process exits.
pub fn stop() {
    crate::state::clear_whisper_encoder();
    crate::state::clear_kokoro();
    crate::state::clear_voice_io();
}

/// Test-only: unregister every action and reset the install flag.
pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
    crate::state::reset_for_tests();
}

pub fn all_actions() -> &'static [&'static str] {
    &ALL_ACTIONS
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_action_meta};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // list_action_meta covers both unary and streaming registrations,
        // which matters now that the surface includes voice.*_stream.
        let names: Vec<String> = list_action_meta()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for n in ALL_ACTIONS {
            assert!(names.contains(&n.to_string()), "missing {n}");
        }
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        reset_for_tests();
    }

    #[tokio::test]
    async fn dispatching_unknown_subaction_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "voice.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn voice_health_dispatches_cleanly() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "voice.health",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "voice.health should reply ok");
        assert_eq!(reply.data["service"], "wylde-voice");
        reset_for_tests();
    }

    #[tokio::test]
    async fn voice_synthesize_dispatches_cleanly() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // Missing phonemes → invalid_request. This exercises the
        // dispatch path without needing the Kokoro model on disk.
        let reply = dispatch_action(serde_json::json!({
            "action": "voice.synthesize",
            "payload": {},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "invalid_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn streaming_actions_register_as_streaming() {
        use wylde_shared::ipc::actions::is_streaming_action;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        assert!(
            is_streaming_action("voice.transcribe_stream"),
            "voice.transcribe_stream must be a streaming action",
        );
        assert!(
            is_streaming_action("voice.synthesize_stream"),
            "voice.synthesize_stream must be a streaming action",
        );
        assert!(
            !is_streaming_action("voice.transcribe"),
            "unary voice.transcribe must NOT be a streaming action",
        );
        reset_for_tests();
    }

    #[tokio::test]
    async fn voice_transcribe_stream_dispatches_invalid_request_for_missing_audio() {
        use wylde_shared::ipc::actions::take_streaming_action;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let fut = take_streaming_action("voice.transcribe_stream", serde_json::json!({}), tx)
            .expect("streaming action resolves");
        fut.await;
        let chunk = rx.recv().await.expect("at least one chunk emitted");
        let err = chunk.expect_err("missing audio → stream-level error");
        assert_eq!(err.code, "invalid_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn voice_synthesize_stream_dispatches_invalid_request_for_missing_phonemes() {
        use wylde_shared::ipc::actions::take_streaming_action;
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let fut = take_streaming_action("voice.synthesize_stream", serde_json::json!({}), tx)
            .expect("streaming action resolves");
        fut.await;
        let chunk = rx.recv().await.expect("at least one chunk emitted");
        let err = chunk.expect_err("missing phonemes → stream-level error");
        assert_eq!(err.code, "invalid_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn voice_list_models_dispatches_cleanly() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "voice.list_models",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "voice.list_models should reply ok");
        assert!(reply.data["stt"]["models"].is_array());
        assert_eq!(reply.data["tts"]["model"]["voices"].as_array().unwrap().len(), 28);
        reset_for_tests();
    }
}
