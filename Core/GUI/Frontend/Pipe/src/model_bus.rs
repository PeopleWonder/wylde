//! Cross-panel model bus.
//!
//! Sibling of [`crate::conversation_bus`] — a *broadcast* + last-value
//! latch — but it answers "which model is effective right now" rather
//! than "which conversation is active". The Settings → "Ollama inference"
//! section follows it to re-query a model's parameter defaults the moment
//! the user changes model elsewhere (its "State 4" live update).
//!
//! Two publishers, one subscriber today:
//!
//!   * The **Chat** panel calls [`publish_active_model`] from its
//!     `select_model` so the inference-bar pick is observable cross-panel
//!     (it also persists the pick via `models.set_active`).
//!   * The **Models** panel calls [`publish_starred_default`] when the
//!     starred-default changes.
//!   * The **Settings** panel [`subscribe`]s and, on each event, re-reads
//!     the *effective* model (`models.get_effective`) + its defaults.
//!
//! Why a dedicated bus rather than polling: matches the established
//! `conversation_bus` pattern so the two buses stay shaped the same, and
//! a broadcast costs nothing when no one is listening.
//!
//! Lives in `wylde-gui-pipe` for the same dependency-graph reason
//! [`crate::conversation_bus`] documents: panel crates depend on the pipe
//! crate, so a shared channel here avoids a registry↔panel cycle. Lazily
//! initialised on first use — no `install_*` step.

use std::sync::{Mutex, OnceLock};

use tokio::sync::broadcast;

/// Channel depth. Model changes are rare (a user picking a model, not a
/// token stream), so a small buffer is ample; a lagging subscriber
/// recovers via [`current_active_model`].
const CHANNEL_CAPACITY: usize = 64;

/// A cross-panel model event.
///
/// Kept small + `Clone` because `broadcast` clones the value once per
/// receiver. `#[non_exhaustive]` so new variants don't break subscribers
/// that already match non-exhaustively.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    /// The inference-bar pick changed (Chat `select_model`). `model` is
    /// the new pick, or `None` for the "(auto)" / cleared state.
    ActiveModelChanged { model: Option<String> },
    /// The starred default changed (Models panel star). `model` is the
    /// new starred default, or `None` when the star was cleared.
    StarredDefaultChanged { model: Option<String> },
}

/// Process-wide broadcast sender, lazily created on first use.
static SENDER: OnceLock<broadcast::Sender<ModelEvent>> = OnceLock::new();

/// Last-published *active* model, so a panel mounting after Chat already
/// published can seed itself. Carries the `Option` verbatim — `None` is a
/// meaningful state ("(auto)"), distinct from "nothing published yet"
/// which [`current_active_model`] folds to `None` as well (a late
/// subscriber re-derives the real effective model via the pipe anyway).
static ACTIVE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<ModelEvent> {
    SENDER.get_or_init(|| broadcast::channel(CHANNEL_CAPACITY).0)
}

fn active_cell() -> &'static Mutex<Option<String>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// Subscribe to model events. Each call returns an independent receiver;
/// drop it to unsubscribe. Safe to call before any publish.
pub fn subscribe() -> broadcast::Receiver<ModelEvent> {
    sender().subscribe()
}

/// Publish a raw [`ModelEvent`]. Returns the number of receivers reached
/// (`0` when nothing is listening — a no-op, not an error).
pub fn publish(event: ModelEvent) -> usize {
    sender().send(event).unwrap_or(0)
}

/// Announce the active inference-bar pick. Updates the last-value latch
/// and broadcasts [`ModelEvent::ActiveModelChanged`].
pub fn publish_active_model(model: Option<String>) -> usize {
    if let Ok(mut slot) = active_cell().lock() {
        *slot = model.clone();
    }
    publish(ModelEvent::ActiveModelChanged { model })
}

/// Announce the starred-default change. Does **not** touch the active
/// latch (the star and the live pick are distinct selections); Settings
/// re-resolves the effective model on either event.
pub fn publish_starred_default(model: Option<String>) -> usize {
    publish(ModelEvent::StarredDefaultChanged { model })
}

/// The most recently announced active model, or `None` if no Chat panel
/// has published one yet this session (also `None` for the "(auto)"
/// state). A subscriber reads this on mount to seed its view, then
/// follows [`subscribe`] for changes.
pub fn current_active_model() -> Option<String> {
    active_cell().lock().ok().and_then(|slot| slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The broadcast channel + latch are process-wide `OnceLock`s shared
    // by every test in this binary. Run concurrently they cross-talk, so
    // a module-wide guard serialises the bus tests; it recovers from a
    // poisoned lock so one panicking test doesn't cascade.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn publish_active_updates_latch() {
        let _g = guard();
        let _ = publish_active_model(Some("llama3.2:3b".into()));
        assert_eq!(current_active_model().as_deref(), Some("llama3.2:3b"));
        // The "(auto)" / cleared state latches as None.
        let _ = publish_active_model(None);
        assert_eq!(current_active_model(), None);
    }

    #[test]
    fn subscriber_receives_active_change() {
        let _g = guard();
        let mut rx = subscribe();
        let reached = publish_active_model(Some("m:1".into()));
        assert!(reached >= 1, "the live subscriber should be counted");
        match rx.try_recv() {
            Ok(ModelEvent::ActiveModelChanged { model }) => {
                assert_eq!(model.as_deref(), Some("m:1"));
            }
            other => panic!("expected ActiveModelChanged, got {other:?}"),
        }
    }

    #[test]
    fn subscriber_receives_starred_change_without_touching_latch() {
        let _g = guard();
        // Seed the active latch, then publish a star change.
        let _ = publish_active_model(Some("active:1".into()));
        let mut rx = subscribe();
        let _ = publish_starred_default(Some("star:1".into()));
        match rx.try_recv() {
            Ok(ModelEvent::StarredDefaultChanged { model }) => {
                assert_eq!(model.as_deref(), Some("star:1"));
            }
            other => panic!("expected StarredDefaultChanged, got {other:?}"),
        }
        // The active latch is untouched by a star change.
        assert_eq!(current_active_model().as_deref(), Some("active:1"));
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let _g = guard();
        let _ = publish_starred_default(Some("nobody:listening".into()));
    }

    #[test]
    fn late_subscriber_reads_latch_then_follows_events() {
        let _g = guard();
        let _ = publish_active_model(Some("late:1".into()));
        assert_eq!(current_active_model().as_deref(), Some("late:1"));
        let mut rx = subscribe();
        let _ = publish_active_model(Some("late:2".into()));
        match rx.try_recv() {
            Ok(ModelEvent::ActiveModelChanged { model }) => {
                assert_eq!(model.as_deref(), Some("late:2"));
            }
            other => panic!("expected the post-subscribe change, got {other:?}"),
        }
    }
}
