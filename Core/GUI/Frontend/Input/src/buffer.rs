//! Pure-Rust text buffer underneath `TextInput`.
//!
//! Separated from the View so editing semantics — cursor movement,
//! grapheme-aware backspace, selection arithmetic, undo ring — can be
//! tested without a gpui app, a window, or a focus handle in scope.
//!
//! Byte offsets, not char or grapheme indices.  Grapheme awareness only
//! kicks in when *moving* the cursor (left/right, word-jump, line-jump);
//! the cursor itself lives at a UTF-8-safe byte boundary at all times.
//!
//! Selection model:
//!   * `anchor` is `None` when there is no selection — the cursor sits
//!     at a single point.
//!   * `anchor = Some(x)` means the selection spans [min(cursor, anchor),
//!     max(cursor, anchor)).  Either edge can be the moving end.
//!
//! Undo model:
//!   * Snapshot taken before every mutating op that isn't a single
//!     character insertion mid-typing (those are coalesced into a single
//!     snapshot per "burst" delimited by a 600ms quiet period — but that
//!     timing logic lives in the View, the buffer just records snapshots
//!     when `push_snapshot` is called).
//!   * Ring buffer capped at `UNDO_LIMIT` entries (FIFO eviction).

use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;

/// Cap on snapshots kept in the undo + redo rings.
pub const UNDO_LIMIT: usize = 100;

/// A single point of state that can be restored by undo / redo.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

/// Editing buffer + cursor state for `TextInput`.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    /// `true` when the buffer is constrained to a single line — newline
    /// insertion is silently dropped.
    single_line: bool,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new(false)
    }
}

impl TextBuffer {
    pub fn new(single_line: bool) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            anchor: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            single_line,
        }
    }

    /// Construct a buffer pre-populated with `text`; cursor lands at the
    /// end.  Useful when adopting an existing String (e.g. the Chat
    /// panel's `prompt` field on first construction).
    pub fn with_text(text: impl Into<String>, single_line: bool) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            single_line,
        }
    }

    // ── Observers ───────────────────────────────────────────────────

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    pub fn is_single_line(&self) -> bool {
        self.single_line
    }

    pub fn has_selection(&self) -> bool {
        self.anchor
            .map(|a| a != self.cursor)
            .unwrap_or(false)
    }

    /// Selection bounds normalised to `(start, end)` with `start <= end`.
    /// `None` when there is no selection.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some(if a < self.cursor {
            (a, self.cursor)
        } else {
            (self.cursor, a)
        })
    }

    /// Selected text, if any.
    pub fn selected_text(&self) -> Option<&str> {
        let (start, end) = self.selection()?;
        Some(&self.text[start..end])
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Snapshot the current state into the undo ring and clear redo.
    /// Call before a mutation when the caller wants undo to roll back
    /// through that point.  Typed insertion bursts coalesce by skipping
    /// this between adjacent character inserts — the View arbitrates
    /// timing.
    pub fn push_snapshot(&mut self) {
        let snap = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        };
        // Skip if identical to the most recent snapshot — keeps the ring
        // from filling with repeats when the View accidentally calls
        // `push_snapshot` twice in a row.
        if self.undo.back() == Some(&snap) {
            return;
        }
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(snap);
        self.redo.clear();
    }

    /// Roll back one snapshot if available.  No-op when there's nothing
    /// to undo; the caller can ignore the return value.
    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop_back() else {
            return false;
        };
        let current = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        };
        if self.redo.len() == UNDO_LIMIT {
            self.redo.pop_front();
        }
        self.redo.push_back(current);
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.text.len());
        self.anchor = snap.anchor.map(|a| a.min(self.text.len()));
        true
    }

    /// Reapply one undone snapshot if available.
    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop_back() else {
            return false;
        };
        let current = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        };
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(current);
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.text.len());
        self.anchor = snap.anchor.map(|a| a.min(self.text.len()));
        true
    }

    // ── Cursor + selection moves ────────────────────────────────────

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    fn start_or_keep_anchor(&mut self, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
    }

    /// Move the cursor one grapheme cluster left.
    pub fn move_left(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = prev_grapheme_boundary(&self.text, self.cursor);
    }

    /// Move the cursor one grapheme cluster right.
    pub fn move_right(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = next_grapheme_boundary(&self.text, self.cursor);
    }

    /// Move the cursor to the start of the previous word.
    pub fn move_word_left(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = prev_word_boundary(&self.text, self.cursor);
    }

    /// Move the cursor to the end of the next word.
    pub fn move_word_right(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = next_word_boundary(&self.text, self.cursor);
    }

    /// Move the cursor to the start of the current line (multi-line) or
    /// the start of the buffer (single-line).
    pub fn move_line_start(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = line_start(&self.text, self.cursor);
    }

    /// Move the cursor to the end of the current line / buffer.
    pub fn move_line_end(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = line_end(&self.text, self.cursor);
    }

    /// Move the cursor up one line preserving column when possible.
    /// No-op for single-line buffers.
    pub fn move_up(&mut self, extend: bool) {
        if self.single_line {
            self.move_line_start(extend);
            return;
        }
        self.start_or_keep_anchor(extend);
        self.cursor = move_up(&self.text, self.cursor);
    }

    /// Move the cursor down one line preserving column when possible.
    /// No-op for single-line buffers.
    pub fn move_down(&mut self, extend: bool) {
        if self.single_line {
            self.move_line_end(extend);
            return;
        }
        self.start_or_keep_anchor(extend);
        self.cursor = move_down(&self.text, self.cursor);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Position the cursor at a byte offset, clamped to a UTF-8 boundary.
    /// `extend = true` extends the existing selection (or starts one);
    /// `false` clears it.  Used by mouse clicks that land on a character
    /// `div`.
    pub fn set_cursor(&mut self, offset: usize, extend: bool) {
        let target = clamp_to_boundary(&self.text, offset);
        self.start_or_keep_anchor(extend);
        self.cursor = target;
    }

    // ── Mutations ───────────────────────────────────────────────────

    /// Replace the selection (or insert at the cursor) with `s`.  Honours
    /// `single_line` by stripping embedded newlines.
    ///
    /// Returns the actually inserted string (after newline stripping) so
    /// the View can record an exact-replay snapshot if it wants.
    pub fn insert_str(&mut self, s: &str) -> String {
        let payload = if self.single_line {
            // Treat CR / CRLF / LF the same — drop them all.  Mirrors
            // every other text input on every platform.
            s.replace(['\r', '\n'], "")
        } else {
            // Multi-line: normalise CRLF → LF; drop bare CR.  Same shape
            // a paste from a Windows clipboard tends to land in.
            s.replace("\r\n", "\n").replace('\r', "")
        };
        if payload.is_empty() && !s.is_empty() {
            // Single-line buffer eating a pure-newline paste — still
            // clear the selection if there was one.  Matches the behaviour
            // a user expects: hitting Enter clears the selection even
            // when the line stays unchanged.
            self.delete_selection();
            return String::new();
        }
        if let Some((start, end)) = self.selection() {
            self.text.replace_range(start..end, &payload);
            self.cursor = start + payload.len();
        } else {
            self.text.insert_str(self.cursor, &payload);
            self.cursor += payload.len();
        }
        self.anchor = None;
        payload
    }

    /// Delete the selection if any, otherwise the grapheme behind the
    /// cursor.  Returns `true` if anything actually changed.
    pub fn backspace(&mut self) -> bool {
        if self.delete_selection().is_some() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let new_cursor = prev_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
        true
    }

    /// Delete the selection if any, otherwise the grapheme in front of
    /// the cursor.  Returns `true` if anything changed.
    pub fn delete_forward(&mut self) -> bool {
        if self.delete_selection().is_some() {
            return true;
        }
        if self.cursor == self.text.len() {
            return false;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        true
    }

    /// Delete from the cursor back to the previous word boundary.
    pub fn delete_word_left(&mut self) -> bool {
        if self.delete_selection().is_some() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let target = prev_word_boundary(&self.text, self.cursor);
        if target == self.cursor {
            return false;
        }
        self.text.replace_range(target..self.cursor, "");
        self.cursor = target;
        true
    }

    /// Delete from the cursor forward to the next word boundary.
    pub fn delete_word_right(&mut self) -> bool {
        if self.delete_selection().is_some() {
            return true;
        }
        if self.cursor == self.text.len() {
            return false;
        }
        let target = next_word_boundary(&self.text, self.cursor);
        if target == self.cursor {
            return false;
        }
        self.text.replace_range(self.cursor..target, "");
        true
    }

    /// Delete the current selection if any; returns the removed text.
    pub fn delete_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection()?;
        let removed = self.text[start..end].to_owned();
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.anchor = None;
        Some(removed)
    }

    /// Replace the entire buffer.  Used when wiring an external setter
    /// (e.g. the Memory panel programmatically clearing the search box).
    pub fn set_text(&mut self, s: impl Into<String>) {
        let s = s.into();
        let normalised = if self.single_line {
            s.replace(['\r', '\n'], "")
        } else {
            s.replace("\r\n", "\n").replace('\r', "")
        };
        self.text = normalised;
        self.cursor = self.text.len();
        self.anchor = None;
    }
}

// ── Boundary helpers ─────────────────────────────────────────────────

fn clamp_to_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    // Walk backwards to the nearest char boundary.  UTF-8 leading bytes
    // are 0xxxxxxx or 11xxxxxx; continuation bytes are 10xxxxxx.
    let bytes = text.as_bytes();
    let mut idx = offset;
    while idx > 0 && (bytes[idx] & 0xC0) == 0x80 {
        idx -= 1;
    }
    idx
}

fn prev_grapheme_boundary(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    // `grapheme_indices` enumerates `(byte_offset, grapheme_str)`; the
    // boundary we want is the start of whichever grapheme contains
    // `offset - 1`.
    let mut prev = 0;
    for (idx, _) in text.grapheme_indices(true) {
        if idx >= offset {
            return prev;
        }
        prev = idx;
    }
    prev
}

fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    for (idx, _) in text.grapheme_indices(true) {
        if idx > offset {
            return idx;
        }
    }
    text.len()
}

/// Word boundary in the "skip whitespace, then skip non-whitespace"
/// sense most text editors use for ctrl+left.
fn prev_word_boundary(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    // Iterate UTF-8 chars from `offset` walking backwards.
    let mut idx = offset;
    let bytes = text.as_bytes();

    // Step 1: skip whitespace immediately before the cursor.
    while idx > 0 {
        let prev = prev_char_boundary(bytes, idx);
        let c = text[prev..idx].chars().next().unwrap();
        if c.is_whitespace() {
            idx = prev;
        } else {
            break;
        }
    }

    // Step 2: skip non-whitespace until we hit whitespace or BOS.
    while idx > 0 {
        let prev = prev_char_boundary(bytes, idx);
        let c = text[prev..idx].chars().next().unwrap();
        if c.is_whitespace() {
            break;
        }
        idx = prev;
    }
    idx
}

fn next_word_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let bytes = text.as_bytes();
    let mut idx = offset;

    // Step 1: skip whitespace from the cursor forward.
    while idx < bytes.len() {
        let next = next_char_boundary(bytes, idx);
        let c = text[idx..next].chars().next().unwrap();
        if !c.is_whitespace() {
            break;
        }
        idx = next;
    }

    // Step 2: skip non-whitespace until we hit whitespace or EOS.
    while idx < bytes.len() {
        let next = next_char_boundary(bytes, idx);
        let c = text[idx..next].chars().next().unwrap();
        if c.is_whitespace() {
            break;
        }
        idx = next;
    }
    idx
}

fn prev_char_boundary(bytes: &[u8], offset: usize) -> usize {
    let mut idx = offset.saturating_sub(1);
    while idx > 0 && (bytes[idx] & 0xC0) == 0x80 {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(bytes: &[u8], offset: usize) -> usize {
    let mut idx = offset + 1;
    while idx < bytes.len() && (bytes[idx] & 0xC0) == 0x80 {
        idx += 1;
    }
    idx.min(bytes.len())
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn line_end(text: &str, offset: usize) -> usize {
    text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len())
}

fn move_up(text: &str, offset: usize) -> usize {
    let line_a = line_start(text, offset);
    if line_a == 0 {
        // Already on the first line.
        return 0;
    }
    let col = offset - line_a;
    // Previous line.
    let line_b_end = line_a - 1; // position of the '\n' character
    let line_b_start = text[..line_b_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_b_len = line_b_end - line_b_start;
    let target = line_b_start + col.min(line_b_len);
    clamp_to_boundary(text, target)
}

fn move_down(text: &str, offset: usize) -> usize {
    let line_a = line_start(text, offset);
    let line_a_end = line_end(text, offset);
    let col = offset - line_a;
    if line_a_end == text.len() {
        return text.len();
    }
    let line_c_start = line_a_end + 1;
    let line_c_end = line_end(text, line_c_start);
    let line_c_len = line_c_end - line_c_start;
    let target = line_c_start + col.min(line_c_len);
    clamp_to_boundary(text, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_buffer_is_empty() {
        let b = TextBuffer::default();
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
        assert!(b.anchor().is_none());
    }

    #[test]
    fn insert_str_appends_at_cursor() {
        let mut b = TextBuffer::default();
        b.insert_str("hello");
        assert_eq!(b.text(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn backspace_deletes_grapheme() {
        let mut b = TextBuffer::with_text("héllo", false);
        b.backspace();
        b.backspace();
        b.backspace();
        b.backspace();
        b.backspace();
        assert!(b.is_empty());
    }

    #[test]
    fn backspace_handles_multibyte_chars() {
        // 'é' is two bytes (0xc3 0xa9) — make sure backspace removes
        // both, not just one.
        let mut b = TextBuffer::with_text("é", false);
        assert_eq!(b.cursor(), 2);
        b.backspace();
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn move_left_right_traverse_graphemes() {
        let mut b = TextBuffer::with_text("héllo", false);
        b.move_left(false);
        assert_eq!(b.cursor(), 5); // after "héll"
        b.move_left(false);
        assert_eq!(b.cursor(), 4); // after "hél"
    }

    #[test]
    fn shift_arrow_starts_then_extends_selection() {
        let mut b = TextBuffer::with_text("hello world", false);
        // Cursor is at 11 (end).
        b.move_left(true);
        b.move_left(true);
        b.move_left(true);
        b.move_left(true);
        b.move_left(true);
        assert_eq!(b.selected_text(), Some("world"));
    }

    #[test]
    fn arrow_without_shift_collapses_selection() {
        let mut b = TextBuffer::with_text("hello", false);
        b.select_all();
        assert!(b.has_selection());
        b.move_left(false);
        assert!(!b.has_selection());
    }

    #[test]
    fn delete_selection_returns_removed_text() {
        let mut b = TextBuffer::with_text("hello world", false);
        b.set_cursor(0, false);
        b.set_cursor(5, true); // select "hello"
        let cut = b.delete_selection().unwrap();
        assert_eq!(cut, "hello");
        assert_eq!(b.text(), " world");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn insert_with_selection_replaces_it() {
        let mut b = TextBuffer::with_text("hello world", false);
        b.set_cursor(6, false);
        b.set_cursor(11, true); // select "world"
        b.insert_str("there");
        assert_eq!(b.text(), "hello there");
        assert_eq!(b.cursor(), 11);
        assert!(!b.has_selection());
    }

    #[test]
    fn select_all_spans_buffer() {
        let mut b = TextBuffer::with_text("abc", false);
        b.select_all();
        assert_eq!(b.selection(), Some((0, 3)));
        assert_eq!(b.selected_text(), Some("abc"));
    }

    #[test]
    fn word_left_skips_whitespace_then_word() {
        let mut b = TextBuffer::with_text("hello world foo", false);
        // Cursor at end (15).
        b.move_word_left(false);
        assert_eq!(b.cursor(), 12); // start of "foo"
        b.move_word_left(false);
        assert_eq!(b.cursor(), 6); // start of "world"
        b.move_word_left(false);
        assert_eq!(b.cursor(), 0); // start of "hello"
    }

    #[test]
    fn word_right_jumps_to_end_of_word() {
        let mut b = TextBuffer::with_text("hello world", false);
        b.set_cursor(0, false);
        b.move_word_right(false);
        assert_eq!(b.cursor(), 5); // end of "hello"
        b.move_word_right(false);
        assert_eq!(b.cursor(), 11); // end of "world"
    }

    #[test]
    fn line_start_end_on_multiline() {
        let mut b = TextBuffer::with_text("one\ntwo\nthree", false);
        b.set_cursor(5, false); // middle of "two"
        b.move_line_start(false);
        assert_eq!(b.cursor(), 4); // start of "two"
        b.move_line_end(false);
        assert_eq!(b.cursor(), 7); // end of "two"
    }

    #[test]
    fn move_up_preserves_column() {
        let mut b = TextBuffer::with_text("hello\nworld", false);
        b.set_cursor(8, false); // "wor|ld"
        b.move_up(false);
        assert_eq!(b.cursor(), 2); // "he|llo"
    }

    #[test]
    fn move_up_clamps_column_to_short_line() {
        let mut b = TextBuffer::with_text("hi\nworld", false);
        b.set_cursor(7, false); // "worl|d" — col 4
        b.move_up(false);
        // Previous line "hi" only has 2 chars; clamp to its end.
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn move_up_on_first_line_goes_to_zero() {
        let mut b = TextBuffer::with_text("hello", false);
        b.set_cursor(3, false);
        b.move_up(false);
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn move_down_preserves_column() {
        let mut b = TextBuffer::with_text("hello\nworld", false);
        b.set_cursor(3, false); // "hel|lo"
        b.move_down(false);
        assert_eq!(b.cursor(), 9); // "wor|ld"
    }

    #[test]
    fn single_line_buffer_strips_newlines_on_insert() {
        let mut b = TextBuffer::new(true);
        b.insert_str("foo\nbar");
        assert_eq!(b.text(), "foobar");
    }

    #[test]
    fn multi_line_buffer_keeps_newlines_on_insert() {
        let mut b = TextBuffer::new(false);
        b.insert_str("foo\nbar");
        assert_eq!(b.text(), "foo\nbar");
    }

    #[test]
    fn multi_line_normalises_crlf_to_lf() {
        let mut b = TextBuffer::new(false);
        b.insert_str("foo\r\nbar\r\nbaz");
        assert_eq!(b.text(), "foo\nbar\nbaz");
    }

    #[test]
    fn undo_rolls_back_one_op() {
        let mut b = TextBuffer::default();
        b.insert_str("hello");
        b.push_snapshot();
        b.insert_str(" world");
        assert_eq!(b.text(), "hello world");
        assert!(b.undo());
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn redo_reapplies_undone_op() {
        let mut b = TextBuffer::default();
        b.insert_str("hello");
        b.push_snapshot();
        b.insert_str(" world");
        b.undo();
        assert!(b.redo());
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn fresh_mutation_clears_redo() {
        let mut b = TextBuffer::default();
        b.insert_str("a");
        b.push_snapshot();
        b.insert_str("b");
        b.undo();
        b.push_snapshot();
        b.insert_str("c");
        assert!(!b.redo());
    }

    #[test]
    fn snapshot_is_deduplicated() {
        let mut b = TextBuffer::default();
        // Three identical snapshots in a row should collapse to one.
        b.push_snapshot();
        b.push_snapshot();
        b.push_snapshot();
        // Mutate so undo has something distinguishable to roll back to.
        b.insert_str("x");
        // Exactly one undo should be possible — the duplicate snapshots
        // didn't fill the ring.
        assert!(b.undo());
        assert!(b.is_empty());
        assert!(!b.undo(), "second undo should fail; only one snapshot was kept");
    }

    #[test]
    fn undo_ring_caps_at_limit() {
        let mut b = TextBuffer::default();
        for i in 0..(UNDO_LIMIT + 50) {
            b.insert_str(&format!("{i}"));
            b.push_snapshot();
        }
        // After UNDO_LIMIT undos the next call returns false — we can't
        // pop snapshots that were evicted to keep the ring bounded.
        let mut undone = 0;
        while b.undo() {
            undone += 1;
        }
        assert!(undone <= UNDO_LIMIT);
    }

    #[test]
    fn set_cursor_clamps_to_utf8_boundary() {
        let mut b = TextBuffer::with_text("é", false); // 2 bytes
        b.set_cursor(1, false); // mid-codepoint
        // Cursor lands on 0, the safe boundary.
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn set_cursor_past_end_clamps_to_end() {
        let mut b = TextBuffer::with_text("hi", false);
        b.set_cursor(99, false);
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn set_cursor_extend_starts_selection_from_anchor() {
        let mut b = TextBuffer::with_text("hello", false);
        b.set_cursor(0, false);
        b.set_cursor(4, true);
        assert_eq!(b.selected_text(), Some("hell"));
    }

    #[test]
    fn delete_word_left_removes_token() {
        let mut b = TextBuffer::with_text("hello world", false);
        // Cursor at end.
        b.delete_word_left();
        assert_eq!(b.text(), "hello ");
    }

    #[test]
    fn delete_word_right_removes_token() {
        let mut b = TextBuffer::with_text("hello world", false);
        b.set_cursor(0, false);
        b.delete_word_right();
        assert_eq!(b.text(), " world");
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut b = TextBuffer::with_text("hi", false);
        assert!(!b.delete_forward());
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut b = TextBuffer::default();
        b.set_cursor(0, false);
        assert!(!b.backspace());
    }

    #[test]
    fn set_text_replaces_buffer_and_resets_cursor() {
        let mut b = TextBuffer::with_text("old", false);
        b.set_text("new");
        assert_eq!(b.text(), "new");
        assert_eq!(b.cursor(), 3);
        assert!(b.anchor().is_none());
    }

    #[test]
    fn single_line_set_text_strips_newlines() {
        let mut b = TextBuffer::new(true);
        b.set_text("a\nb");
        assert_eq!(b.text(), "ab");
    }
}
