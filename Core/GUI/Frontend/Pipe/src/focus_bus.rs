//! Cross-panel **focus** bus — the typed deep-link companion to [`nav_bus`].
//!
//! `nav_bus::request_nav` only selects a *panel*. The IDE deep-link (S7) needs
//! to additionally target a *tab within* the Workspaces panel and focus a graph
//! node — e.g. "click a vocab word in the InferenceBar → open the Workspaces
//! Graph tab centred on that symbol". The InferenceBar lives in the Chat panel,
//! so this crosses a panel boundary.
//!
//! Shape (mirrors `nav_bus`, inverted consumer): the **Workspaces panel** is
//! the consumer, so it drains the receiver; any panel (the composer) pushes a
//! [`WorkspaceFocus`]. The channel is an unbounded mpsc created lazily on first
//! use, so a focus pushed *before* the Workspaces panel has ever mounted is
//! **buffered** and delivered the moment the panel starts draining — no Shell
//! coupling, no lost message, no polling.
//!
//! Lives in `wylde-gui-pipe` for the same reason `nav_bus` does: it is the one
//! crate both the producing and consuming panels already depend on, avoiding a
//! registry↔panel dependency cycle.

use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;

/// A request to focus something inside the Workspaces panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceFocus {
    /// Tab to select (a panel-specific string key: `"graph"`, `"editor"`,
    /// `"files"`, …). `None` leaves the current tab as-is.
    pub tab: Option<String>,
    /// A graph node / symbol id to focus (drives `GraphView::focus_node`).
    /// `None` selects the tab without focusing a node.
    pub node_id: Option<String>,
}

struct Channel {
    tx: mpsc::UnboundedSender<WorkspaceFocus>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<WorkspaceFocus>>>,
}

fn channel() -> &'static Channel {
    static CH: OnceLock<Channel> = OnceLock::new();
    CH.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        Channel {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    })
}

/// Push a focus request (e.g. the composer's "view in graph" affordance).
/// Buffered until the Workspaces panel drains it; always succeeds.
pub fn request_workspace_focus(focus: WorkspaceFocus) {
    let _ = channel().tx.send(focus);
}

/// Take the receiver — the Workspaces panel calls this **once** on mount and
/// drains it for the rest of its life. Returns `None` on a second call (the
/// panel is a singleton in the Shell's mounted-view cache).
pub fn take_workspace_focus_receiver() -> Option<mpsc::UnboundedReceiver<WorkspaceFocus>> {
    channel().rx.lock().ok().and_then(|mut g| g.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_drain_delivers_buffered_focus() {
        // A focus pushed before the receiver is taken is still delivered.
        request_workspace_focus(WorkspaceFocus {
            tab: Some("graph".into()),
            node_id: Some("sym::foo".into()),
        });
        let mut rx = take_workspace_focus_receiver().expect("receiver available once");
        let got = rx.try_recv().expect("buffered focus delivered");
        assert_eq!(got.tab.as_deref(), Some("graph"));
        assert_eq!(got.node_id.as_deref(), Some("sym::foo"));
        // The receiver can only be taken once.
        assert!(take_workspace_focus_receiver().is_none());
    }
}
