//! Cross-panel **active-file** latch — "which file is open in the Workspaces
//! editor right now".
//!
//! Companion to [`crate::workspace_scope_bus`] (active *workspace*) and
//! [`crate::conversation_bus`] (active *conversation*). This answers a fourth
//! question for RAG slice 2.5 (active-file boost): when the user has a file
//! open in the Workspaces editor, a turn sent on the docked chat should bias
//! retrieval toward that file. The producer (the Workspaces panel's
//! `open_in_editor` / enter / leave) and the consumer (the docked `ChatPanel`,
//! which reads the current value at turn-send) live in different panel crates,
//! so the shared signal belongs in `wylde-gui-pipe` — the one crate both
//! already depend on (the same dependency-graph reason the other buses cite).
//!
//! Shape: a **last-value latch only** — unlike the scope/conversation buses,
//! the consumer never needs to *react* to a change, only to read the *current*
//! open file synchronously when it sends a turn. So there is no mpsc channel,
//! no receiver, no single-consumer rule: just [`publish_active_file`] (the
//! editor opens a file; entering/leaving a workspace clears it) and
//! [`current_active_file`] (the dock reads it at send time). A missing latch /
//! `None` reads as "no file open", the correct default — the boost is then a
//! no-op and ranking stays pure cosine.

use std::sync::{Mutex, OnceLock};

/// The workspace-relative path of the file open in the editor, or `None` when
/// none is open (or a workspace switch cleared it).
pub type ActiveFile = Option<String>;

fn latch() -> &'static Mutex<ActiveFile> {
    static LATCH: OnceLock<Mutex<ActiveFile>> = OnceLock::new();
    LATCH.get_or_init(|| Mutex::new(None))
}

/// Set the active-file latch. The Workspaces panel publishes the opened file's
/// workspace-relative path on `open_in_editor`, and `None` on enter/leave so a
/// file open in one workspace never biases another workspace's turns. A blank
/// path is normalised to `None`.
pub fn publish_active_file(path: ActiveFile) {
    let cleaned = path
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    if let Ok(mut slot) = latch().lock() {
        *slot = cleaned;
    }
}

/// The file currently open in the Workspaces editor, or `None`. The docked
/// `ChatPanel` reads this when it sends a turn so the harness can fold it into
/// the retrieval query (2.5).
pub fn current_active_file() -> ActiveFile {
    latch().lock().ok().and_then(|slot| slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The latch is a process-wide `OnceLock`, so one ordered test owns it.
    #[test]
    fn active_file_latch_round_trips_and_clears() {
        publish_active_file(Some("services/x/foo.rs".to_owned()));
        assert_eq!(current_active_file().as_deref(), Some("services/x/foo.rs"));
        // A blank publish normalises to "no file open".
        publish_active_file(Some("   ".to_owned()));
        assert_eq!(current_active_file(), None, "blank clears the latch");
        // Re-set, then an explicit clear (workspace switch).
        publish_active_file(Some("a/b.rs".to_owned()));
        publish_active_file(None);
        assert_eq!(current_active_file(), None, "None clears the latch");
    }
}
