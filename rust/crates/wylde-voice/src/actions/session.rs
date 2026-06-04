//! GUI-facing `voice.*` actions (Slice 11.E+).
//!
//! Eight verbs ported from `Voice/pipe.py`:
//!
//! * `voice.toggle` / `voice.start_session` — drive one full
//!   capture → STT → chat → TTS → play round-trip.
//! * `voice.end_session` — cancel the in-flight capture early.
//! * `voice.set_mode` / `voice.get_mode` — persistent push-to-talk vs
//!   always-on toggle.
//! * `voice.set_active_conversation` — GUI mirror push.
//! * `voice.get_status` — full state snapshot for the dashboard.
//! * `voice.check_wake_word_model` — does the harness have the wake-word
//!   bundle installed?
//! * `voice.pull_wake_word_model` — kick a Gateway model pull (returns a
//!   tracking `job_id`).
//! * `voice.subscribe_status` — long-poll cursor over the event ring.
//!
//! Wire envelopes match the Python pipe exactly so the GUI doesn't see
//! a behavioural change when `WYLDE_WYLDE_VOICE_IMPL` flips to `rust`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use wylde_shared::ipc::{IpcError, Reply};

use crate::actions::error::invalid_request;
use crate::config_persist::{ALL_BACKENDS, ALL_MODES, ALL_VAD_SENSITIVITIES};
use crate::orchestrator::{run_session, SessionInputs};
use crate::orchestrator_clients::{CpalPlayback, HarnessIpcClient, MicSessionCapture};
use crate::service_state::ServiceState;
use crate::wakeword::download::{spawn_pull_job, PullJobs, PullStatus};

use crate::model_registry_bridge as registry;

/// One-at-a-time guard for the orchestrator. The Python pipe used a
/// non-blocking lock so two simultaneous `voice.toggle` calls don't
/// fight over the mic — we keep the same shape.
fn session_lock() -> &'static AsyncMutex<()> {
    static LOCK: std::sync::OnceLock<AsyncMutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// Process-wide cancel handle for `voice.end_session`. The capture
/// adapter wired into the orchestrator owns this; we keep an
/// independent clone here so a stop call can fire even if the
/// orchestrator hasn't bound a capture yet.
fn cancel_handle() -> &'static std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> {
    static CH: std::sync::OnceLock<
        std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    > = std::sync::OnceLock::new();
    CH.get_or_init(|| std::sync::Mutex::new(None))
}

fn set_cancel_handle(h: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    if let Ok(mut slot) = cancel_handle().lock() {
        *slot = Some(h);
    }
}

fn fire_cancel() -> bool {
    let h = match cancel_handle().lock() {
        Ok(slot) => slot.clone(),
        Err(_) => return false,
    };
    match h {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

// ── voice.toggle / voice.start_session ──────────────────────────────────

pub async fn handle_voice_toggle(payload: Value) -> Reply {
    // Acquire the singleton session lock — non-blocking. Python's
    // `_session_lock.acquire(blocking=False)` shape; in tokio we
    // `try_lock`.
    let lock = session_lock();
    let guard = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            return Reply::err(IpcError::new(
                "busy",
                "a session is already in flight",
            ));
        }
    };

    let max_seconds = payload
        .get("max_seconds")
        .and_then(Value::as_f64)
        .map(|s| s as f32)
        .unwrap_or(30.0);
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let state = ServiceState::global();
    let active_id = state.active_conversation_id().await;

    let capture = MicSessionCapture::new();
    set_cancel_handle(capture.cancel_handle());
    let capture: std::sync::Arc<MicSessionCapture> = std::sync::Arc::new(capture);
    let playback: std::sync::Arc<CpalPlayback> = std::sync::Arc::new(CpalPlayback);
    let harness: std::sync::Arc<HarnessIpcClient> = std::sync::Arc::new(HarnessIpcClient::new());

    // Begin/end the session against ServiceState in parallel with the
    // orchestrator's own internal state machine. We mirror Python's
    // shape: the service-level state is the source the GUI subscribes
    // to; the orchestrator's flow is the source of timings.
    let session_id_seed = uuid::Uuid::new_v4().simple().to_string();
    let session_id: String = session_id_seed.chars().take(12).collect();
    state
        .begin_session(active_id.clone(), session_id.clone())
        .await;

    let inputs = SessionInputs {
        active_conversation_id: &active_id,
        max_capture_seconds: max_seconds,
        model: model.as_deref(),
    };
    let result = run_session(
        std::sync::Arc::clone(&capture),
        std::sync::Arc::clone(&playback),
        std::sync::Arc::clone(&harness),
        inputs,
    )
    .await;

    // Tell ServiceState the round-trip is done. Use the orchestrator's
    // own session_id in the result envelope so the GUI sees the same
    // id throughout — replace the seeded one we showed at begin.
    state
        .end_session(
            result.transcript.clone(),
            result.response.clone(),
            result.error.clone(),
        )
        .await;

    // Drop the registered cancel handle when the round-trip finishes so
    // a follow-on `voice.end_session` with no active session is a no-op
    // rather than triggering a stale flag.
    if let Ok(mut slot) = cancel_handle().lock() {
        *slot = None;
    }
    drop(guard);

    let envelope = json!({
        "session_id": result.session_id,
        "conversation_id": result.conversation_id,
        "transcript": result.transcript,
        "response": result.response,
        "aborted": result.aborted,
        "error": result.error,
        "timings_ms": serde_json::to_value(&result.timings_ms).unwrap_or(Value::Null),
    });
    Reply::ok(envelope)
}

// ── voice.end_session ───────────────────────────────────────────────────

pub async fn handle_voice_end_session(_payload: Value) -> Reply {
    let state = ServiceState::global();
    let had_active = fire_cancel();
    let snap = state.snapshot().await;
    // Belt-and-braces: when there was nothing in flight, snap any stuck
    // intermediate state back to idle.
    if !had_active {
        state.force_idle().await;
    }
    Reply::ok(json!({
        "ok": true,
        "had_active_session": snap.get("active_session").map(|v| !v.is_null()).unwrap_or(false),
        "state": snap.get("state").cloned().unwrap_or(Value::Null),
    }))
}

// ── voice.set_mode / voice.get_mode ────────────────────────────────────

pub async fn handle_voice_set_mode(payload: Value) -> Reply {
    let mode = match payload.get("mode").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => {
            return Reply::err(invalid_request(format!(
                "mode must be one of {ALL_MODES:?}"
            )));
        }
    };
    let state = ServiceState::global();
    match state.set_mode(&mode).await {
        Ok(new_mode) => Reply::ok(json!({"mode": new_mode})),
        Err(_) => Reply::err(invalid_request(format!(
            "mode must be one of {ALL_MODES:?}"
        ))),
    }
}

pub async fn handle_voice_get_mode(_payload: Value) -> Reply {
    let state = ServiceState::global();
    Reply::ok(json!({"mode": state.get_mode().await}))
}

// ── voice.get_config / voice.set_config (Slice 6) ───────────────────────

/// Return the full persisted voice config (mode, push-to-talk hotkey,
/// STT backend preference, mic device, VAD sensitivity, wake-word
/// model + enabled). Backs the Settings → Voice panel's load.
pub async fn handle_voice_get_config(_payload: Value) -> Reply {
    let state = ServiceState::global();
    Reply::ok(state.get_config_value().await)
}

/// Merge a partial config patch into the persisted voice config. The
/// payload IS the patch — any subset of the config keys. Enum-shaped
/// keys are validated up front so a typo surfaces as a clear
/// `invalid_request` (rather than being silently snapped to a default
/// inside [`crate::config_persist::VoiceConfig::normalised`], which the
/// GUI couldn't distinguish from a successful write). Reply: the merged
/// config, same shape as `voice.get_config`.
pub async fn handle_voice_set_config(payload: Value) -> Reply {
    if let Some(m) = payload.get("mode").and_then(Value::as_str) {
        if !ALL_MODES.contains(&m) {
            return Reply::err(invalid_request(format!("mode must be one of {ALL_MODES:?}")));
        }
    }
    if let Some(b) = payload.get("stt_backend_pref").and_then(Value::as_str) {
        if !ALL_BACKENDS.contains(&b) {
            return Reply::err(invalid_request(format!(
                "stt_backend_pref must be one of {ALL_BACKENDS:?}"
            )));
        }
    }
    if let Some(s) = payload.get("vad_sensitivity").and_then(Value::as_str) {
        if !ALL_VAD_SENSITIVITIES.contains(&s) {
            return Reply::err(invalid_request(format!(
                "vad_sensitivity must be one of {ALL_VAD_SENSITIVITIES:?}"
            )));
        }
    }
    let state = ServiceState::global();
    Reply::ok(state.apply_config_patch(&payload).await)
}

// ── voice.set_active_conversation ─────────────────────────────────────

pub async fn handle_voice_set_active_conversation(payload: Value) -> Reply {
    let conv = match payload.get("conversation_id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => return Reply::err(invalid_request("conversation_id is required")),
    };
    let state = ServiceState::global();
    let new_id = state.set_active_conversation(conv).await;
    Reply::ok(json!({"conversation_id": new_id}))
}

// ── voice.get_status ──────────────────────────────────────────────────

pub async fn handle_voice_get_status(_payload: Value) -> Reply {
    let state = ServiceState::global();
    Reply::ok(state.snapshot().await)
}

// ── voice.check_wake_word_model ───────────────────────────────────────

pub async fn handle_voice_check_wake_word_model(payload: Value) -> Reply {
    let state = ServiceState::global();
    let configured = state.wake_word_model().await;
    let model_name = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(configured);
    let installed = registry::is_wakeword_installed(&model_name);
    state.set_wake_word_installed(installed).await;
    Reply::ok(json!({
        "installed": installed,
        "model": model_name,
    }))
}

// ── voice.pull_wake_word_model ────────────────────────────────────────

pub async fn handle_voice_pull_wake_word_model(payload: Value) -> Reply {
    let state = ServiceState::global();
    let configured = state.wake_word_model().await;
    let model_name = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(configured);
    let job_id = spawn_pull_job(model_name.clone());
    state.set_wake_word_pull_job(Some(job_id.clone())).await;
    Reply::ok(json!({
        "job_id": job_id,
        "model": model_name,
    }))
}

/// Out-of-band helper for the GUI — query the in-progress / done /
/// failed status of a previously-issued pull. Not part of the 8 task
/// verbs but kept here because it's the natural follow-on call for a
/// caller holding a `job_id`. Wired through service.rs so the GUI can
/// reach it without polling the slower `voice.check_wake_word_model`.
pub async fn handle_voice_wake_word_pull_status(payload: Value) -> Reply {
    let job_id = match payload.get("job_id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => return Reply::err(invalid_request("job_id is required")),
    };
    let jobs = PullJobs::global();
    match jobs.status(&job_id).await {
        Some(PullStatus::InProgress) => Reply::ok(json!({
            "job_id": job_id,
            "state": "in_progress",
        })),
        Some(PullStatus::Done { bundle_dir }) => Reply::ok(json!({
            "job_id": job_id,
            "state": "done",
            "bundle_dir": bundle_dir.display().to_string(),
        })),
        Some(PullStatus::Failed { error }) => Reply::ok(json!({
            "job_id": job_id,
            "state": "failed",
            "error": error,
        })),
        None => Reply::err(IpcError::new(
            "unknown_job",
            format!("no pull job tracked for id {job_id}"),
        )),
    }
}

// ── voice.subscribe_status ────────────────────────────────────────────

pub async fn handle_voice_subscribe_status(payload: Value) -> Reply {
    let cursor = payload
        .get("cursor")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let max_wait_ms = payload
        .get("max_wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5_000);
    let state = ServiceState::global();
    let value = state.poll_events(cursor, max_wait_ms).await;
    Reply::ok(value)
}

// ── Used by service.rs to release the captured Arc references on
//    shutdown. Nothing to clean up at the service-state level today
//    (it's a OnceLock); kept here so the surface is symmetric with the
//    other modules' `clear_*` helpers.
pub fn shutdown_session_state() {
    let _ = Arc::clone(&ServiceState::global());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_mode_returns_current() {
        // We don't fight the global singleton — just read the default.
        let r = handle_voice_get_mode(Value::Null).await;
        assert!(r.ok);
        let mode = r.data["mode"].as_str().unwrap();
        assert!(["push_to_talk", "always_on"].contains(&mode));
    }

    #[tokio::test]
    async fn set_mode_rejects_unknown() {
        let r = handle_voice_set_mode(json!({"mode": "supersonic"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn get_config_returns_full_block() {
        let r = handle_voice_get_config(Value::Null).await;
        assert!(r.ok);
        // Every Slice-6 key is present (values depend on the on-disk
        // config but the keys must exist for the panel to bind to them).
        for key in [
            "mode",
            "wake_word_model",
            "wake_word_enabled",
            "push_to_talk_hotkey",
            "stt_backend_pref",
            "vad_sensitivity",
        ] {
            assert!(r.data.get(key).is_some(), "missing key {key}");
        }
        assert!(r.data.as_object().unwrap().contains_key("input_device"));
    }

    #[tokio::test]
    async fn set_config_rejects_bad_backend() {
        let r = handle_voice_set_config(json!({ "stt_backend_pref": "quantum" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn set_config_rejects_bad_vad_sensitivity() {
        let r = handle_voice_set_config(json!({ "vad_sensitivity": "ludicrous" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn set_config_accepts_valid_patch() {
        let r = handle_voice_set_config(json!({
            "stt_backend_pref": "cpu",
            "vad_sensitivity": "low",
        }))
        .await;
        assert!(r.ok);
        assert_eq!(r.data["stt_backend_pref"], "cpu");
        assert_eq!(r.data["vad_sensitivity"], "low");
    }

    #[tokio::test]
    async fn set_active_conversation_requires_field() {
        let r = handle_voice_set_active_conversation(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn set_active_conversation_round_trips() {
        let r = handle_voice_set_active_conversation(json!({"conversation_id": "conv-77"})).await;
        assert!(r.ok);
        assert_eq!(r.data["conversation_id"], "conv-77");
        let snap = handle_voice_get_status(Value::Null).await;
        assert_eq!(snap.data["active_conversation_id"], "conv-77");
    }

    #[tokio::test]
    async fn check_wake_word_model_falls_back_to_configured() {
        let r = handle_voice_check_wake_word_model(json!({})).await;
        assert!(r.ok);
        assert!(r.data["model"].as_str().is_some());
        // Installed flag is a boolean — value depends on the dev box.
        assert!(r.data["installed"].is_boolean());
    }

    #[tokio::test]
    async fn pull_wake_word_model_returns_job_id() {
        // We don't await the spawned task — that uses curl which may not
        // be present in CI. The handler returns the job id immediately.
        let r = handle_voice_pull_wake_word_model(json!({"model": "openWakeWord/hey-jarvis"})).await;
        assert!(r.ok);
        let job_id = r.data["job_id"].as_str().unwrap();
        assert_eq!(job_id.len(), 12);
        assert_eq!(r.data["model"], "openWakeWord/hey-jarvis");
    }

    #[tokio::test]
    async fn wake_word_pull_status_requires_job_id() {
        let r = handle_voice_wake_word_pull_status(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }

    #[tokio::test]
    async fn wake_word_pull_status_unknown_job() {
        let r = handle_voice_wake_word_pull_status(json!({"job_id": "deadbeef"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "unknown_job");
    }

    #[tokio::test]
    async fn end_session_no_active_returns_no_op() {
        let r = handle_voice_end_session(Value::Null).await;
        assert!(r.ok);
        assert_eq!(r.data["ok"], true);
        // After force_idle the state is idle/error.
        let state = r.data["state"].as_str().unwrap();
        assert!(["idle", "error"].contains(&state));
    }

    #[tokio::test]
    async fn subscribe_status_returns_cursor() {
        let r = handle_voice_subscribe_status(json!({"cursor": 0, "max_wait_ms": 0})).await;
        assert!(r.ok);
        assert!(r.data["events"].is_array());
        assert!(r.data["next_cursor"].is_number());
    }
}
