//! The per-conversation undo stack (Plan v2 §5.9) — Ctrl+Z / Ctrl+Shift+Z
//! (Ctrl+Y) across every anchor surface.
//!
//! Generic over the action type: each consumer pushes an entry carrying the
//! human label (for the "… — undone." toast) and whatever inverse data it
//! needs to apply the undo. Redo re-applies. Depth-capped at
//! [`DEFAULT_DEPTH`] (50, configurable per stack) — pushing past the cap
//! drops the oldest entry; any new edit clears the redo branch (the
//! standard linear-history rule).

/// Plan §5.9's default stack depth.
pub const DEFAULT_DEPTH: usize = 50;

/// One undoable action: a display label plus the consumer's payload.
#[derive(Clone, Debug, PartialEq)]
pub struct UndoEntry<T> {
    /// Toast text fragment, e.g. `"Connection saved to workspace"`.
    pub label: String,
    pub action: T,
    /// Position on a shared undo timeline (0 when the consumer doesn't
    /// interleave). A unified arbiter (the chat panel's §5.9 Ctrl+Z)
    /// stamps via [`UndoStack::push_seq`] and compares
    /// [`UndoStack::top_seq`] against sibling stacks to undo strictly by
    /// recency.
    pub seq: u64,
}

/// A bounded undo/redo stack.
#[derive(Clone, Debug)]
pub struct UndoStack<T> {
    depth: usize,
    undo: Vec<UndoEntry<T>>,
    redo: Vec<UndoEntry<T>>,
}

impl<T> Default for UndoStack<T> {
    fn default() -> Self {
        Self::with_depth(DEFAULT_DEPTH)
    }
}

impl<T> UndoStack<T> {
    pub fn with_depth(depth: usize) -> Self {
        UndoStack {
            depth: depth.max(1),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Record a fresh action. Clears the redo branch; drops the oldest entry
    /// past the depth cap.
    pub fn push(&mut self, label: impl Into<String>, action: T) {
        self.push_seq(label, action, 0);
    }

    /// [`Self::push`] with a shared-timeline stamp (unified undo, §5.9).
    pub fn push_seq(&mut self, label: impl Into<String>, action: T, seq: u64) {
        self.redo.clear();
        self.undo.push(UndoEntry {
            label: label.into(),
            action,
            seq,
        });
        if self.undo.len() > self.depth {
            self.undo.remove(0);
        }
    }

    /// Timeline stamp of the newest undoable entry.
    pub fn top_seq(&self) -> Option<u64> {
        self.undo.last().map(|e| e.seq)
    }

    /// Timeline stamp of the newest redoable entry.
    pub fn top_redo_seq(&self) -> Option<u64> {
        self.redo.last().map(|e| e.seq)
    }

    /// Drop only the redo branch — the unified timeline's linear-history
    /// rule when a NEW op lands on a sibling stack.
    pub fn clear_redo(&mut self) {
        self.redo.clear();
    }

    /// Ctrl+Z: pop the latest action (the caller applies its inverse, then
    /// shows the `"<label> — undone."` toast). Moves it to the redo branch.
    pub fn undo(&mut self) -> Option<&UndoEntry<T>>
    where
        T: Clone,
    {
        let entry = self.undo.pop()?;
        self.redo.push(entry);
        self.redo.last()
    }

    /// Ctrl+Shift+Z / Ctrl+Y: re-apply the latest undone action.
    pub fn redo(&mut self) -> Option<&UndoEntry<T>>
    where
        T: Clone,
    {
        let entry = self.redo.pop()?;
        self.undo.push(entry);
        self.undo.last()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// A conversation switch starts a fresh history.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_round_trip_with_labels() {
        let mut s: UndoStack<i32> = UndoStack::default();
        s.push("Connection saved to workspace", 1);
        s.push("Anchor excluded", 2);
        assert!(s.can_undo() && !s.can_redo());

        let u = s.undo().unwrap();
        assert_eq!(u.action, 2);
        assert_eq!(u.label, "Anchor excluded");
        assert!(s.can_redo());

        let r = s.redo().unwrap();
        assert_eq!(r.action, 2);
        assert!(!s.can_redo());
    }

    #[test]
    fn new_edit_clears_redo_branch() {
        let mut s: UndoStack<i32> = UndoStack::default();
        s.push("a", 1);
        s.push("b", 2);
        s.undo();
        s.push("c", 3);
        assert!(!s.can_redo(), "linear history — redo branch dropped");
        assert_eq!(s.undo().unwrap().action, 3);
        assert_eq!(s.undo().unwrap().action, 1);
    }

    #[test]
    fn seq_stamps_ride_the_timeline_and_survive_undo() {
        let mut s: UndoStack<i32> = UndoStack::default();
        s.push_seq("a", 1, 10);
        s.push_seq("b", 2, 20);
        assert_eq!(s.top_seq(), Some(20));
        assert_eq!(s.top_redo_seq(), None);
        // Undo moves the stamp to the redo branch unchanged.
        s.undo();
        assert_eq!(s.top_seq(), Some(10));
        assert_eq!(s.top_redo_seq(), Some(20));
        // clear_redo drops ONLY the redo branch (sibling-stack op landed).
        s.clear_redo();
        assert_eq!(s.top_redo_seq(), None);
        assert_eq!(s.top_seq(), Some(10), "undo branch untouched");
        // Plain push stamps 0 (non-interleaving consumers).
        s.push("c", 3);
        assert_eq!(s.top_seq(), Some(0));
    }

    #[test]
    fn depth_cap_drops_oldest_and_clear_resets() {
        let mut s: UndoStack<usize> = UndoStack::with_depth(3);
        for i in 0..5 {
            s.push(format!("op {i}"), i);
        }
        assert_eq!(s.undo().unwrap().action, 4);
        assert_eq!(s.undo().unwrap().action, 3);
        assert_eq!(s.undo().unwrap().action, 2);
        assert!(!s.can_undo(), "0 and 1 fell off the cap");

        s.clear();
        assert!(!s.can_undo() && !s.can_redo());
    }
}
