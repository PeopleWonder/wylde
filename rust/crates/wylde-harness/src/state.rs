//! Per-turn state + process-wide turn registry.
//!
//! Slice 5.A only exposed [`new_turn_id`] — `chat.run_turn` was fully
//! synchronous. Slice 5.B (this one) populates the registry so:
//!
//! * `chat.start_turn` returns a turn id immediately and spawns the
//!   turn-driving task.
//! * `chat.cancel` flips a per-turn cancellation flag (the task
//!   observes between Ollama chunks) and returns whether the turn was
//!   in-flight.
//! * `chat.stream_turn` / `chat.stream_tools` subscribe to a turn's
//!   event buffers and emit each event as one IPC stream chunk.
//!
//! ## Event buffering model
//!
//! Each [`TurnHandle`] holds a `Vec<TurnEvent>` and a `Vec<ToolEvent>`
//! under independent `Mutex`es plus a single [`tokio::sync::Notify`]
//! that fires on every append. Streaming subscribers maintain their
//! own cursor into the buffer — so multiple subscribers (e.g. the
//! caller plus an observer dashboard) can each get the full sequence
//! from their subscription point.
//!
//! The append-only buffer is also what makes `chat.stream_turn`
//! tolerate a brief gap between `chat.start_turn` returning and the
//! subscriber connecting — events appended before subscription are
//! still in the buffer and replayed from cursor 0. This matches the
//! Python long-poll's `cursor` semantics. A bounded subscribe-late
//! window past the buffer cap is the only failure mode and is fine for
//! the slice; future work can swap in tokio's `broadcast` if a
//! buffer-cap eviction strategy becomes necessary.
//!
//! ## Wire compatibility with the Python registry
//!
//! Python uses `uuid.uuid4().hex` — the 32-char no-hyphen form. We
//! mirror that so a turn id round-tripped through the strangler still
//! looks like every other id in the logs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, Notify};
use uuid::Uuid;

use crate::events::{ToolEvent, TurnEvent};

/// Fresh hex turn id — same shape as the Python driver's
/// `_new_turn_id()` (uuid4 with hyphens stripped).
pub fn new_turn_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Per-turn shared state — owned by an `Arc` so the driving task,
/// every stream subscriber, and the cancel handler can hold their
/// own clone.
pub struct TurnHandle {
    pub turn_id: String,
    pub conversation_id: String,
    /// Fires when [`cancelled`] flips true. The turn-driving task
    /// `select!`s on it between Ollama chunks so a cancel mid-stream
    /// stops the upstream generation promptly.
    pub cancel: Arc<Notify>,
    pub cancelled: Arc<AtomicBool>,
    /// Flipped true once the turn reaches a terminal state (complete,
    /// aborted, or error). Stream subscribers drain the remaining
    /// events past their cursor and exit when this is set.
    pub done: Arc<AtomicBool>,
    /// Append-only event buffer for `chat.stream_turn` subscribers.
    pub turn_events: Arc<AsyncMutex<Vec<TurnEvent>>>,
    /// Append-only event buffer for `chat.stream_tools` subscribers.
    pub tool_events: Arc<AsyncMutex<Vec<ToolEvent>>>,
    /// Fires whenever a new event lands in either buffer or `done`
    /// flips. Subscribers `notified()` between buffer reads.
    pub notify: Arc<Notify>,
}

impl TurnHandle {
    fn new(turn_id: String, conversation_id: String) -> Self {
        Self {
            turn_id,
            conversation_id,
            cancel: Arc::new(Notify::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            turn_events: Arc::new(AsyncMutex::new(Vec::new())),
            tool_events: Arc::new(AsyncMutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Append a user-facing event and wake every subscriber.
    pub async fn push_turn_event(&self, ev: TurnEvent) {
        self.turn_events.lock().await.push(ev);
        self.notify.notify_waiters();
    }

    /// Append a tool-activity event and wake every subscriber.
    #[allow(dead_code)] // Wired up in slice 5.C when tool decode lands.
    pub async fn push_tool_event(&self, ev: ToolEvent) {
        self.tool_events.lock().await.push(ev);
        self.notify.notify_waiters();
    }

    /// Mark terminal. Subscribers drain past their cursor and exit.
    pub fn mark_done(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Process-wide turn registry. Lazy-initialised; one slot per active
/// turn id.
fn registry() -> &'static StdMutex<HashMap<String, Arc<TurnHandle>>> {
    static REG: OnceLock<StdMutex<HashMap<String, Arc<TurnHandle>>>> = OnceLock::new();
    REG.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Register a fresh turn handle. Caller supplies the turn id (so the
/// `start_turn` handler can echo whatever id the client passed); if
/// the id collides with an active turn the new handle wins.
pub fn register_turn(turn_id: String, conversation_id: String) -> Arc<TurnHandle> {
    let handle = Arc::new(TurnHandle::new(turn_id.clone(), conversation_id));
    let mut reg = registry().lock().expect("turn registry poisoned");
    reg.insert(turn_id, Arc::clone(&handle));
    handle
}

/// Look up an active turn. Returns `None` once the entry has been
/// removed via [`remove_turn`] — typically the long-tail drop after a
/// turn finishes.
pub fn get_turn(turn_id: &str) -> Option<Arc<TurnHandle>> {
    let reg = registry().lock().expect("turn registry poisoned");
    reg.get(turn_id).map(Arc::clone)
}

/// Cancel an in-flight turn. Returns `true` if the turn was present
/// AND not already cancelled (matches the Python `cancel_turn`
/// return shape: "did this call actually request a cancel").
pub fn cancel_turn(turn_id: &str) -> bool {
    let handle = match get_turn(turn_id) {
        Some(h) => h,
        None => return false,
    };
    if handle.cancelled.swap(true, Ordering::SeqCst) {
        return false; // already cancelled
    }
    handle.cancel.notify_waiters();
    handle.notify.notify_waiters();
    true
}

/// Drop the registry slot for `turn_id`. The driving task calls this
/// after [`TurnHandle::mark_done`] when no subscribers can still race;
/// the canonical place is at end-of-task once the broadcast has been
/// drained. Idempotent.
pub fn remove_turn(turn_id: &str) {
    let mut reg = registry().lock().expect("turn registry poisoned");
    reg.remove(turn_id);
}

/// Test helper: drop every registered turn.
///
/// Deliberately NOT called from `service::reset_for_tests`: that
/// helper runs concurrently with `turn::actions` tests that registered
/// their own (uuid-unique) slots, and a global wipe would race them.
/// Reserved for the rare diagnostic flow where a debugger or repl-
/// driven session wants a clean slate.
#[allow(dead_code)]
pub(crate) fn clear_all_turns() {
    let mut reg = registry().lock().expect("turn registry poisoned");
    reg.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AbortReason;

    #[test]
    fn turn_id_is_32_hex_chars() {
        let id = new_turn_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn turn_ids_are_unique() {
        let a = new_turn_id();
        let b = new_turn_id();
        assert_ne!(a, b);
    }

    // Test hygiene note: every test uses a unique uuid for `turn_id`,
    // so the registry can hold cruft from other parallel tests without
    // contaminating us. The old `clear_all_turns()` call here used to
    // wipe everything mid-test — racing with other tests that had just
    // registered their own turn. Cleanup is per-id via `remove_turn`.

    #[tokio::test]
    async fn register_and_get_turn_returns_same_handle() {
        let id = new_turn_id();
        let h1 = register_turn(id.clone(), "c1".into());
        let h2 = get_turn(&id).expect("registered");
        // Same Arc — they should point at the same underlying state.
        assert!(Arc::ptr_eq(&h1, &h2));
        remove_turn(&id);
        assert!(get_turn(&id).is_none());
    }

    #[tokio::test]
    async fn cancel_turn_flips_flag_once() {
        let id = new_turn_id();
        let _ = register_turn(id.clone(), "c1".into());

        assert!(cancel_turn(&id), "first cancel returns true");
        assert!(!cancel_turn(&id), "second cancel is a no-op (already cancelled)");

        let handle = get_turn(&id).expect("still in registry");
        assert!(handle.is_cancelled());
        remove_turn(&id);
    }

    #[tokio::test]
    async fn cancel_turn_returns_false_for_unknown_id() {
        // A literal unknown id can't collide with anything other tests
        // registered (they use fresh uuids); no clear needed.
        assert!(!cancel_turn("no-such-turn-literal"));
    }

    #[tokio::test]
    async fn push_event_wakes_a_notify_waiter() {
        let id = new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        // Subscribe to the notify BEFORE the push so the wait is racing.
        let notified = handle.notify.notified();
        tokio::pin!(notified);

        let h2 = Arc::clone(&handle);
        let pid = id.clone();
        tokio::spawn(async move {
            // Slight delay so the await below is parked first.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            h2.push_turn_event(TurnEvent::TurnComplete {
                turn_id: pid,
                final_message: "ok".into(),
            })
            .await;
        });

        // If notify_waiters doesn't fire we hang forever — test timeout
        // (default 60s) catches it.
        tokio::time::timeout(std::time::Duration::from_secs(2), notified)
            .await
            .expect("notify should fire on push");

        let buf = handle.turn_events.lock().await;
        assert_eq!(buf.len(), 1);
        remove_turn(&id);
    }

    #[tokio::test]
    async fn mark_done_sets_flag_and_wakes() {
        let id = new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        let notified = handle.notify.notified();
        tokio::pin!(notified);

        let h2 = Arc::clone(&handle);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            h2.mark_done();
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), notified)
            .await
            .expect("notify should fire on mark_done");

        assert!(handle.is_done());
        remove_turn(&id);
    }

    #[tokio::test]
    async fn cancel_wakes_the_per_turn_cancel_notify() {
        let id = new_turn_id();
        let handle = register_turn(id.clone(), "c1".into());

        // Subscribe to the cancel notify BEFORE flipping the flag so
        // the wait is racing with `notify_waiters()`.
        let cancel_wait = handle.cancel.notified();
        tokio::pin!(cancel_wait);

        // Cancel synchronously — no spawn race; the swap+notify
        // happens before the await below.
        assert!(cancel_turn(&id), "first cancel should return true");

        tokio::time::timeout(std::time::Duration::from_secs(2), cancel_wait)
            .await
            .expect("cancel notify should fire");
        remove_turn(&id);
    }

    #[test]
    fn turn_aborted_event_shape_round_trips() {
        // Sanity that the AbortReason enum round-trips through serde
        // — important for the streaming wire format.
        let ev = TurnEvent::TurnAborted {
            turn_id: "t".into(),
            reason: AbortReason::Cancelled,
            error: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "turn_aborted");
        assert_eq!(v["reason"], "cancelled");
    }
}
