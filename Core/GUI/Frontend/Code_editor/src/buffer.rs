//! Pure-Rust editing buffer underneath the code editor.
//!
//! Adapted from `wylde-gpui-input`'s proven `TextBuffer` (grapheme-aware
//! cursor, byte-offset addressing, selection arithmetic, snapshot undo) — but
//! a **sibling, independent** implementation (OQ-3 = Option B: do NOT wrap the
//! input crate). Differences from the input buffer, tuned for code:
//!   * always multi-line (no `single_line` mode),
//!   * line-addressing helpers (`line_count`, `offset_of_line`,
//!     `line_col_of`) the gutter, scroll-to-line, and cull math need,
//!   * `insert_newline` with leading-whitespace auto-indent,
//!   * `insert_tab` honouring a configurable indent width / soft tabs.
//!
//! Byte offsets, not char/grapheme indices. The cursor always sits on a
//! UTF-8 boundary. Separated from the gpui View so every editing rule is
//! testable without a window or focus handle.

use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;

/// Cap on snapshots kept in the undo + redo rings (FIFO eviction).
pub const UNDO_LIMIT: usize = 200;

/// A restorable point of editing state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

/// Editing buffer + cursor/selection + undo for the code editor.
///
/// Selection model mirrors the input buffer: `anchor = None` is a caret;
/// `anchor = Some(x)` spans `[min(cursor,anchor), max(cursor,anchor))`.
#[derive(Debug, Clone)]
pub struct EditBuffer {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
}

impl Default for EditBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl EditBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            anchor: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        }
    }

    pub fn with_text(text: impl Into<String>) -> Self {
        let text = normalise(&text.into());
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        }
    }

    // ── Observers ───────────────────────────────────────────────────────

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn has_selection(&self) -> bool {
        self.anchor.map(|a| a != self.cursor).unwrap_or(false)
    }

    /// Selection bounds normalised to `(start, end)` with `start <= end`, or
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

    pub fn selected_text(&self) -> Option<&str> {
        let (s, e) = self.selection()?;
        Some(&self.text[s..e])
    }

    // ── Line addressing (code-editor extensions) ────────────────────────

    /// Number of logical lines (always ≥ 1). A trailing newline yields a
    /// final empty line the caret can sit on.
    pub fn line_count(&self) -> usize {
        self.text.bytes().filter(|b| *b == b'\n').count() + 1
    }

    /// Byte offset where logical line `line` (0-based) starts, clamped to the
    /// last line.
    pub fn offset_of_line(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0usize;
        for (i, b) in self.text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == line {
                    return i + 1;
                }
            }
        }
        self.text.len()
    }

    /// `(line, column)` for a byte `offset`, both 0-based; column is a byte
    /// offset within the line.
    pub fn line_col_of(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.text.len());
        let line = self.text[..offset].bytes().filter(|b| *b == b'\n').count();
        let col = offset - line_start(&self.text, offset);
        (line, col)
    }

    /// Byte offset of the caret's current line start — used for indent
    /// detection.
    pub fn current_line_start(&self) -> usize {
        line_start(&self.text, self.cursor)
    }

    // ── Undo ────────────────────────────────────────────────────────────

    /// Snapshot current state into the undo ring (clears redo). The View
    /// coalesces typing bursts by calling this once per burst.
    pub fn push_snapshot(&mut self) {
        if self.undo.back().is_some_and(|s| {
            s.text == self.text && s.cursor == self.cursor && s.anchor == self.anchor
        }) {
            return;
        }
        let snap = Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        };
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(snap);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

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
        self.restore(snap);
        true
    }

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
        self.restore(snap);
        true
    }

    fn restore(&mut self, snap: Snapshot) {
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.text.len());
        self.anchor = snap.anchor.map(|a| a.min(self.text.len()));
    }

    // ── Cursor + selection moves ────────────────────────────────────────

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

    pub fn move_left(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = prev_grapheme_boundary(&self.text, self.cursor);
    }
    pub fn move_right(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = next_grapheme_boundary(&self.text, self.cursor);
    }
    pub fn move_word_left(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = prev_word_boundary(&self.text, self.cursor);
    }
    pub fn move_word_right(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = next_word_boundary(&self.text, self.cursor);
    }
    pub fn move_line_start(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = line_start(&self.text, self.cursor);
    }
    pub fn move_line_end(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = line_end(&self.text, self.cursor);
    }
    pub fn move_up(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = move_up(&self.text, self.cursor);
    }
    pub fn move_down(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = move_down(&self.text, self.cursor);
    }
    pub fn move_document_start(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = 0;
    }
    pub fn move_document_end(&mut self, extend: bool) {
        self.start_or_keep_anchor(extend);
        self.cursor = self.text.len();
    }

    /// Move the caret up/down `rows` lines (page scroll), preserving column.
    pub fn move_vertical(&mut self, rows: i32, extend: bool) {
        self.start_or_keep_anchor(extend);
        let mut c = self.cursor;
        if rows >= 0 {
            for _ in 0..rows {
                c = move_down(&self.text, c);
            }
        } else {
            for _ in 0..(-rows) {
                c = move_up(&self.text, c);
            }
        }
        self.cursor = c;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Place the cursor at a byte offset (clamped to a UTF-8 boundary).
    pub fn set_cursor(&mut self, offset: usize, extend: bool) {
        let target = clamp_to_boundary(&self.text, offset);
        self.start_or_keep_anchor(extend);
        self.cursor = target;
    }

    // ── Mutations ───────────────────────────────────────────────────────

    /// Replace the selection (or insert at the cursor) with `s`. CRLF/CR are
    /// normalised to LF. Returns the actually-inserted string.
    pub fn insert_str(&mut self, s: &str) -> String {
        let payload = normalise(s);
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

    /// Insert a newline, carrying the current line's leading whitespace as the
    /// new line's indent (auto-indent). Returns the inserted string.
    pub fn insert_newline(&mut self) -> String {
        // Indent is read from the caret's line *before* any selection delete.
        let line_begin = line_start(&self.text, active_start(self));
        let indent: String = self.text[line_begin..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let payload = format!("\n{indent}");
        self.insert_str(&payload)
    }

    /// Insert one indent step at the caret. Soft tabs (`soft`) insert `width`
    /// spaces; hard tabs insert a single `\t`.
    pub fn insert_tab(&mut self, soft: bool, width: usize) -> String {
        if soft {
            let pad = " ".repeat(width.max(1));
            self.insert_str(&pad)
        } else {
            self.insert_str("\t")
        }
    }

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

    pub fn delete_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection()?;
        let removed = self.text[start..end].to_owned();
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.anchor = None;
        Some(removed)
    }

    /// Replace the whole buffer; cursor lands at the end, selection cleared.
    pub fn set_text(&mut self, s: impl Into<String>) {
        self.text = normalise(&s.into());
        self.cursor = self.cursor.min(self.text.len());
        self.anchor = None;
    }
}

/// Caret's active position (cursor), used for indent detection at newline.
fn active_start(b: &EditBuffer) -> usize {
    match b.selection() {
        Some((s, _)) => s,
        None => b.cursor,
    }
}

/// Normalise CRLF → LF and drop bare CR — the shape a Windows paste lands in.
fn normalise(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "")
}

// ── Boundary helpers (ported from the proven input buffer) ──────────────

fn clamp_to_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
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

fn prev_word_boundary(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut idx = offset;
    let bytes = text.as_bytes();
    while idx > 0 {
        let prev = prev_char_boundary(bytes, idx);
        let c = text[prev..idx].chars().next().unwrap();
        if c.is_whitespace() {
            idx = prev;
        } else {
            break;
        }
    }
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
    while idx < bytes.len() {
        let next = next_char_boundary(bytes, idx);
        let c = text[idx..next].chars().next().unwrap();
        if !c.is_whitespace() {
            break;
        }
        idx = next;
    }
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
        return 0;
    }
    let col = offset - line_a;
    let line_b_end = line_a - 1;
    let line_b_start = text[..line_b_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_b_len = line_b_end - line_b_start;
    clamp_to_boundary(text, line_b_start + col.min(line_b_len))
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
    clamp_to_boundary(text, line_c_start + col.min(line_c_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_cursor() {
        let mut b = EditBuffer::new();
        b.insert_str("hello");
        assert_eq!(b.text(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn crlf_normalised_on_insert_and_set_text() {
        let mut b = EditBuffer::new();
        b.insert_str("a\r\nb\rc");
        assert_eq!(b.text(), "a\nbc");
        b.set_text("x\r\ny");
        assert_eq!(b.text(), "x\ny");
    }

    #[test]
    fn line_count_and_offsets() {
        let b = EditBuffer::with_text("one\ntwo\nthree");
        assert_eq!(b.line_count(), 3);
        assert_eq!(b.offset_of_line(0), 0);
        assert_eq!(b.offset_of_line(1), 4);
        assert_eq!(b.offset_of_line(2), 8);
        // Past the end clamps to len.
        assert_eq!(b.offset_of_line(99), b.len());
    }

    #[test]
    fn trailing_newline_adds_a_final_line() {
        let b = EditBuffer::with_text("a\n");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.offset_of_line(1), 2);
    }

    #[test]
    fn line_col_of_maps_back() {
        let b = EditBuffer::with_text("ab\ncde");
        assert_eq!(b.line_col_of(0), (0, 0));
        assert_eq!(b.line_col_of(2), (0, 2));
        assert_eq!(b.line_col_of(4), (1, 1)); // 'd'
    }

    #[test]
    fn newline_auto_indents() {
        let mut b = EditBuffer::with_text("    foo");
        b.set_cursor(7, false); // end of "    foo"
        b.insert_newline();
        assert_eq!(b.text(), "    foo\n    ");
        assert_eq!(b.cursor(), b.len());
    }

    #[test]
    fn newline_indent_uses_line_under_caret_not_eof() {
        let mut b = EditBuffer::with_text("  a\nbbbb");
        b.set_cursor(3, false); // end of "  a"
        b.insert_newline();
        assert_eq!(b.text(), "  a\n  \nbbbb");
    }

    #[test]
    fn soft_and_hard_tab() {
        let mut b = EditBuffer::new();
        b.insert_tab(true, 4);
        assert_eq!(b.text(), "    ");
        let mut h = EditBuffer::new();
        h.insert_tab(false, 4);
        assert_eq!(h.text(), "\t");
    }

    #[test]
    fn grapheme_backspace_multibyte() {
        let mut b = EditBuffer::with_text("é");
        assert_eq!(b.cursor(), 2);
        b.backspace();
        assert!(b.is_empty());
    }

    #[test]
    fn selection_replace_and_delete() {
        let mut b = EditBuffer::with_text("hello world");
        b.set_cursor(6, false);
        b.set_cursor(11, true);
        assert_eq!(b.selected_text(), Some("world"));
        b.insert_str("there");
        assert_eq!(b.text(), "hello there");
        assert!(!b.has_selection());
    }

    #[test]
    fn vertical_move_preserves_column() {
        let mut b = EditBuffer::with_text("hello\nworld");
        b.set_cursor(3, false); // hel|lo
        b.move_down(false);
        assert_eq!(b.cursor(), 9); // wor|ld
        b.move_up(false);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn page_move_multiple_rows() {
        let mut b = EditBuffer::with_text("a\nb\nc\nd\ne");
        b.set_cursor(0, false);
        b.move_vertical(3, false);
        assert_eq!(b.line_col_of(b.cursor()).0, 3);
    }

    #[test]
    fn document_start_end() {
        let mut b = EditBuffer::with_text("a\nb\nc");
        b.set_cursor(2, false);
        b.move_document_start(false);
        assert_eq!(b.cursor(), 0);
        b.move_document_end(false);
        assert_eq!(b.cursor(), b.len());
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut b = EditBuffer::new();
        b.insert_str("hello");
        b.push_snapshot();
        b.insert_str(" world");
        assert_eq!(b.text(), "hello world");
        assert!(b.undo());
        assert_eq!(b.text(), "hello");
        assert!(b.redo());
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn snapshot_dedup() {
        let mut b = EditBuffer::new();
        b.push_snapshot();
        b.push_snapshot();
        b.insert_str("x");
        assert!(b.undo());
        assert!(b.is_empty());
        assert!(!b.undo());
    }

    #[test]
    fn word_moves() {
        let mut b = EditBuffer::with_text("hello world foo");
        b.move_word_left(false);
        assert_eq!(b.cursor(), 12);
        b.set_cursor(0, false);
        b.move_word_right(false);
        assert_eq!(b.cursor(), 5);
    }
}
