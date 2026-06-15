//! Cross-panel **workspace-scope** bus — "which workspace is the docked chat
//! scoped to right now".
//!
//! Companion to [`crate::conversation_bus`] (active *conversation*) and
//! [`crate::focus_bus`] (deep-link focus). This bus answers a third,
//! orthogonal question: when the user *enters* a workspace in the Workspaces
//! panel, the InferenceBar dock that lives in the Chat panel must re-scope its
//! chat to that `workspace_id`; when they *leave*, it must clear back to
//! unbound. That crosses a panel boundary (producer = Workspaces panel,
//! consumer = the docked `ChatPanel`), exactly the situation the pipe-crate
//! buses exist for.
//!
//! Shape (mirrors [`crate::focus_bus`], single-consumer): the docked
//! `ChatPanel` is the one consumer, so it *drains* the receiver; the
//! Workspaces panel's `enter_workspace`/`leave_workspace` push a
//! [`WorkspaceScope`]. The channel is an unbounded mpsc created lazily on
//! first use, so a scope published *before* the docked panel has mounted is
//! **buffered** and delivered the moment the panel starts draining — no Shell
//! coupling, no lost enter, no polling.
//!
//! A last-value latch ([`current_active_workspace`]) lets the docked panel
//! seed itself synchronously on mount (the same late-mounter gap
//! [`crate::conversation_bus`] documents): rather than replaying the whole
//! buffer, a panel can read the *current* scope in one call, then follow the
//! receiver for subsequent changes.
//!
//! Lives in `wylde-gui-pipe` for the same dependency-graph reason the other
//! buses do: it is the one crate both the producing and consuming panel crates
//! already depend on, so a shared channel here avoids a registry↔panel cycle.
//!
//! This module ships the bus **only**; the producer (`enter_workspace` /
//! `leave_workspace` publishing here) and the consumer (the docked `ChatPanel`
//! subscribing) land in slice C3.

use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;

/// The docked chat's active workspace scope.
///
/// * `Some(workspace_id)` — a workspace was entered; bind the dock to it.
/// * `None` — the workspace was left; clear the dock's scope back to unbound.
///
/// Kept as a bare `Option<String>` (rather than a one-field struct) so the
/// publish helper's signature reads exactly like the intent: "the active
/// workspace is now *this*, or nothing".
pub type WorkspaceScope = Option<String>;

struct Channel {
    tx: mpsc::UnboundedSender<WorkspaceScope>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<WorkspaceScope>>>,
    /// Last-published scope, so a late-mounting consumer can read the current
    /// value on mount instead of draining the whole buffer.
    latch: Mutex<WorkspaceScope>,
}

fn channel() -> &'static Channel {
    static CH: OnceLock<Channel> = OnceLock::new();
    CH.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        Channel {
            tx,
            rx: Mutex::new(Some(rx)),
            latch: Mutex::new(None),
        }
    })
}

/// Announce the docked chat's active workspace scope (the Workspaces panel's
/// `enter_workspace` pushes `Some(id)`; `leave_workspace` pushes `None`).
///
/// Updates the last-value latch (so a panel mounting later can read it) and
/// buffers the value for the consumer's receiver. Always succeeds: a publish
/// with no consumer yet is buffered, not dropped.
pub fn publish_active_workspace(workspace_id: WorkspaceScope) {
    if let Ok(mut slot) = channel().latch.lock() {
        *slot = workspace_id.clone();
    }
    // `send` only errors if the receiver has been dropped; the docked panel is
    // a singleton that drains for its whole life, so this is a benign no-op in
    // the unreachable "consumer gone" case.
    let _ = channel().tx.send(workspace_id);
}

/// The most recently announced workspace scope, or `None` if nothing has been
/// published yet this session (which also reads as "unbound" — the correct
/// default for a dock that has not entered a workspace). A consumer reads this
/// on mount to seed its scope, then follows [`take_workspace_scope_receiver`]
/// for changes.
pub fn current_active_workspace() -> WorkspaceScope {
    channel().latch.lock().ok().and_then(|slot| slot.clone())
}

/// Take the receiver — the docked `ChatPanel` calls this **once** on mount and
/// drains it for the rest of its life. Returns `None` on a second call (the
/// dock is a singleton in the Shell's mounted-view cache), matching
/// [`crate::focus_bus::take_workspace_focus_receiver`].
pub fn take_workspace_scope_receiver() -> Option<mpsc::UnboundedReceiver<WorkspaceScope>> {
    channel().rx.lock().ok().and_then(|mut g| g.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The channel + latch are process-wide `OnceLock`s, and the mpsc receiver
    // can be taken exactly once for the whole binary. A single end-to-end test
    // therefore owns the global exclusively (the focus_bus precedent), covering
    // buffered delivery, the latch, leave-clears, post-mount delivery, and the
    // take-once guard in one ordered pass — no cross-test contention on the
    // single receiver.
    #[test]
    fn workspace_scope_lifecycle() {
        // An enter published before the consumer mounts is buffered (unbounded
        // mpsc), so the docked ChatPanel sees it when it drains on mount.
        publish_active_workspace(Some("ws-alpha".to_string()));
        assert_eq!(
            current_active_workspace().as_deref(),
            Some("ws-alpha"),
            "latch reflects the entered workspace",
        );

        // Leaving clears the scope; the latch tracks the most recent value.
        publish_active_workspace(None);
        assert_eq!(
            current_active_workspace(),
            None,
            "leave clears the latch back to unbound",
        );

        // The consumer takes the receiver and drains both buffered events in
        // publish order.
        let mut rx = take_workspace_scope_receiver().expect("receiver available once");
        assert_eq!(
            rx.try_recv().expect("first buffered scope"),
            Some("ws-alpha".to_string()),
        );
        assert_eq!(rx.try_recv().expect("second buffered scope"), None);

        // A scope published after the consumer is live still arrives.
        publish_active_workspace(Some("ws-beta".to_string()));
        assert_eq!(
            rx.try_recv().expect("post-mount scope"),
            Some("ws-beta".to_string()),
        );

        // The receiver can only be taken once (singleton consumer).
        assert!(
            take_workspace_scope_receiver().is_none(),
            "second take returns None",
        );
    }
}
