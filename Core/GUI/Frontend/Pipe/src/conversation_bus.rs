//! Cross-panel conversation bus.
//!
//! Sibling of [`crate::nav_bus`], but a *broadcast* rather than a single-
//! consumer channel.  The nav bus answers "which panel is selected" and
//! has exactly one drain (the Shell); this bus answers "which conversation
//! is active" and has *many* interested readers — the Memory panel's
//! short-term view, the Dashboard, and (later) anything that wants to
//! follow the live chat without owning the conversation id itself.
//!
//! Shape:
//!
//!   * A process-wide `tokio::sync::broadcast` channel.  Any panel calls
//!     [`subscribe`] to get its own receiver; the Chat panel calls
//!     [`publish_active_conversation`] whenever it adopts / switches the
//!     conversation id.  `broadcast` fans one send out to every live
//!     receiver, so adding a subscriber is free and order-independent.
//!   * A last-value latch ([`current_active_conversation`]).  `broadcast`
//!     only delivers to receivers that exist *at send time*, so a panel
//!     that mounts after the Chat panel already published would otherwise
//!     miss the current conversation until the next switch.  The latch
//!     lets a late subscriber read the current value on mount, then keep
//!     up via its receiver.
//!
//! Lives in `wylde-gui-pipe` for the same dependency-graph reason
//! [`crate::nav_bus`] documents: the panel crates depend on the pipe
//! crate, so a shared channel here avoids a registry↔panel cycle.  (The
//! task brief floated `wylde-shared` / `wylde-panel-registry`; the pipe
//! crate is the established home for the nav bus and keeps both buses
//! side-by-side.)
//!
//! The channel is lazily initialised on first use, so unlike the nav bus
//! there is no `install_*` step in `main.rs` — `subscribe` before any
//! `publish` is fine (the receiver simply waits), and `publish` with no
//! subscribers is a silently-absorbed no-op.

use std::sync::{Mutex, OnceLock};

use tokio::sync::broadcast;

/// Channel depth.  Conversation events are rare (a user switching chats,
/// not a token stream), so a small buffer is ample; a slow subscriber
/// that falls this far behind gets a `Lagged` it recovers from by reading
/// [`current_active_conversation`].
const CHANNEL_CAPACITY: usize = 64;

/// A cross-panel conversation event.
///
/// Kept deliberately small + `Clone` because `broadcast` clones the value
/// once per receiver.  New variants can be added without touching
/// subscribers that `match` non-exhaustively (they already must, since
/// the enum is `#[non_exhaustive]`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationEvent {
    /// The active conversation changed — either the Chat panel adopted a
    /// harness-minted id for a fresh "default" chat, or the user switched
    /// to a different conversation.  Carries the new id so subscribers can
    /// re-query conversation-scoped state (e.g. the short-term buffer).
    ActiveConversationChanged { conversation_id: String },
    /// The set of conversations changed — one was created, deleted, or
    /// renamed.  Carries no id; subscribers that render a conversation
    /// list re-fetch it.  (Emitted by Slice B's switcher; the bus ships
    /// the variant now so subscribers can match it exhaustively.)
    ConversationListChanged,
}

/// The process-wide broadcast sender, lazily created on first use.
static SENDER: OnceLock<broadcast::Sender<ConversationEvent>> = OnceLock::new();

/// Last-published active conversation id.  Lets a late subscriber catch
/// up on mount (see the module note on the broadcast "no replay" gap).
static ACTIVE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn sender() -> &'static broadcast::Sender<ConversationEvent> {
    SENDER.get_or_init(|| broadcast::channel(CHANNEL_CAPACITY).0)
}

fn active_cell() -> &'static Mutex<Option<String>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// Subscribe to conversation events.  Each call returns an independent
/// receiver; drop it to unsubscribe.  Safe to call before any publish —
/// the receiver simply waits for the first event.
pub fn subscribe() -> broadcast::Receiver<ConversationEvent> {
    sender().subscribe()
}

/// Publish a raw [`ConversationEvent`].  Returns the number of receivers
/// the event reached (`0` when nothing is listening — a no-op, not an
/// error).  Prefer the typed helpers below for the common cases.
pub fn publish(event: ConversationEvent) -> usize {
    // `send` errors only when there are no receivers; that's a benign
    // "nobody is listening yet" so we fold it to 0.
    sender().send(event).unwrap_or(0)
}

/// Announce the active conversation.  Updates the last-value latch (so a
/// panel mounting later can read it) and broadcasts an
/// [`ConversationEvent::ActiveConversationChanged`].
///
/// Idempotent on the latch: re-announcing the same id still broadcasts
/// (subscribers may want to force a refresh), but [`current_active_conversation`]
/// settles on the same value.
pub fn publish_active_conversation(conversation_id: impl Into<String>) -> usize {
    let id = conversation_id.into();
    if let Ok(mut slot) = active_cell().lock() {
        *slot = Some(id.clone());
    }
    publish(ConversationEvent::ActiveConversationChanged {
        conversation_id: id,
    })
}

/// Announce that the conversation *list* changed (create / delete /
/// rename).  Does not touch the active-conversation latch.
pub fn publish_conversation_list_changed() -> usize {
    publish(ConversationEvent::ConversationListChanged)
}

/// The most recently announced active conversation id, or `None` if no
/// Chat panel has published one yet this session.  A subscriber reads
/// this on mount to seed its view, then follows [`subscribe`] for changes.
pub fn current_active_conversation() -> Option<String> {
    active_cell().lock().ok().and_then(|slot| slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The broadcast channel + latch are process-wide `OnceLock`s shared by
    // every test in this binary.  Run concurrently they cross-talk — one
    // test's publish lands in another's receiver, and the single latch is
    // clobbered between a write and its read.  A module-wide guard
    // serialises the bus tests so each sees the global exclusively; the
    // guard recovers from a poisoned lock (a panicking test) so one
    // failure doesn't cascade into spurious failures for the rest.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn publish_active_updates_latch() {
        let _g = guard();
        let _ = publish_active_conversation("conv-latch-test");
        assert_eq!(
            current_active_conversation().as_deref(),
            Some("conv-latch-test"),
        );
    }

    #[test]
    fn subscriber_receives_active_change() {
        let _g = guard();
        let mut rx = subscribe();
        let reached = publish_active_conversation("conv-sub-test");
        assert!(reached >= 1, "the live subscriber should be counted");
        // Serialised, so the only message in our receiver is ours.
        match rx.try_recv() {
            Ok(ConversationEvent::ActiveConversationChanged { conversation_id }) => {
                assert_eq!(conversation_id, "conv-sub-test");
            }
            other => panic!("expected ActiveConversationChanged, got {other:?}"),
        }
    }

    #[test]
    fn subscriber_receives_list_change() {
        let _g = guard();
        let mut rx = subscribe();
        let _ = publish_conversation_list_changed();
        assert!(matches!(
            rx.try_recv(),
            Ok(ConversationEvent::ConversationListChanged),
        ));
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let _g = guard();
        // No receiver held → send folds the "no receivers" error to 0
        // rather than panicking.
        let _ = publish_conversation_list_changed();
    }

    #[test]
    fn late_subscriber_reads_latch_then_follows_events() {
        let _g = guard();
        // Publish BEFORE subscribing: the broadcast won't replay, but the
        // latch carries the value so a late mounter can seed itself.
        let _ = publish_active_conversation("conv-late");
        assert_eq!(current_active_conversation().as_deref(), Some("conv-late"));

        // A receiver taken now still picks up the *next* change.
        let mut rx = subscribe();
        let _ = publish_active_conversation("conv-late-2");
        match rx.try_recv() {
            Ok(ConversationEvent::ActiveConversationChanged { conversation_id }) => {
                assert_eq!(conversation_id, "conv-late-2");
            }
            other => panic!("expected the post-subscribe change, got {other:?}"),
        }
    }
}
