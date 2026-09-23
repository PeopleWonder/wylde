//! File watcher → per-file delta-upsert (Slice I).
//!
//! Keeps a workspace's code graph + vector index fresh as the user edits: a
//! `notify` watch on the **active** workspace folder feeds raw filesystem
//! events through a [`debouncer`] (coalescing an editor's save-burst into one
//! change), then dispatches each settled change to the per-file delta path
//! ([`crate::rag::indexer::delta`]). Without it the graph staled on every edit
//! after the last full ingest.
//!
//! ## One watcher, the active workspace only (design decision)
//!
//! Per the MRU model exactly one workspace is active at a time, and a watch is
//! a live OS handle + a background task + a tree of inotify/RDCW registrations.
//! Watching every recently-active workspace would multiply that cost for
//! folders the user isn't touching, and a non-active workspace's graph going
//! briefly stale is harmless — it's re-walked on activation (`set_active`
//! already delta-reindexes). So the watcher follows the active pointer:
//! activating a workspace tears down the previous watch and starts a fresh one;
//! deactivating/deleting it stops the watch. Simpler, and the cost tracks what
//! the user is actually working on.
//!
//! ## Module layout (Build Order §2)
//!
//!   * [`notify`] — `notify`-crate integration + event→change translation.
//!   * [`debouncer`] — the per-path quiet-window coalescer (pure, clock-injected).
//!   * this file — the public lifecycle (`on_active_changed` / `stop` /
//!     `status` / `pause` / `resume`), the async debounce-and-dispatch loop,
//!     and the `delta_upsert_complete` observer event.
//!
//! ## Enablement
//!
//! The auto-start hooks are gated behind [`enable`] so they only fire in the
//! live service (`main.rs` calls `enable()` at boot). Unit tests that drive the
//! registry verbs therefore never spawn a real watcher; the loop itself is
//! tested directly with a mock dispatcher.

pub mod debouncer;
pub mod notify;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::rag::indexer::{self, delta};
use crate::registry::{self, WorkspaceDefinition};

use debouncer::{ChangeKind, Debouncer};
use notify::RawChange;

/// Default debounce quiet-window. Override with
/// `WYLDE_WORKSPACES_WATCH_DEBOUNCE_MS`.
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;

/// The observer event name emitted after each settled delta. Future phases
/// (the graph panel refresh) subscribe via [`subscribe`]; today it drives the
/// info log + the in-process broadcast.
pub const DELTA_UPSERT_COMPLETE_EVENT: &str = "workspaces.events.delta_upsert_complete";

/// Observability snapshot returned by `workspaces.watcher.status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WatcherStatus {
    /// The workspace currently watched, or `None` when no watcher is running.
    pub active_workspace: Option<String>,
    /// Approximate count of indexed files in the watched workspace (refreshed
    /// each time a debounced batch is ingested).
    pub files_watched: u32,
    /// Epoch seconds of the most recent filesystem event seen, or `None`.
    pub last_event_at: Option<f64>,
    /// True while the watch is paused (events dropped; resume re-walks).
    pub paused: bool,
}

impl WatcherStatus {
    /// As the IPC reply `Value`.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The completion event broadcast after each settled delta.
#[derive(Clone, Debug)]
pub struct DeltaEvent {
    pub workspace_id: String,
    pub path: String,
    /// `"upsert"` or `"remove"`.
    pub action: &'static str,
    pub graph_chunk_nodes: u32,
    /// Watcher-to-graph processing time for this delta, in milliseconds —
    /// excludes the debounce quiet-window (the deliberate coalescing wait).
    /// This is the number the <500ms ingest budget is measured against.
    pub took_ms: f64,
}

/// Control messages the lifecycle sends into a running loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Pause,
    Resume,
    Shutdown,
}

// ── Observer event bus ──────────────────────────────────────────────────────

/// Channel depth for the delta-event bus. A lagging subscriber loses the
/// oldest events rather than stalling the watcher.
const EVENT_BUS_CAPACITY: usize = 128;

/// The process-wide bus the *live service* publishes on. There is one watcher
/// per process (see [`active`]), so one bus is the right shape in production —
/// panels subscribe via [`subscribe`] without having to be handed a sender.
///
/// It is deliberately **not** what [`run_loop`] reaches for. The loop is
/// handed its sender ([`start_for`] passes a clone of this one), so a test can
/// give each loop a private channel. Before #246 the loop published straight
/// to this global: `cargo test` runs a binary's tests on parallel threads, so
/// sibling watcher tests' events landed in each other's receivers and
/// `delta_event_is_broadcast_on_dispatch` failed ~17% of the time (the #83
/// self-collision class). Injection removes the hazard by construction rather
/// than by a convention each new test has to remember.
fn event_bus() -> &'static broadcast::Sender<DeltaEvent> {
    static BUS: OnceLock<broadcast::Sender<DeltaEvent>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(EVENT_BUS_CAPACITY).0)
}

/// Subscribe to [`DeltaEvent`]s (the `delta_upsert_complete` stream). A
/// lagging/absent subscriber never blocks the watcher.
pub fn subscribe() -> broadcast::Receiver<DeltaEvent> {
    event_bus().subscribe()
}

// ── Dispatcher abstraction (so the loop is testable offline) ────────────────

/// What the loop calls to apply a settled change. The production impl drives
/// the real per-file delta path; tests inject a recording mock so the
/// debounce-and-dispatch orchestration is verified without notify or Neo4j.
trait DeltaDispatcher: Send + Sync + 'static {
    /// The workspace these deltas belong to (for the completion event).
    fn workspace_id(&self) -> String;
    /// Current indexed-file count, for the status snapshot.
    fn files_watched(&self) -> u32;
    /// Apply one settled change.
    fn dispatch(
        &self,
        path: PathBuf,
        kind: ChangeKind,
    ) -> impl std::future::Future<Output = delta::DeltaOutcome> + Send;
    /// Re-walk the whole workspace (a resume catches up on what it missed).
    fn catch_up(&self) -> impl std::future::Future<Output = ()> + Send;
}

/// Production dispatcher: per-file delta + full re-walk on resume.
struct ServiceDispatcher {
    def: WorkspaceDefinition,
}

impl ServiceDispatcher {
    fn new(def: WorkspaceDefinition) -> Self {
        Self { def }
    }
}

impl DeltaDispatcher for ServiceDispatcher {
    fn workspace_id(&self) -> String {
        self.def.id.clone()
    }

    fn files_watched(&self) -> u32 {
        // Distinct indexed file paths — the honest "what's being kept fresh"
        // count. A small JSONL read, done once per debounced batch.
        let mut seen = std::collections::HashSet::new();
        for c in indexer::store::load_chunks(&self.def.id) {
            seen.insert(c.path);
        }
        seen.len() as u32
    }

    fn dispatch(
        &self,
        path: PathBuf,
        kind: ChangeKind,
    ) -> impl std::future::Future<Output = delta::DeltaOutcome> + Send {
        let def = self.def.clone();
        let path = path.to_string_lossy().into_owned();
        async move {
            match kind {
                ChangeKind::Upsert => delta::upsert_file(&def, &path).await,
                ChangeKind::Remove => delta::remove_file(&def, &path).await,
            }
        }
    }

    fn catch_up(&self) -> impl std::future::Future<Output = ()> + Send {
        // Re-fetch the latest def (toggles may have changed while paused).
        let id = self.def.id.clone();
        async move {
            if let Some(def) = registry::get(&id) {
                if def.rag_enabled && !def.folder.trim().is_empty() {
                    let _ = indexer::reindex(&def).await;
                }
            }
        }
    }
}

// ── The debounce + dispatch loop ────────────────────────────────────────────

/// Drive one watch: coalesce raw events, dispatch each settled change, and
/// publish the completion event on `deltas_tx`.
///
/// `deltas_tx` is injected rather than looked up globally so each loop owns
/// its own event stream — the live service hands it a clone of [`event_bus`],
/// tests hand it a private channel. See [`event_bus`] for why (#246).
async fn run_loop<D: DeltaDispatcher>(
    mut events_rx: UnboundedReceiver<RawChange>,
    mut control_rx: UnboundedReceiver<Control>,
    dispatcher: D,
    status: Arc<Mutex<WatcherStatus>>,
    window: Duration,
    deltas_tx: broadcast::Sender<DeltaEvent>,
) {
    let mut deb = Debouncer::new(window);
    let mut paused = false;

    loop {
        // Sleep until the earliest pending deadline; if nothing's pending (or
        // we're paused) park on a long sleep that any event/control wakes.
        let deadline = if paused { None } else { deb.next_deadline() };
        let sleep = match deadline {
            Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)),
            None => tokio::time::sleep(Duration::from_secs(3600)),
        };
        tokio::pin!(sleep);

        tokio::select! {
            biased;

            ctrl = control_rx.recv() => match ctrl {
                None | Some(Control::Shutdown) => break,
                Some(Control::Pause) => {
                    paused = true;
                    deb.clear();
                    set_paused(&status, true);
                    tracing::info!("workspaces.watcher: paused ({})", dispatcher.workspace_id());
                }
                Some(Control::Resume) => {
                    paused = false;
                    deb.clear();
                    set_paused(&status, false);
                    tracing::info!(
                        "workspaces.watcher: resumed ({}) — re-walking to catch up",
                        dispatcher.workspace_id()
                    );
                    dispatcher.catch_up().await;
                    set_files_watched(&status, dispatcher.files_watched());
                }
            },

            recv = events_rx.recv() => match recv {
                None => break, // notify watcher dropped → channel closed
                Some((path, kind)) => {
                    note_event(&status);
                    if !paused {
                        deb.record(path, kind, Instant::now());
                    }
                }
            },

            _ = &mut sleep => {
                if paused {
                    continue;
                }
                let due = deb.drain_due(Instant::now());
                if due.is_empty() {
                    continue;
                }
                for (path, kind) in due {
                    let started = Instant::now();
                    let outcome = dispatcher.dispatch(path, kind).await;
                    finish_delta(&dispatcher, &outcome, started, &deltas_tx);
                }
                set_files_watched(&status, dispatcher.files_watched());
            }
        }
    }

    tracing::debug!(
        "workspaces.watcher: loop exited ({})",
        dispatcher.workspace_id()
    );
}

/// Log + broadcast one settled delta (skips a filtered no-op) on the loop's
/// own `deltas_tx`.
fn finish_delta<D: DeltaDispatcher>(
    dispatcher: &D,
    outcome: &delta::DeltaOutcome,
    started: Instant,
    deltas_tx: &broadcast::Sender<DeltaEvent>,
) {
    if outcome.action == "skip" {
        return;
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        "{DELTA_UPSERT_COMPLETE_EVENT}: ws={} action={} path={} graph_chunks={} \
         indexed={} took={elapsed_ms:.1}ms{}{}",
        dispatcher.workspace_id(),
        outcome.action,
        outcome.path,
        outcome.graph_chunk_nodes,
        outcome.chunks_indexed,
        outcome
            .graph_error
            .as_deref()
            .map(|e| format!(" graph_err={e}"))
            .unwrap_or_default(),
        outcome
            .vector_error
            .as_deref()
            .map(|e| format!(" vector_err={e}"))
            .unwrap_or_default(),
    );
    let _ = deltas_tx.send(DeltaEvent {
        workspace_id: dispatcher.workspace_id(),
        path: outcome.path.clone(),
        action: outcome.action,
        graph_chunk_nodes: outcome.graph_chunk_nodes,
        took_ms: elapsed_ms,
    });
}

fn note_event(status: &Arc<Mutex<WatcherStatus>>) {
    if let Ok(mut s) = status.lock() {
        s.last_event_at = Some(registry::epoch_now());
    }
}

fn set_paused(status: &Arc<Mutex<WatcherStatus>>, paused: bool) {
    if let Ok(mut s) = status.lock() {
        s.paused = paused;
    }
}

fn set_files_watched(status: &Arc<Mutex<WatcherStatus>>, n: u32) {
    if let Ok(mut s) = status.lock() {
        s.files_watched = n;
    }
}

// ── Process-wide lifecycle ──────────────────────────────────────────────────

/// One live watcher: the kept-alive notify handle, the loop's control channel,
/// and its shared status. Dropping it stops the OS watch and (via the closed
/// control channel) ends the loop.
struct ActiveWatcher {
    workspace_id: String,
    _watcher: ::notify::RecommendedWatcher,
    control: UnboundedSender<Control>,
    status: Arc<Mutex<WatcherStatus>>,
    _handle: tokio::task::JoinHandle<()>,
}

fn active() -> &'static Mutex<Option<ActiveWatcher>> {
    static ACTIVE: OnceLock<Mutex<Option<ActiveWatcher>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// Whether the auto-start hooks are armed (only the live service arms them).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Arm the watcher auto-start hooks. Called once by `main.rs` at service boot.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Whether [`enable`] has been called.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Debounce window from env, default [`DEFAULT_DEBOUNCE_MS`].
fn debounce_window() -> Duration {
    let ms = std::env::var("WYLDE_WORKSPACES_WATCH_DEBOUNCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_DEBOUNCE_MS);
    Duration::from_millis(ms)
}

/// React to a change in the active workspace: (re)start the watcher for the
/// current active workspace, or stop if there's none / it's ineligible (no
/// folder, RAG disabled, folder missing). No-op until [`enable`]d.
///
/// Called after `set_active` and `delete`, and once at boot.
pub fn on_active_changed() {
    if !is_enabled() {
        return;
    }
    let active_id = registry::state::load_or_default().active_id;
    match active_id.and_then(|id| registry::get(&id)) {
        Some(def)
            if def.rag_enabled
                && !def.folder.trim().is_empty()
                && Path::new(&def.folder).is_dir() =>
        {
            // Already watching this exact workspace? Leave it running.
            if active()
                .lock()
                .map(|g| g.as_ref().map(|a| a.workspace_id.clone()))
                .ok()
                .flatten()
                == Some(def.id.clone())
            {
                return;
            }
            if let Err(e) = start_for(def) {
                tracing::warn!("workspaces.watcher: failed to start: {e}");
            }
        }
        _ => stop(),
    }
}

/// Boot hook: arm + start watching the active workspace (if any).
pub fn on_boot() {
    enable();
    on_active_changed();
}

/// Build + spawn a watch for `def`, replacing any current watcher.
fn start_for(def: WorkspaceDefinition) -> ::notify::Result<()> {
    stop();

    let (ev_tx, ev_rx) = unbounded_channel::<RawChange>();
    let (ctrl_tx, ctrl_rx) = unbounded_channel::<Control>();

    let watcher = notify::build_watcher(&def.folder, ev_tx)?;

    let dispatcher = ServiceDispatcher::new(def.clone());
    let status = Arc::new(Mutex::new(WatcherStatus {
        active_workspace: Some(def.id.clone()),
        files_watched: dispatcher.files_watched(),
        last_event_at: None,
        paused: false,
    }));

    // The live service publishes on the process-wide bus, so `subscribe()`
    // callers (the graph panel) see this watcher's deltas.
    let handle = tokio::spawn(run_loop(
        ev_rx,
        ctrl_rx,
        dispatcher,
        status.clone(),
        debounce_window(),
        event_bus().clone(),
    ));

    *active().lock().expect("watcher mutex") = Some(ActiveWatcher {
        workspace_id: def.id.clone(),
        _watcher: watcher,
        control: ctrl_tx,
        status,
        _handle: handle,
    });
    tracing::info!("workspaces.watcher: watching {} ({})", def.folder, def.id);
    Ok(())
}

/// Stop the active watcher (if any). Idempotent.
pub fn stop() {
    if let Some(a) = active().lock().expect("watcher mutex").take() {
        let _ = a.control.send(Control::Shutdown); // best-effort control signal to watcher (wylde-check: discard-result-ok)
                                                   // Dropping `a` drops the notify handle (stops the OS watch) and the
                                                   // control sender; the loop ends on the Shutdown / closed channel.
        tracing::info!("workspaces.watcher: stopped ({})", a.workspace_id);
    }
}

/// The current status snapshot (default/empty when no watcher runs).
pub fn status() -> WatcherStatus {
    match &*active().lock().expect("watcher mutex") {
        Some(a) => a.status.lock().map(|s| s.clone()).unwrap_or_default(),
        None => WatcherStatus::default(),
    }
}

/// Pause the active watch (drop incoming events). Returns the watched
/// workspace id, or `None` if nothing is running.
pub fn pause() -> Option<String> {
    let guard = active().lock().expect("watcher mutex");
    let a = guard.as_ref()?;
    let _ = a.control.send(Control::Pause); // best-effort control signal to watcher (wylde-check: discard-result-ok)
    Some(a.workspace_id.clone())
}

/// Resume the active watch and re-walk the workspace to catch up on edits
/// missed while paused. Returns the watched workspace id, or `None`.
pub fn resume() -> Option<String> {
    let guard = active().lock().expect("watcher mutex");
    let a = guard.as_ref()?;
    let _ = a.control.send(Control::Resume); // best-effort control signal to watcher (wylde-check: discard-result-ok)
    Some(a.workspace_id.clone())
}

#[cfg(test)]
mod tests;
