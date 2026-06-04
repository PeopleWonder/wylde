//! GUI-facing voice service state (Slice 11.E+ port of
//! `Voice/state.py::VoiceState`).
//!
//! Holds the runtime data the eight GUI-facing actions read and write:
//!
//! * the current mode (push-to-talk vs always-on) — persisted to disk;
//! * the active conversation id mirrored from the GUI;
//! * the active session and last error;
//! * a bounded ring of [`StatusEvent`]s so `voice.subscribe_status`
//!   can long-poll without missing events between calls;
//! * the wake-word installed flag.
//!
//! The Python predecessor used a single `RLock`; the Rust port uses a
//! `tokio::sync::Mutex` so handlers can `await` while holding state, and
//! a `Notify` to wake `subscribe_status` waiters when new events land.
//! Synchronous helpers exist on top for the non-async access patterns
//! (`snapshot`, `get_mode`).

use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};

use crate::config_persist::{
    load_config, save_config, VoiceConfig, ALL_MODES,
};

/// Lowercase wire strings; lined up with `Voice/state.py::STATE_*` and
/// [`crate::orchestrator::STATE_*`].
pub const STATE_IDLE: &str = "idle";
pub const STATE_LISTENING: &str = "listening";
pub const STATE_PROCESSING: &str = "processing";
pub const STATE_PLAYING: &str = "playing";
pub const STATE_ERROR: &str = "error";

/// Max events buffered in the long-poll ring. Mirrors Python's
/// `_max_events`. Drops oldest when full.
const MAX_EVENTS: usize = 256;

/// Caller-side max wait clamp for `voice.subscribe_status`. The Python
/// pipe layer caps at 25s so the connection isn't held indefinitely;
/// we keep the same number.
pub const MAX_SUBSCRIBE_WAIT_MS: u64 = 25_000;

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub started_at: f64,
    pub completed_at: Option<f64>,
    pub conversation_id: String,
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl Session {
    fn to_value(&self) -> Value {
        json!({
            "session_id": self.id,
            "started_at": self.started_at,
            "completed_at": self.completed_at,
            "conversation_id": self.conversation_id,
            "transcript": self.transcript,
            "response": self.response,
            "error": self.error,
        })
    }
}

/// One emission for the status long-poll stream. Wire shape matches
/// Python's `StatusEvent.to_dict` exactly: `type`, `at`, and the
/// per-event `data` keys flattened to top-level.
#[derive(Debug, Clone)]
pub struct StatusEvent {
    pub kind: String,
    pub at: f64,
    pub data: Value,
}

impl StatusEvent {
    fn to_value(&self) -> Value {
        let mut base = json!({
            "type": self.kind,
            "at": self.at,
        });
        if let Some(map) = self.data.as_object() {
            let target = base.as_object_mut().expect("freshly built map");
            for (k, v) in map {
                target.insert(k.clone(), v.clone());
            }
        }
        base
    }
}

/// Process-wide singleton.
pub struct ServiceState {
    inner: Mutex<Inner>,
    notify: Notify,
}

struct Inner {
    config: VoiceConfig,
    state: String,
    last_error: Option<String>,
    active_conversation_id: String,
    active_session: Option<Session>,
    events: Vec<StatusEvent>,
    wake_word_installed: bool,
    wake_word_pull_job: Option<String>,
}

impl ServiceState {
    fn new(config: VoiceConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                config,
                state: STATE_IDLE.to_owned(),
                last_error: None,
                active_conversation_id: String::new(),
                active_session: None,
                events: Vec::new(),
                wake_word_installed: false,
                wake_word_pull_job: None,
            }),
            notify: Notify::new(),
        }
    }

    /// Acquire the process-wide singleton. First call loads from disk.
    pub fn global() -> Arc<ServiceState> {
        static SINGLETON: std::sync::OnceLock<Arc<ServiceState>> = std::sync::OnceLock::new();
        SINGLETON
            .get_or_init(|| Arc::new(ServiceState::new(load_config())))
            .clone()
    }

    /// Test seam — drop the singleton and reload from disk on next use.
    /// Pairs with `reset_for_tests` in [`crate::state`].
    pub fn replace_for_tests(state: ServiceState) {
        // We can't actually reset a OnceLock, so tests that need
        // isolation use `ServiceState::new` directly via the in-tree
        // helpers below. Provided here for symmetry with the Python
        // `install_test_doubles` seam.
        let _ = state;
    }

    /// Snapshot for `voice.get_status`. Mirrors the Python `snapshot()`
    /// output keys exactly.
    pub async fn snapshot(&self) -> Value {
        let g = self.inner.lock().await;
        json!({
            "state": g.state,
            "mode": g.config.mode,
            "listening": g.state == STATE_LISTENING,
            "last_error": g.last_error,
            "active_session": g.active_session.as_ref().map(Session::to_value),
            "active_conversation_id": g.active_conversation_id,
            "wake_word_installed": g.wake_word_installed,
            "wake_word_model": g.config.wake_word_model,
        })
    }

    pub async fn get_mode(&self) -> String {
        self.inner.lock().await.config.mode.clone()
    }

    /// Switch mode, persist, emit a `state` event so subscribers see the
    /// flip. Returns the new mode value.
    pub async fn set_mode(&self, mode: &str) -> Result<String, &'static str> {
        if !ALL_MODES.contains(&mode) {
            return Err("unknown mode");
        }
        let payload;
        {
            let mut g = self.inner.lock().await;
            g.config.mode = mode.to_owned();
            // Persist outside the lock would race with a concurrent
            // set_mode; keep it inside. The file is small.
            if let Err(e) = save_config(&g.config) {
                tracing::warn!("wylde-voice: save_config failed: {e}");
            }
            payload = json!({"state": g.state.clone(), "mode": g.config.mode.clone()});
        }
        self.emit("state", payload).await;
        Ok(mode.to_owned())
    }

    /// Full persisted config as a JSON object — the `voice.get_config`
    /// reply the Settings → Voice panel reads on load (Slice 6).
    pub async fn get_config_value(&self) -> Value {
        let g = self.inner.lock().await;
        serde_json::to_value(&g.config).unwrap_or(Value::Null)
    }

    /// Merge a partial config patch, persist atomically, emit a `config`
    /// event so subscribers reconcile, and return the merged config as
    /// JSON. The single write path behind `voice.set_config` (Slice 6).
    ///
    /// Validation lives in [`VoiceConfig::with_patch`] →
    /// [`VoiceConfig::normalised`]: an out-of-range value in the patch is
    /// snapped to its safe default rather than rejected, so a malformed
    /// patch can never wedge the on-disk config. Callers that want a
    /// hard "bad value" error (e.g. to surface it in the GUI) pre-check
    /// the enum at the action layer.
    pub async fn apply_config_patch(&self, patch: &Value) -> Value {
        let merged;
        {
            let mut g = self.inner.lock().await;
            let updated = g.config.clone().with_patch(patch);
            g.config = updated.clone();
            // Persist inside the lock so a concurrent patch can't
            // interleave a stale write (mirrors set_mode).
            if let Err(e) = save_config(&g.config) {
                tracing::warn!("wylde-voice: save_config failed: {e}");
            }
            merged = updated;
        }
        let value = serde_json::to_value(&merged).unwrap_or(Value::Null);
        self.emit("config", value.clone()).await;
        value
    }

    /// Mirror the GUI's active-conversation id.
    pub async fn set_active_conversation(&self, conversation_id: String) -> String {
        let mut g = self.inner.lock().await;
        g.active_conversation_id = conversation_id;
        g.active_conversation_id.clone()
    }

    pub async fn active_conversation_id(&self) -> String {
        self.inner.lock().await.active_conversation_id.clone()
    }

    /// Begin a new session, transitioning the state to LISTENING and
    /// emitting a `session_started` event. The caller — the orchestrator
    /// wrapper around `voice.toggle` — owns the session for the duration
    /// of the round-trip.
    pub async fn begin_session(&self, conversation_id: String, session_id: String) -> Session {
        let started = current_unix_seconds();
        let sess = Session {
            id: session_id,
            started_at: started,
            completed_at: None,
            conversation_id,
            transcript: String::new(),
            response: String::new(),
            error: None,
        };
        {
            let mut g = self.inner.lock().await;
            g.active_session = Some(sess.clone());
            g.state = STATE_LISTENING.to_owned();
        }
        self.emit("session_started", sess.to_value()).await;
        sess
    }

    /// Finalise the active session. Records transcript/response/error,
    /// transitions state to IDLE (or ERROR if `error` is `Some`), emits
    /// `session_ended`. Returns the closed session for the caller's
    /// result envelope.
    pub async fn end_session(
        &self,
        transcript: String,
        response: String,
        error: Option<String>,
    ) -> Option<Session> {
        let finished;
        {
            let mut g = self.inner.lock().await;
            let mut sess = g.active_session.take()?;
            sess.transcript = transcript;
            sess.response = response;
            sess.error = error.clone();
            sess.completed_at = Some(current_unix_seconds());
            g.state = if error.is_some() {
                STATE_ERROR.to_owned()
            } else {
                STATE_IDLE.to_owned()
            };
            g.last_error = error;
            finished = sess.clone();
            // Keep the closed session out of `active_session` so the
            // GUI's get_status reads as idle.
            g.active_session = None;
        }
        self.emit("session_ended", finished.to_value()).await;
        Some(finished)
    }

    pub async fn set_state(&self, new_state: &str) {
        {
            let mut g = self.inner.lock().await;
            g.state = new_state.to_owned();
        }
        self.emit("state", json!({"state": new_state})).await;
    }

    /// Force the state back to IDLE without emitting a session_ended
    /// event. Belt-and-braces; used by `voice.end_session` when there
    /// was no active session to close.
    pub async fn force_idle(&self) {
        let needs_flip;
        {
            let mut g = self.inner.lock().await;
            needs_flip = !matches!(g.state.as_str(), STATE_IDLE | STATE_ERROR);
            if needs_flip {
                g.state = STATE_IDLE.to_owned();
            }
        }
        if needs_flip {
            self.emit("state", json!({"state": STATE_IDLE})).await;
        }
    }

    pub async fn set_wake_word_installed(&self, installed: bool) {
        let model;
        {
            let mut g = self.inner.lock().await;
            g.wake_word_installed = installed;
            model = g.config.wake_word_model.clone();
        }
        self.emit(
            "wake_word_status",
            json!({"installed": installed, "model": model}),
        )
        .await;
    }

    pub async fn wake_word_model(&self) -> String {
        self.inner.lock().await.config.wake_word_model.clone()
    }

    pub async fn set_wake_word_pull_job(&self, job_id: Option<String>) {
        self.inner.lock().await.wake_word_pull_job = job_id;
    }

    /// Pull the next batch of events for a long-poll subscriber.
    /// `cursor < 0` or beyond the ring snaps to the current head so a
    /// stale subscriber resynchronises silently — same behaviour as
    /// Python's `poll_events`.
    pub async fn poll_events(&self, cursor: i64, max_wait_ms: u64) -> Value {
        let max_wait_ms = max_wait_ms.min(MAX_SUBSCRIBE_WAIT_MS);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(max_wait_ms);

        loop {
            let now_len;
            let new_events: Vec<Value>;
            let next_cursor: usize;
            {
                let g = self.inner.lock().await;
                now_len = g.events.len();
                let safe_cursor: usize = if cursor < 0 || (cursor as usize) > now_len {
                    now_len
                } else {
                    cursor as usize
                };
                if safe_cursor < now_len {
                    new_events = g
                        .events
                        .iter()
                        .skip(safe_cursor)
                        .map(StatusEvent::to_value)
                        .collect();
                    next_cursor = now_len;
                    return json!({
                        "events": new_events,
                        "next_cursor": next_cursor,
                    });
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return json!({
                    "events": Vec::<Value>::new(),
                    "next_cursor": now_len,
                });
            }
            // Wait for a notify wake or the deadline, whichever first.
            let _ = tokio::time::timeout(remaining, self.notify.notified()).await;
        }
    }

    async fn emit(&self, kind: &str, data: Value) {
        {
            let mut g = self.inner.lock().await;
            g.events.push(StatusEvent {
                kind: kind.to_owned(),
                at: current_unix_seconds(),
                data,
            });
            if g.events.len() > MAX_EVENTS {
                let overflow = g.events.len() - MAX_EVENTS;
                g.events.drain(..overflow);
            }
        }
        self.notify.notify_waiters();
    }

    /// Test-only — drop every event so a fresh test starts cold. Avoids
    /// the long-running ring contaminating sibling tests.
    #[cfg(test)]
    pub async fn clear_events_for_tests(&self) {
        self.inner.lock().await.events.clear();
    }
}

fn current_unix_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state() -> Arc<ServiceState> {
        Arc::new(ServiceState::new(VoiceConfig::default()))
    }

    #[tokio::test]
    async fn snapshot_starts_idle() {
        let s = fresh_state();
        let v = s.snapshot().await;
        assert_eq!(v["state"], STATE_IDLE);
        assert_eq!(v["mode"], "push_to_talk");
        assert_eq!(v["listening"], false);
        assert!(v["active_session"].is_null());
        assert_eq!(v["active_conversation_id"], "");
        assert_eq!(v["wake_word_installed"], false);
    }

    #[tokio::test]
    async fn set_mode_rejects_unknown() {
        let s = fresh_state();
        let err = s.set_mode("turbo").await;
        assert!(err.is_err());
        // Mode unchanged.
        assert_eq!(s.get_mode().await, "push_to_talk");
    }

    #[tokio::test]
    async fn set_mode_accepts_known_and_persists_to_state() {
        let s = fresh_state();
        // No on-disk save in this test path (config_path() lives in the
        // user's tree); we just check the in-memory effect.
        let new = s.set_mode("always_on").await.unwrap();
        assert_eq!(new, "always_on");
        assert_eq!(s.get_mode().await, "always_on");
        // Emitted a state event.
        let v = s.poll_events(0, 0).await;
        let evs = v["events"].as_array().unwrap();
        assert!(evs.iter().any(|e| e["type"] == "state" && e["mode"] == "always_on"));
    }

    #[tokio::test]
    async fn active_conversation_round_trips() {
        let s = fresh_state();
        assert_eq!(s.active_conversation_id().await, "");
        s.set_active_conversation("conv-42".into()).await;
        assert_eq!(s.active_conversation_id().await, "conv-42");
        let v = s.snapshot().await;
        assert_eq!(v["active_conversation_id"], "conv-42");
    }

    #[tokio::test]
    async fn get_config_value_reports_full_block() {
        let s = fresh_state();
        let v = s.get_config_value().await;
        assert_eq!(v["mode"], "push_to_talk");
        assert_eq!(v["stt_backend_pref"], "auto");
        assert_eq!(v["vad_sensitivity"], "medium");
        assert_eq!(v["wake_word_enabled"], false);
        assert!(v["input_device"].is_null());
        assert_eq!(v["push_to_talk_hotkey"], "Ctrl+Space");
    }

    #[tokio::test]
    async fn apply_config_patch_merges_and_emits() {
        let s = fresh_state();
        let merged = s
            .apply_config_patch(&json!({
                "stt_backend_pref": "npu",
                "vad_sensitivity": "high",
                "input_device": "USB Mic",
            }))
            .await;
        assert_eq!(merged["stt_backend_pref"], "npu");
        assert_eq!(merged["vad_sensitivity"], "high");
        assert_eq!(merged["input_device"], "USB Mic");
        // Untouched key keeps its default.
        assert_eq!(merged["mode"], "push_to_talk");
        // In-memory config reflects the merge.
        let after = s.get_config_value().await;
        assert_eq!(after["stt_backend_pref"], "npu");
        // A `config` event was emitted for subscribers.
        let evs = s.poll_events(0, 0).await;
        assert!(evs["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "config" && e["stt_backend_pref"] == "npu"));
    }

    #[tokio::test]
    async fn apply_config_patch_snaps_bad_enum_to_default() {
        let s = fresh_state();
        let merged = s
            .apply_config_patch(&json!({ "stt_backend_pref": "tpu" }))
            .await;
        assert_eq!(merged["stt_backend_pref"], "auto");
    }

    #[tokio::test]
    async fn begin_and_end_session_emit_events() {
        let s = fresh_state();
        s.begin_session("conv-1".into(), "sess-abc".into()).await;
        let v = s.snapshot().await;
        assert_eq!(v["state"], STATE_LISTENING);
        assert_eq!(v["listening"], true);
        let closed = s
            .end_session("hello".into(), "hi".into(), None)
            .await
            .unwrap();
        assert_eq!(closed.id, "sess-abc");
        assert_eq!(closed.transcript, "hello");
        assert_eq!(closed.response, "hi");
        let v = s.snapshot().await;
        assert_eq!(v["state"], STATE_IDLE);
        assert!(v["active_session"].is_null());
        // session_started + state(listening) + session_ended + state(idle?).
        let evs = s.poll_events(0, 0).await;
        let kinds: Vec<&str> = evs["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"session_started"));
        assert!(kinds.contains(&"session_ended"));
    }

    #[tokio::test]
    async fn end_session_with_error_lands_state_error() {
        let s = fresh_state();
        s.begin_session("conv-1".into(), "sess-x".into()).await;
        s.end_session(String::new(), String::new(), Some("no_audio".into()))
            .await;
        let v = s.snapshot().await;
        assert_eq!(v["state"], STATE_ERROR);
        assert_eq!(v["last_error"], "no_audio");
    }

    #[tokio::test]
    async fn poll_events_respects_cursor() {
        let s = fresh_state();
        s.set_mode("always_on").await.unwrap(); // 1 event
        s.set_active_conversation("c".into()).await; // 0 events (no emit)
        s.set_state(STATE_PROCESSING).await; // 1 event
        let first = s.poll_events(0, 0).await;
        let next_cursor = first["next_cursor"].as_u64().unwrap();
        assert!(first["events"].as_array().unwrap().len() >= 2);
        // From the new cursor, no events until new emissions.
        let none = s.poll_events(next_cursor as i64, 0).await;
        assert!(none["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn poll_events_clamps_stale_cursor() {
        let s = fresh_state();
        s.set_mode("always_on").await.unwrap();
        // Way past the end — the response should snap us forward.
        let r = s.poll_events(9_999, 0).await;
        assert!(r["events"].as_array().unwrap().is_empty());
        assert_eq!(r["next_cursor"], 1);
        // Negative cursor → also clamps.
        let r2 = s.poll_events(-1, 0).await;
        assert!(r2["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn poll_events_wakes_on_emit() {
        let s = fresh_state();
        let cloned = Arc::clone(&s);
        // Subscribe with a 200 ms timeout, then emit from a spawned task.
        let waiter = tokio::spawn(async move { cloned.poll_events(0, 1_000).await });
        // Yield so the waiter parks.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        s.set_mode("always_on").await.unwrap();
        let v = waiter.await.unwrap();
        let evs = v["events"].as_array().unwrap();
        assert!(!evs.is_empty(), "subscriber should have seen the state event");
    }

    #[tokio::test]
    async fn wake_word_status_emits_event() {
        let s = fresh_state();
        s.set_wake_word_installed(true).await;
        let v = s.poll_events(0, 0).await;
        let evs = v["events"].as_array().unwrap();
        let found = evs.iter().find(|e| e["type"] == "wake_word_status").unwrap();
        assert_eq!(found["installed"], true);
        assert_eq!(found["model"], DEFAULT_WAKE_WORD_MODEL_VALUE);
        // snapshot also reflects it.
        let snap = s.snapshot().await;
        assert_eq!(snap["wake_word_installed"], true);
    }

    #[tokio::test]
    async fn event_ring_caps_at_max_events() {
        let s = fresh_state();
        for _ in 0..(MAX_EVENTS + 64) {
            s.set_state(STATE_PROCESSING).await;
        }
        let v = s.poll_events(0, 0).await;
        let count = v["events"].as_array().unwrap().len();
        assert!(count <= MAX_EVENTS);
    }

    const DEFAULT_WAKE_WORD_MODEL_VALUE: &str =
        crate::config_persist::DEFAULT_WAKE_WORD_MODEL;
}
