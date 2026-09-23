//! Background memory scheduler — Rust port of
//! `Core/harness/memory/scheduler.py` (full-Rust cutover slice R2b).
//!
//! A single tokio task polls every `poll_interval_s` seconds and fires
//! [`crate::memory::reflection::reflect`] at separate cadences:
//!
//! * conversation reflection — fires when a conversation has been idle
//!   ≥ `conversation_idle_s` (default 10 min) AND hasn't been reflected
//!   since its last activity.
//! * long-term reflection — fires every `long_term_reflect_s`
//!   (default 24 h).
//!
//! Cadences are env-overridable with the SAME variables Python read:
//! `WYLDE_SCHED_POLL_S` (60), `WYLDE_SCHED_CONV_IDLE_S` (600),
//! `WYLDE_SCHED_LT_REFLECT_S` (86400).
//!
//! ## State file
//!
//! `<data_dir>/scheduler_state.json` — same path and JSON shape Python
//! wrote, so a cutover boot resumes from the Python scheduler's state
//! without replaying the backlog:
//!
//! ```json
//! {
//!   "long_term_reflected_at": 1749400000.5,
//!   "conversation_reflected_at": {"<conversation_id>": 1749400100.25}
//! }
//! ```
//!
//! (serde writes the two top-level keys alphabetically where Python
//! wrote insertion order — JSON-equivalent, and both loaders are
//! lenient about ordering.)
//!
//! State is saved once per tick, mirroring Python.
//!
//! ## Testability
//!
//! [`MemoryScheduler::tick`] runs one iteration with an injected clock
//! ([`MemoryScheduler::with_clock`]) and an injected reflect fn
//! ([`MemoryScheduler::with_reflect_fn`]), so cadence assertions are
//! deterministic and never sleep — the same split Python's
//! `MemoryScheduler.tick` kept for its tests. [`MemoryScheduler::start`]
//! wraps the loop in a spawned tokio task.
//!
//! ## Failure isolation
//!
//! Python wrapped every per-scope fire in `try/except` so one bad scope
//! couldn't kill the tick. The Rust cycles don't raise — every failure
//! mode (model down, store IO error) folds into a skipped
//! [`ReflectionResult`] inside `reflect` itself — so the equivalent
//! isolation is structural: each fire is awaited independently, its
//! outcome only logged, and the state save failure is logged without
//! aborting the loop.
//!
//! ## Boot wiring
//!
//! [`start_default`] is called from [`crate::service::install`], gated
//! on [`crate::config::Config::scheduler_enabled`]
//! (`WYLDE_HARNESS_SCHEDULER`, default ON). It always wires the
//! production [`OllamaReflectionChat`], which resolves the chat model
//! per call — so a model picked after boot is honoured without a
//! restart (Python built its chat_fn once at boot; the router resolved
//! models per call too, so behaviour matches). One deliberate
//! divergence: the loop sleeps one poll interval BEFORE its first tick
//! (Python ticked immediately), giving the service mesh time to settle
//! at boot and keeping short-lived test processes from ticking at all.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::memory::common::{data_dir, ensure_dir};
use crate::memory::conversations::store as conversations_store;
use crate::memory::long_term::reflection::{ReflectOptions, ReflectionChat, ReflectionResult};
use crate::memory::reflection::{reflect, OllamaReflectionChat};

/// State filename under `data_dir()` — same as Python's `STATE_PATH`.
pub const STATE_FILENAME: &str = "scheduler_state.json";

/// `<data_dir>/scheduler_state.json`.
pub fn state_path() -> PathBuf {
    data_dir().join(STATE_FILENAME)
}

// ── State persistence ──────────────────────────────────────────────────

/// Last-fired timestamps per scope. Mutated by the scheduler task only;
/// loaded once at construction, persisted after every tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedulerState {
    pub long_term_reflected_at: f64,
    pub conversation_reflected_at: HashMap<String, f64>,
}

impl SchedulerState {
    /// JSON shape — matches Python `SchedulerState.to_dict()`.
    pub fn to_value(&self) -> Value {
        let mut conv = Map::new();
        for (k, v) in &self.conversation_reflected_at {
            conv.insert(k.clone(), json!(v));
        }
        json!({
            "long_term_reflected_at": self.long_term_reflected_at,
            "conversation_reflected_at": Value::Object(conv),
        })
    }

    /// Lenient decode — missing / wrong-typed fields fold to defaults,
    /// matching Python `SchedulerState.from_dict`'s `or`-defaults.
    pub fn from_value(v: &Value) -> Self {
        let long_term_reflected_at = v
            .get("long_term_reflected_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let conversation_reflected_at = v
            .get("conversation_reflected_at")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
                    .collect()
            })
            .unwrap_or_default();
        SchedulerState {
            long_term_reflected_at,
            conversation_reflected_at,
        }
    }
}

/// Load state from `path`; any miss / parse failure reads as empty
/// (mirrors Python `_load_state`'s warn-and-default).
pub fn load_state(path: &Path) -> SchedulerState {
    if !path.exists() {
        return SchedulerState::default();
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: state unreadable, treating as empty: {e}");
            return SchedulerState::default();
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) if v.is_object() => SchedulerState::from_value(&v),
        Ok(_) => SchedulerState::default(),
        Err(e) => {
            tracing::warn!("scheduler: state unreadable, treating as empty: {e}");
            SchedulerState::default()
        }
    }
}

/// Persist state atomically (temp + rename), pretty-printed like
/// Python's `indent=2` dump.
pub fn save_state(state: &SchedulerState, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let body = serde_json::to_string_pretty(&state.to_value())
        .expect("scheduler state serialises to JSON");
    // Same tmp name Python's `path.with_suffix(".json.tmp")` produced.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ── Cadence config ─────────────────────────────────────────────────────

/// Per-scope cadence floors. `poll_interval_s` is the loop tick; the
/// others are minimum gaps between fires. Mirrors Python's
/// `CadenceConfig`.
#[derive(Debug, Clone)]
pub struct CadenceConfig {
    pub poll_interval_s: f64,
    pub conversation_idle_s: f64,
    pub long_term_reflect_s: f64,
}

impl CadenceConfig {
    /// Read the cadences from the same env vars Python read, with the
    /// same defaults. (Python read them once at import; reading per
    /// construction is the test-friendly equivalent.)
    pub fn from_env() -> Self {
        CadenceConfig {
            poll_interval_s: env_f64("WYLDE_SCHED_POLL_S", 60.0),
            conversation_idle_s: env_f64("WYLDE_SCHED_CONV_IDLE_S", 600.0),
            long_term_reflect_s: env_f64("WYLDE_SCHED_LT_REFLECT_S", 86400.0),
        }
    }
}

impl Default for CadenceConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(default)
}

// ── Scheduler ──────────────────────────────────────────────────────────

/// What one tick fired — Python returned `{"conversation": n,
/// "long_term": n}` for the same assertions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickCounts {
    pub conversation: usize,
    pub long_term: usize,
}

/// Injectable clock (epoch seconds).
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Future an injected reflect fn returns.
pub type ReflectFuture = Pin<Box<dyn Future<Output = ReflectionResult> + Send>>;

/// Test seam: replaces the real `reflect` dispatch per fire.
pub type ReflectFn = Arc<dyn Fn(String) -> ReflectFuture + Send + Sync>;

/// Tokio-task-backed scheduler. One instance is constructed at service
/// install ([`start_default`]).
pub struct MemoryScheduler {
    chat: Option<Arc<dyn ReflectionChat>>,
    cadence: CadenceConfig,
    clock: Clock,
    state_path: PathBuf,
    state: SchedulerState,
    reflect_fn: Option<ReflectFn>,
}

impl MemoryScheduler {
    /// Construct with env cadences, the system clock, and the default
    /// state path; state is loaded immediately (Python's `__init__`).
    pub fn new(chat: Option<Arc<dyn ReflectionChat>>) -> Self {
        let path = state_path();
        let state = load_state(&path);
        MemoryScheduler {
            chat,
            cadence: CadenceConfig::from_env(),
            clock: Arc::new(system_clock),
            state_path: path,
            state,
            reflect_fn: None,
        }
    }

    /// Override the cadences (tests / embedders).
    pub fn with_cadence(mut self, cadence: CadenceConfig) -> Self {
        self.cadence = cadence;
        self
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Re-target the state file; reloads state from the new path.
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state = load_state(&path);
        self.state_path = path;
        self
    }

    /// Inject a reflect fn (tests) — replaces the real dispatch.
    pub fn with_reflect_fn(mut self, f: ReflectFn) -> Self {
        self.reflect_fn = Some(f);
        self
    }

    /// Read-only snapshot of the state dict (Python's `.state()`).
    pub fn state(&self) -> Value {
        self.state.to_value()
    }

    /// Run one scheduler iteration. Returns fire counts so tests can
    /// assert which scopes fired this tick. Mirrors Python's `tick`:
    /// no chat fn (and no injected reflect fn) → zeros, no state write.
    pub async fn tick(&mut self) -> TickCounts {
        let mut counts = TickCounts::default();
        if self.chat.is_none() && self.reflect_fn.is_none() {
            return counts;
        }

        let now = (self.clock)();

        // Conversation reflection — idle window driven.
        counts.conversation = self.tick_conversations(now).await;

        // Long-term reflection — global, daily.
        counts.long_term = self.tick_long_term(now).await;

        // Persist state once per tick (cheap; small file).
        if let Err(e) = save_state(&self.state, &self.state_path) {
            tracing::error!("scheduler: state save failed: {e}");
        }
        counts
    }

    /// Fire conversation-scoped reflection on idle windows — or on
    /// working-memory **pressure** (memory plan M3b). A conversation
    /// fires when we haven't already reflected since its last activity
    /// AND either:
    ///
    /// * it has been idle ≥ `conversation_idle_s` (the original rule), or
    /// * its non-superseded working-memory count has crossed the
    ///   pressure threshold (`WYLDE_SCHED_WM_PRESSURE_N`, default 30) —
    ///   active sessions never idle, so without this WM grows
    ///   monotonically to the injection cap, which is exactly the
    ///   tier-7 pressure M3's render-time degrade defends against.
    ///   Consolidating mid-session shrinks the never-drop floor at the
    ///   source.
    ///
    /// A successful reflection supersedes the consumed entries (count
    /// drops, trigger disarms); a skipped one re-arms only after the
    /// next activity bumps `updated_at` past the reflect stamp.
    async fn tick_conversations(&mut self, now: f64) -> usize {
        let mut fired = 0;
        let pressure_n = wm_pressure_threshold();
        for meta in conversations_store::list_conversations() {
            let Some(cid) = meta
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let updated_at = meta
                .get("updated_at")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let last = self
                .state
                .conversation_reflected_at
                .get(cid)
                .copied()
                .unwrap_or(0.0);
            if last >= updated_at {
                continue;
            }
            let idle_fire = (now - updated_at) >= self.cadence.conversation_idle_s;
            // The count read is cheap but still a file read — only taken
            // when the idle rule didn't already decide to fire.
            let pressure_fire = !idle_fire
                && pressure_n
                    .map(|n| non_superseded_wm_count(cid) >= n)
                    .unwrap_or(false);
            if !idle_fire && !pressure_fire {
                continue;
            }
            if pressure_fire {
                tracing::info!(
                    "scheduler: WM pressure trigger for {cid} (≥ {} live entries)",
                    pressure_n.unwrap_or(0)
                );
            }
            let cid = cid.to_owned();
            self.fire_reflect(&format!("conversation:{cid}")).await;
            self.state.conversation_reflected_at.insert(cid, now);
            fired += 1;
        }
        fired
    }

    async fn tick_long_term(&mut self, now: f64) -> usize {
        if now - self.state.long_term_reflected_at < self.cadence.long_term_reflect_s {
            return 0;
        }
        self.fire_reflect("long_term").await;
        self.state.long_term_reflected_at = now;
        1
    }

    /// One fire. Failures never propagate — `reflect` folds every
    /// error into a skipped result, which is only logged here (the
    /// Rust analogue of Python's per-fire `try/except`).
    async fn fire_reflect(&self, scope: &str) {
        let result = match &self.reflect_fn {
            Some(f) => f(scope.to_owned()).await,
            None => reflect(scope, self.chat.as_deref(), ReflectOptions::default()).await,
        };
        tracing::info!(
            "scheduler: reflected {} (skipped={}, inputs={})",
            scope,
            result.skipped,
            result.inputs_considered
        );
    }

    /// Spawn the polling loop on the current tokio runtime. Returns
    /// `None` (not started) when no chat fn is wired — mirroring
    /// Python `start()`'s `False`. The returned handle's `abort()` is
    /// the stop mechanism (the harness lets background tasks die with
    /// the process, same as the turn-task pool).
    pub fn start(mut self) -> Option<tokio::task::JoinHandle<()>> {
        if self.chat.is_none() && self.reflect_fn.is_none() {
            tracing::info!(
                "scheduler: not started — no chat fn supplied (LLM not wired); \
                 reflection / curation will run only via direct calls."
            );
            return None;
        }
        let poll = self.cadence.poll_interval_s.max(1.0);
        tracing::info!(
            "scheduler: started (poll={:.0}s, lt_reflect={:.0}s)",
            poll,
            self.cadence.long_term_reflect_s
        );
        Some(tokio::spawn(async move {
            loop {
                // Sleep BEFORE each tick — see module docs (deliberate
                // divergence from Python's tick-first loop).
                tokio::time::sleep(std::time::Duration::from_secs_f64(poll)).await;
                let _ = self.tick().await;
            }
        }))
    }
}

fn system_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Default working-memory pressure threshold (M3b) — ~75% of the B8
/// injection cap (40 entries), so consolidation fires before the prompt
/// window fills.
const WM_PRESSURE_DEFAULT_N: usize = 30;

/// The M3b pressure threshold: `WYLDE_SCHED_WM_PRESSURE_N` (default
/// [`WM_PRESSURE_DEFAULT_N`]); `0` / `off` / `false` disables the
/// trigger entirely (the slice kill switch).
fn wm_pressure_threshold() -> Option<usize> {
    match std::env::var("WYLDE_SCHED_WM_PRESSURE_N") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            if matches!(t.as_str(), "off" | "false") {
                return None;
            }
            match t.parse::<usize>() {
                Ok(0) => None,
                Ok(n) => Some(n),
                Err(_) => Some(WM_PRESSURE_DEFAULT_N),
            }
        }
        Err(_) => Some(WM_PRESSURE_DEFAULT_N),
    }
}

/// Count of live (non-superseded) working-memory entries for a
/// conversation — the same population reflection would consume.
fn non_superseded_wm_count(conversation_id: &str) -> usize {
    crate::memory::short_term::store::get_working_memory(conversation_id)
        .map(|entries| {
            entries
                .iter()
                .filter(|e| {
                    e.get("superseded_by")
                        .and_then(Value::as_str)
                        .is_none_or(|s| s.is_empty())
                })
                .count()
        })
        .unwrap_or(0)
}

// ── Boot wiring ────────────────────────────────────────────────────────

static STARTED: AtomicBool = AtomicBool::new(false);

/// Start the production scheduler once per process. Called from
/// [`crate::service::install`]. Returns `true` when a scheduler is
/// running (now or from an earlier call). Gated on
/// `Config::scheduler_enabled` (`WYLDE_HARNESS_SCHEDULER`, default ON)
/// and on a tokio runtime being present.
pub fn start_default() -> bool {
    if !crate::config::Config::get().scheduler_enabled {
        tracing::info!("scheduler: disabled via WYLDE_HARNESS_SCHEDULER");
        return false;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!("scheduler: no async runtime at install; not started");
        return false;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return true;
    }
    let chat: Arc<dyn ReflectionChat> = Arc::new(OllamaReflectionChat);
    let started = MemoryScheduler::new(Some(chat)).start().is_some();
    if !started {
        STARTED.store(false, Ordering::SeqCst);
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;
    use std::sync::Mutex;

    /// Literal bytes the Python scheduler wrote
    /// (`json.dumps(state.to_dict(), indent=2)`) — pinned so the Rust
    /// loader keeps reading real on-disk state across the cutover.
    const PYTHON_STATE_FIXTURE: &str = "{\n  \"long_term_reflected_at\": 1749400000.5,\n  \"conversation_reflected_at\": {\n    \"conv-a\": 1749400100.25,\n    \"conv-b\": 0.0\n  }\n}";

    fn seed_conversation(cid: &str, updated_at: i64) {
        let dir = crate::memory::common::conversations_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let doc = json!({
            "id": cid,
            "title": "T",
            "created_at": updated_at,
            "updated_at": updated_at,
            "messages": [],
            "working_memory": [],
        });
        std::fs::write(
            dir.join(format!("{cid}.json")),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    fn fixed_clock(t: Arc<Mutex<f64>>) -> Clock {
        Arc::new(move || *t.lock().unwrap())
    }

    fn recording_reflect_fn(fired: Arc<Mutex<Vec<String>>>) -> ReflectFn {
        Arc::new(move |scope: String| {
            let fired = Arc::clone(&fired);
            Box::pin(async move {
                fired.lock().unwrap().push(scope.clone());
                ReflectionResult::skipped(scope, 0, "test fire")
            }) as ReflectFuture
        })
    }

    fn test_scheduler(
        clock_cell: &Arc<Mutex<f64>>,
        fired: &Arc<Mutex<Vec<String>>>,
    ) -> MemoryScheduler {
        MemoryScheduler::new(None)
            .with_cadence(CadenceConfig {
                poll_interval_s: 60.0,
                conversation_idle_s: 600.0,
                long_term_reflect_s: 86400.0,
            })
            .with_clock(fixed_clock(Arc::clone(clock_cell)))
            .with_reflect_fn(recording_reflect_fn(Arc::clone(fired)))
    }

    // ── state round-trip ─────────────────────────────────────────────

    #[test]
    fn loads_python_written_state_fixture() {
        let _env = TestEnv::new();
        let path = state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, PYTHON_STATE_FIXTURE).unwrap();
        let state = load_state(&path);
        assert_eq!(state.long_term_reflected_at, 1749400000.5);
        assert_eq!(state.conversation_reflected_at["conv-a"], 1749400100.25);
        assert_eq!(state.conversation_reflected_at["conv-b"], 0.0);
    }

    #[test]
    fn state_round_trips_through_save_and_load() {
        let _env = TestEnv::new();
        let path = state_path();
        let mut state = SchedulerState {
            long_term_reflected_at: 123.5,
            ..Default::default()
        };
        state
            .conversation_reflected_at
            .insert("c1".to_owned(), 456.25);
        save_state(&state, &path).unwrap();
        assert_eq!(load_state(&path), state);
        // Same top-level keys + nested map shape Python wrote.
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["long_term_reflected_at"], 123.5);
        assert_eq!(raw["conversation_reflected_at"]["c1"], 456.25);
        // No tmp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn missing_or_garbage_state_reads_as_empty() {
        let _env = TestEnv::new();
        let path = state_path();
        assert_eq!(load_state(&path), SchedulerState::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_state(&path), SchedulerState::default());
        std::fs::write(&path, "[1,2]").unwrap();
        assert_eq!(load_state(&path), SchedulerState::default());
    }

    // ── tick cadence ─────────────────────────────────────────────────

    #[tokio::test]
    async fn tick_without_chat_or_reflect_fn_is_a_noop() {
        let _env = TestEnv::new();
        let mut s = MemoryScheduler::new(None);
        let counts = s.tick().await;
        assert_eq!(counts, TickCounts::default());
        assert!(!state_path().exists(), "noop tick must not write state");
    }

    #[tokio::test]
    async fn conversation_fires_once_per_idle_window() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        // Idle for 700s (>= 600 gate) at the first tick.
        seed_conversation("conv-idle", (base - 700.0) as i64);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);

        // Tick 1: conversation fires; long_term fires too (state starts
        // at 0.0, so `now - 0 >= 86400` — same first-boot behaviour as
        // Python with a fresh state file).
        let counts = s.tick().await;
        assert_eq!(
            counts,
            TickCounts {
                conversation: 1,
                long_term: 1
            }
        );
        assert_eq!(
            *fired.lock().unwrap(),
            vec!["conversation:conv-idle".to_owned(), "long_term".to_owned()]
        );

        // Tick 2, one poll later: nothing re-fires — last_reflected is
        // now >= updated_at and the long-term gap hasn't elapsed.
        *clock_cell.lock().unwrap() = base + 60.0;
        let counts = s.tick().await;
        assert_eq!(counts, TickCounts::default());
        assert_eq!(fired.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn conversation_refires_after_new_activity_then_idle() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        seed_conversation("conv-busy", (base - 700.0) as i64);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        s.tick().await;
        assert_eq!(fired.lock().unwrap().len(), 2); // conv + first long_term

        // New activity at base+120, then idle past the window again.
        seed_conversation("conv-busy", (base + 120.0) as i64);
        *clock_cell.lock().unwrap() = base + 1000.0;
        let counts = s.tick().await;
        assert_eq!(
            counts.conversation, 1,
            "re-fires after fresh activity + idle"
        );
        assert_eq!(
            fired.lock().unwrap().last().unwrap(),
            "conversation:conv-busy"
        );
    }

    #[tokio::test]
    async fn conversation_does_not_fire_before_idle_window() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        // Active 100s ago — under the 600s idle gate.
        seed_conversation("conv-active", (base - 100.0) as i64);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 0);
        assert_eq!(*fired.lock().unwrap(), vec!["long_term".to_owned()]);
    }

    // ── M3b: working-memory pressure trigger ────────────────────────

    /// Seed a conversation with `live` non-superseded WM entries (plus
    /// one superseded entry, which must NOT count toward pressure).
    fn seed_conversation_with_wm(cid: &str, updated_at: i64, live: usize) {
        let dir = crate::memory::common::conversations_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let mut wm: Vec<Value> = (0..live)
            .map(|i| json!({"kind": "fact", "data": format!("entry {i}"), "at": updated_at}))
            .collect();
        wm.push(json!({"kind": "fact", "data": "consumed", "superseded_by": "ref-1"}));
        let doc = json!({
            "id": cid,
            "title": "T",
            "created_at": updated_at,
            "updated_at": updated_at,
            "messages": [],
            "working_memory": wm,
        });
        std::fs::write(
            dir.join(format!("{cid}.json")),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn wm_pressure_fires_reflection_for_an_active_conversation() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        // Active 10s ago — far under the idle gate — but 30 live WM
        // entries (the default threshold).
        seed_conversation_with_wm("conv-pressure", (base - 10.0) as i64, 30);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 1, "pressure fires despite activity");
        assert!(fired
            .lock()
            .unwrap()
            .contains(&"conversation:conv-pressure".to_owned()));

        // No re-fire until fresh activity bumps updated_at past the stamp.
        *clock_cell.lock().unwrap() = base + 60.0;
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 0, "stamped — no spin on a skip");
    }

    #[tokio::test]
    async fn wm_below_threshold_does_not_fire_pressure() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        // 29 live + 1 superseded: the superseded entry must not tip it.
        seed_conversation_with_wm("conv-under", (base - 10.0) as i64, 29);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 0);
    }

    #[tokio::test]
    async fn wm_pressure_threshold_is_env_tunable_and_disableable() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        seed_conversation_with_wm("conv-tune", (base - 10.0) as i64, 10);

        let prior = std::env::var_os("WYLDE_SCHED_WM_PRESSURE_N");
        std::env::set_var("WYLDE_SCHED_WM_PRESSURE_N", "10");
        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 1, "tuned-down threshold fires at 10");

        // Disabled: re-arm with fresh activity, then assert no fire.
        std::env::set_var("WYLDE_SCHED_WM_PRESSURE_N", "off");
        seed_conversation_with_wm("conv-tune", (base + 30.0) as i64, 10);
        *clock_cell.lock().unwrap() = base + 60.0;
        let counts = s.tick().await;
        assert_eq!(counts.conversation, 0, "off disables the trigger");
        match prior {
            Some(v) => std::env::set_var("WYLDE_SCHED_WM_PRESSURE_N", v),
            None => std::env::remove_var("WYLDE_SCHED_WM_PRESSURE_N"),
        }
    }

    #[tokio::test]
    async fn long_term_respects_its_cadence() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        // Pre-seed state: long-term last reflected at `base`.
        let pre = SchedulerState {
            long_term_reflected_at: base,
            conversation_reflected_at: HashMap::new(),
        };
        save_state(&pre, &state_path()).unwrap();

        let clock_cell = Arc::new(Mutex::new(base + 100.0));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);

        // 100s after the last fire: under the 86400s gap.
        assert_eq!(s.tick().await.long_term, 0);
        // One day + 1s later: fires.
        *clock_cell.lock().unwrap() = base + 86401.0;
        assert_eq!(s.tick().await.long_term, 1);
        assert_eq!(*fired.lock().unwrap(), vec!["long_term".to_owned()]);
        // And not again immediately.
        *clock_cell.lock().unwrap() = base + 86500.0;
        assert_eq!(s.tick().await.long_term, 0);
    }

    #[tokio::test]
    async fn tick_persists_state_each_time() {
        let _env = TestEnv::new();
        let base = 1_750_000_000.0_f64;
        seed_conversation("conv-save", (base - 700.0) as i64);

        let clock_cell = Arc::new(Mutex::new(base));
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s = test_scheduler(&clock_cell, &fired);
        s.tick().await;

        let on_disk = load_state(&state_path());
        assert_eq!(on_disk.long_term_reflected_at, base);
        assert_eq!(on_disk.conversation_reflected_at["conv-save"], base);

        // A fresh scheduler resumes from the persisted state — no
        // backlog replay (the Python restart semantics).
        let fired2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut s2 = test_scheduler(&clock_cell, &fired2);
        *clock_cell.lock().unwrap() = base + 60.0;
        let counts = s2.tick().await;
        assert_eq!(counts, TickCounts::default());
        assert!(fired2.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_returns_none_without_chat() {
        let _env = TestEnv::new();
        assert!(MemoryScheduler::new(None).start().is_none());
    }

    #[test]
    fn cadence_env_overrides_apply() {
        // Direct env_f64 checks — avoids mutating process env for the
        // full CadenceConfig (other tests run in parallel).
        assert_eq!(env_f64("WYLDE_SCHED_TEST_ABSENT_VAR", 60.0), 60.0);
        let c = CadenceConfig {
            poll_interval_s: 1.0,
            conversation_idle_s: 2.0,
            long_term_reflect_s: 3.0,
        };
        assert_eq!(c.poll_interval_s, 1.0);
        assert_eq!(c.conversation_idle_s, 2.0);
        assert_eq!(c.long_term_reflect_s, 3.0);
    }
}
