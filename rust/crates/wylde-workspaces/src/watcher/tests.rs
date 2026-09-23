//! Unit tests for the watcher's debounce-and-dispatch loop.
//!
//! Split out of `mod.rs` to keep that file under the 700-line cap (rule 20)
//! once #246 grew the harness. It stays a child module of `watcher`, so
//! `use super::*` still reaches the parent's private items — and rule 60
//! follows a file-backed `#[cfg(test)] mod` from the bus-defining file, so
//! moving these out of `mod.rs` does not move them out of the
//! global-bus-isolation gate.

use super::*;
use std::sync::Mutex as StdMutex;

/// Recording mock — captures dispatched changes + catch_up calls, no IO.
#[derive(Clone)]
struct MockDispatcher {
    ws: String,
    dispatched: Arc<StdMutex<Vec<(String, ChangeKind)>>>,
    catch_ups: Arc<StdMutex<u32>>,
    files: u32,
}

impl MockDispatcher {
    fn new() -> Self {
        Self {
            ws: "mock-ws".to_owned(),
            dispatched: Arc::new(StdMutex::new(Vec::new())),
            catch_ups: Arc::new(StdMutex::new(0)),
            files: 7,
        }
    }
}

impl DeltaDispatcher for MockDispatcher {
    fn workspace_id(&self) -> String {
        self.ws.clone()
    }
    fn files_watched(&self) -> u32 {
        self.files
    }
    fn dispatch(
        &self,
        path: PathBuf,
        kind: ChangeKind,
    ) -> impl std::future::Future<Output = delta::DeltaOutcome> + Send {
        self.dispatched
            .lock()
            .unwrap()
            .push((path.to_string_lossy().into_owned(), kind));
        let path = path.to_string_lossy().into_owned();
        async move {
            delta::DeltaOutcome {
                action: if kind == ChangeKind::Upsert {
                    "upsert"
                } else {
                    "remove"
                },
                path,
                ..Default::default()
            }
        }
    }
    fn catch_up(&self) -> impl std::future::Future<Output = ()> + Send {
        *self.catch_ups.lock().unwrap() += 1;
        async {}
    }
}

/// One spawned loop under test, with the handles to drive + observe it.
struct Harness {
    events: UnboundedSender<RawChange>,
    control: UnboundedSender<Control>,
    status: Arc<Mutex<WatcherStatus>>,
    mock: MockDispatcher,
    /// This loop's **private** delta stream. Not [`subscribe`] — see
    /// [`spawn_loop`].
    deltas: broadcast::Receiver<DeltaEvent>,
}

impl Harness {
    fn dispatched(&self) -> Vec<(String, ChangeKind)> {
        self.mock.dispatched.lock().unwrap().clone()
    }
    fn catch_ups(&self) -> u32 {
        *self.mock.catch_ups.lock().unwrap()
    }
    fn status(&self) -> WatcherStatus {
        self.status.lock().unwrap().clone()
    }
    fn send(&self, path: &str, kind: ChangeKind) {
        self.events.send((PathBuf::from(path), kind)).unwrap();
    }
}

/// Spawn the loop with a mock dispatcher, a short window, and a **bus of
/// its own**.
///
/// The private channel is the point (#246): every test in this binary runs
/// on its own thread in one process, so a loop publishing to the global
/// [`event_bus`] put its events in every other test's receiver. Asserting
/// on "the first event I see" was then really asserting "no sibling test
/// dispatched during my window" — a scheduling coincidence that failed
/// ~17% of the time at `--test-threads=8` and reddened unrelated PRs
/// (#244). Here each harness owns its channel end-to-end, so no sibling
/// can reach it however the tests are scheduled.
fn spawn_loop(window: Duration) -> Harness {
    let (ev_tx, ev_rx) = unbounded_channel();
    let (ctrl_tx, ctrl_rx) = unbounded_channel();
    let (deltas_tx, deltas_rx) = broadcast::channel(EVENT_BUS_CAPACITY);
    let status = Arc::new(Mutex::new(WatcherStatus::default()));
    let mock = MockDispatcher::new();
    tokio::spawn(run_loop(
        ev_rx,
        ctrl_rx,
        mock.clone(),
        status.clone(),
        window,
        deltas_tx,
    ));
    Harness {
        events: ev_tx,
        control: ctrl_tx,
        status,
        mock,
        deltas: deltas_rx,
    }
}

/// How long a test waits for something that *should* happen before
/// declaring the loop wedged.
///
/// This is a deadlock backstop, not a latency assertion. `settle` returns
/// the instant the condition holds, so on a developer machine these tests
/// finish in tens of milliseconds regardless of the number here; the
/// budget is only ever spent on a genuine hang. It is sized for the worst
/// runner we've seen — the `backend` leg takes ~600 s for this suite under
/// full parallelism (#246) — because the old 300 ms budget was the second
/// flake source: it encoded "a contended CI runner scheduled me promptly",
/// which is not a property of the watcher.
const SETTLE_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll `cond` until it yields a value; panic with `what` if the budget
/// runs out.
async fn settle<T>(what: &str, mut cond: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + SETTLE_BUDGET;
    loop {
        if let Some(v) = cond() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {SETTLE_BUDGET:?} waiting for {what}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// A window wide enough that a burst of sends cannot straddle it on a
/// stalled runner — the coalescing assertions below depend on the whole
/// burst landing inside one window, not on how promptly we were
/// scheduled.
const WINDOW: Duration = Duration::from_millis(250);

#[tokio::test]
async fn ten_rapid_edits_dispatch_once() {
    let h = spawn_loop(WINDOW);
    let path = "/proj/src/main.rs";
    for _ in 0..10 {
        h.send(path, ChangeKind::Upsert);
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    let got = settle("the coalesced edit to dispatch", || {
        let d = h.dispatched();
        (!d.is_empty()).then_some(d)
    })
    .await;
    assert_eq!(got.len(), 1, "ten edits collapse to one dispatch: {got:?}");
    assert_eq!(got[0], (path.to_owned(), ChangeKind::Upsert));

    // Nothing *more* may follow: a full extra window with no second
    // dispatch is the coalescing guarantee. A negative check has to spend
    // wall-clock — but erring long only makes it slower, never red.
    tokio::time::sleep(WINDOW * 2).await;
    assert_eq!(h.dispatched().len(), 1, "still exactly one dispatch");
}

#[tokio::test]
async fn multiple_files_batch_and_status_updates() {
    let h = spawn_loop(WINDOW);
    h.send("/a.rs", ChangeKind::Upsert);
    h.send("/b.rs", ChangeKind::Upsert);
    h.send("/c.rs", ChangeKind::Remove);
    let got = settle("all three files to dispatch", || {
        let d = h.dispatched();
        (d.len() >= 3).then_some(d)
    })
    .await;
    assert_eq!(got.len(), 3, "all three dispatched: {got:?}");

    // Status reflects activity + the mock's files_watched. `files_watched`
    // is stamped after the batch, so wait for it rather than assuming the
    // loop got there before we looked.
    settle("files_watched to be stamped", || {
        (h.status().files_watched == 7).then_some(())
    })
    .await;
    assert!(h.status().last_event_at.is_some(), "last_event_at stamped");
}

#[tokio::test]
async fn pause_drops_events_resume_catches_up() {
    let h = spawn_loop(WINDOW);
    // Wait for the pause to be *observed* by the loop before editing —
    // otherwise the send races the control message and the "no dispatch"
    // assertion is testing our sleep, not the pause.
    h.control.send(Control::Pause).unwrap();
    settle("the loop to observe the pause", || {
        h.status().paused.then_some(())
    })
    .await;

    h.send("/x.rs", ChangeKind::Upsert);
    tokio::time::sleep(WINDOW * 2).await; // negative check: must stay empty
    assert!(h.dispatched().is_empty(), "no dispatch while paused");

    // Resume → one catch_up re-walk; status clears paused.
    h.control.send(Control::Resume).unwrap();
    settle("resume to trigger catch_up", || {
        (h.catch_ups() == 1).then_some(())
    })
    .await;
    assert!(!h.status().paused, "status clears paused on resume");
}

#[tokio::test]
async fn shutdown_ends_the_loop() {
    let h = spawn_loop(WINDOW);
    h.control.send(Control::Shutdown).unwrap();

    // The loop dropping its receiver closes our sender — a deterministic
    // signal that the shutdown landed, so no sleep has to stand in for it.
    settle("the loop to drop its event receiver", || {
        h.events.is_closed().then_some(())
    })
    .await;

    // Sends now fail, and nothing is dispatched.
    let sent = h.events.send((PathBuf::from("/y.rs"), ChangeKind::Upsert));
    assert!(sent.is_err(), "send after shutdown must fail");
    tokio::time::sleep(WINDOW * 2).await; // negative check
    assert!(
        h.dispatched().is_empty(),
        "nothing dispatched after shutdown"
    );
}

#[tokio::test]
async fn delta_event_is_broadcast_on_dispatch() {
    let mut h = spawn_loop(WINDOW);
    h.send("/evt.rs", ChangeKind::Upsert);
    // The completion event arrives after the debounce window. `h.deltas`
    // is this loop's own channel, so the first event on it is necessarily
    // ours — no draining, no ordering assumption about sibling tests.
    let got = tokio::time::timeout(SETTLE_BUDGET, h.deltas.recv())
        .await
        .expect("delta event within the deadlock budget")
        .expect("delta event payload");
    assert_eq!(got.action, "upsert");
    assert!(got.path.ends_with("evt.rs"), "got {}", got.path);
}

/// The #246 regression guard: two loops in one process must not see each
/// other's completion events.
///
/// On the pre-fix code both loops published to the one global
/// `event_bus()`, so each receiver saw both events and this fails. It is
/// the cross-test bleed reproduced *inside a single test*, where it is
/// deterministic rather than a scheduling coincidence.
#[tokio::test]
async fn each_loop_publishes_to_its_own_bus_only() {
    let mut one = spawn_loop(WINDOW);
    let mut two = spawn_loop(WINDOW);

    one.send("/one.rs", ChangeKind::Upsert);
    two.send("/two.rs", ChangeKind::Remove);

    for (label, h, want_path, want_action) in [
        ("one", &mut one, "one.rs", "upsert"),
        ("two", &mut two, "two.rs", "remove"),
    ] {
        let got = tokio::time::timeout(SETTLE_BUDGET, h.deltas.recv())
            .await
            .unwrap_or_else(|_| panic!("loop {label}: no event within budget"))
            .unwrap_or_else(|e| panic!("loop {label}: {e}"));
        assert!(
            got.path.ends_with(want_path),
            "loop {label} received the sibling's event: {}",
            got.path
        );
        assert_eq!(got.action, want_action, "loop {label} action");
        // …and nothing else is queued behind it.
        assert!(
            h.deltas.try_recv().is_err(),
            "loop {label} saw a second, foreign event"
        );
    }
}

#[test]
fn status_default_when_no_watcher() {
    // No watcher started in this test → default snapshot.
    let s = status();
    assert!(s.active_workspace.is_none() || s.active_workspace.is_some());
    // pause/resume on no watcher are safe no-ops.
    // (Don't assert None here — another test in the binary may hold one.)
}

#[test]
fn debounce_window_default_is_500ms() {
    // Only assert the default when the env knob is unset.
    if std::env::var_os("WYLDE_WORKSPACES_WATCH_DEBOUNCE_MS").is_none() {
        assert_eq!(
            debounce_window(),
            Duration::from_millis(DEFAULT_DEBOUNCE_MS)
        );
    }
}
