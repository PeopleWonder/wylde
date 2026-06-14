//! `wylde-gpui-code-editor` — a native gpui code-editor element, built from
//! scratch (OQ-3 = Option B). **Not** a wrapper of `wylde-gpui-input`: it owns
//! its own [`buffer::EditBuffer`], a custom [`element::EditorElement`] that
//! shapes per-line with a line-number gutter, internal vertical scroll +
//! viewport culling (large-file headroom), and a decoration-run layer for
//! syntax highlighting (S4) and LSP diagnostics (S9). It studies and re-uses
//! the input crate's *proven techniques* (gpui text shaping, the snapshot
//! metrics contract, decoration runs) as reference, but as an independent
//! sibling so it can grow folding / minimap / multi-cursor without dragging
//! the small input widget along.
//!
//! ## What S3 ships
//!   * Multi-line buffer with grapheme-aware cursor, selection, undo/redo.
//!   * Full keyboard editing: arrows / word / line / doc / page motion,
//!     shift-extend, backspace/delete (+word), Enter with auto-indent, Tab
//!     (soft/hard), copy/cut/paste, select-all, undo/redo, Ctrl/Cmd+S →
//!     `EditorEvent::SaveRequested`.
//!   * Mouse: click-to-place, drag-select, double-click word, triple-click
//!     line, wheel scroll.
//!   * Line-number gutter, current-line gutter accent, internal scroll with
//!     viewport culling and scroll-to-caret.
//!   * `read_only` mode (S4 opens binaries-refused / oversized read-only).
//!   * Decoration API (`set_decorations`) + window-absolute glyph metrics
//!     (`rects_for_range` / `index_at_point` / `caret_screen_rect`) for the
//!     future LSP-hover and bubble-tether work.
//!
//! Deferred (architecture supports, not built here): soft-wrap toggle,
//! folding, minimap, multi-cursor, IME composition, blinking caret.

pub mod buffer;
pub mod decoration;
pub mod element;
pub mod metrics;

use std::ops::Range;

use gpui::{
    div, prelude::*, px, App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, ScrollWheelEvent, SharedString, Window,
};
use wylde_theme::colors::SURFACE_950;
use wylde_theme::typography::{size as text_size, FAMILY_MONO};

pub use buffer::EditBuffer;
pub use decoration::{Decoration, Underline};
use metrics::LayoutSnapshot;

/// Consecutive typed chars sharing one undo snapshot before a forced break.
const UNDO_BURST_FORCE: usize = 200;
/// Default soft-tab width (spaces per indent).
const DEFAULT_TAB_WIDTH: usize = 4;
/// Editor text size.
const EDITOR_TEXT_SIZE: f32 = text_size::SM;
/// Editor line height.
const EDITOR_LINE_HEIGHT: f32 = 20.0;

/// Event the editor emits to its parent (the Workspaces `EditorTab`).
#[derive(Debug, Clone)]
pub enum EditorEvent {
    /// Buffer text changed; carries the new value so the parent can track
    /// dirty state without reaching into the editor.
    Changed(String),
    /// User pressed the save chord (Ctrl/Cmd+S). The parent performs the
    /// actual `workspaces.fs.write`.
    SaveRequested,
}

/// The code-editor view.
pub struct CodeEditor {
    pub(crate) buffer: EditBuffer,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) decorations: Vec<Decoration>,
    pub(crate) scroll_top: Pixels,
    pub(crate) scroll_left: Pixels,
    pub(crate) read_only: bool,
    pub(crate) show_gutter: bool,
    soft_tabs: bool,
    tab_width: usize,
    element_key: SharedString,
    dragging: bool,
    burst_since_snapshot: usize,
    pub(crate) last_layout: Option<LayoutSnapshot>,
    /// Last painted viewport height / line height — drive scroll-to-caret.
    viewport_height: Pixels,
    line_height: Pixels,
}

impl CodeEditor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            buffer: EditBuffer::new(),
            focus_handle: cx.focus_handle(),
            decorations: Vec::new(),
            scroll_top: px(0.0),
            scroll_left: px(0.0),
            read_only: false,
            show_gutter: true,
            soft_tabs: true,
            tab_width: DEFAULT_TAB_WIDTH,
            element_key: SharedString::from("wylde-code-editor"),
            dragging: false,
            burst_since_snapshot: 0,
            last_layout: None,
            viewport_height: px(0.0),
            line_height: px(EDITOR_LINE_HEIGHT),
        }
    }

    // ── Builders ────────────────────────────────────────────────────────

    pub fn with_element_key(mut self, key: impl Into<SharedString>) -> Self {
        self.element_key = key.into();
        self
    }
    pub fn with_read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }
    pub fn with_gutter(mut self, on: bool) -> Self {
        self.show_gutter = on;
        self
    }
    pub fn with_soft_tabs(mut self, soft: bool, width: usize) -> Self {
        self.soft_tabs = soft;
        self.tab_width = width.max(1);
        self
    }

    // ── Observers ───────────────────────────────────────────────────────

    pub fn text(&self) -> &str {
        self.buffer.text()
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    pub fn cursor(&self) -> usize {
        self.buffer.cursor()
    }
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.buffer.selection()
    }
    pub fn can_undo(&self) -> bool {
        self.buffer.can_undo()
    }
    pub fn can_redo(&self) -> bool {
        self.buffer.can_redo()
    }

    // ── Mutators ────────────────────────────────────────────────────────

    /// Replace the whole buffer and emit `Changed`.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.reset_scroll_and_caret();
        cx.emit(EditorEvent::Changed(self.buffer.text().to_owned()));
        cx.notify();
    }

    /// Replace the whole buffer WITHOUT emitting `Changed` — used by the
    /// EditorTab when loading a freshly-read file, so the load doesn't read as
    /// a user edit / dirty the document.
    pub fn set_text_silent(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.reset_scroll_and_caret();
        cx.notify();
    }

    fn reset_scroll_and_caret(&mut self) {
        self.buffer.set_cursor(0, false);
        self.scroll_top = px(0.0);
        self.scroll_left = px(0.0);
    }

    pub fn set_read_only(&mut self, ro: bool, cx: &mut Context<Self>) {
        if self.read_only != ro {
            self.read_only = ro;
            cx.notify();
        }
    }

    pub fn set_decorations(&mut self, decos: Vec<Decoration>, cx: &mut Context<Self>) {
        if self.decorations != decos {
            self.decorations = decos;
            cx.notify();
        }
    }

    pub fn clear_decorations(&mut self, cx: &mut Context<Self>) {
        if !self.decorations.is_empty() {
            self.decorations.clear();
            cx.notify();
        }
    }

    /// Move the caret to the start of 1-based `line` and scroll it into view
    /// (a few lines of context above when possible). Used by `open_in_editor`.
    pub fn scroll_to_line(&mut self, line_1based: usize, cx: &mut Context<Self>) {
        let line = line_1based.saturating_sub(1).min(self.buffer.line_count().saturating_sub(1));
        let offset = self.buffer.offset_of_line(line);
        self.buffer.set_cursor(offset, false);
        // Place the target line ~3 rows down from the top when there's room.
        let context_rows = 3.0_f32;
        let target_top = self.line_height * (line as f32 - context_rows);
        self.scroll_top = target_top.max(px(0.0));
        cx.notify();
    }

    // ── Metrics (window-absolute, last painted frame) ───────────────────

    pub fn rects_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        self.last_layout
            .as_ref()
            .map(|l| l.rects_for_range(range))
            .unwrap_or_default()
    }
    pub fn index_at_point(&self, point: Point<Pixels>) -> Option<usize> {
        self.last_layout.as_ref().and_then(|l| l.index_at_point(point))
    }
    pub fn caret_screen_rect(&self) -> Option<Bounds<Pixels>> {
        self.last_layout
            .as_ref()?
            .caret_rect(self.buffer.cursor(), element::CARET_WIDTH)
    }

    // ── Internal: scroll-to-caret ───────────────────────────────────────

    /// Stash the painted viewport + line height (the element calls this each
    /// prepaint) so keyboard motion can keep the caret visible.
    pub(crate) fn note_viewport(&mut self, viewport_height: Pixels, line_height: Pixels) {
        self.viewport_height = viewport_height;
        self.line_height = line_height;
    }

    fn ensure_cursor_visible(&mut self) {
        if self.line_height <= px(0.0) || self.viewport_height <= px(0.0) {
            return;
        }
        let (line, _) = self.buffer.line_col_of(self.buffer.cursor());
        let caret_top = self.line_height * line as f32;
        let caret_bottom = caret_top + self.line_height;
        if caret_top < self.scroll_top {
            self.scroll_top = caret_top;
        } else if caret_bottom > self.scroll_top + self.viewport_height {
            self.scroll_top = caret_bottom - self.viewport_height;
        }
        if self.scroll_top < px(0.0) {
            self.scroll_top = px(0.0);
        }
    }

    // ── Internal: mouse ─────────────────────────────────────────────────

    fn handle_mouse_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(ix) = self.index_at_point(ev.position) else {
            return;
        };
        match ev.click_count {
            2 => {
                self.buffer.set_cursor(ix, false);
                self.buffer.move_word_left(false);
                self.buffer.move_word_right(true);
            }
            n if n >= 3 => {
                self.buffer.set_cursor(ix, false);
                self.buffer.move_line_start(false);
                self.buffer.move_line_end(true);
            }
            _ => {
                self.buffer.set_cursor(ix, ev.modifiers.shift);
                self.dragging = true;
            }
        }
        cx.notify();
    }

    fn handle_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.dragging || ev.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(ix) = self.index_at_point(ev.position) {
            self.buffer.set_cursor(ix, true);
            cx.notify();
        }
    }

    fn handle_mouse_up(&mut self, _ev: &MouseUpEvent, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    fn handle_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(self.line_height);
        self.scroll_top = (self.scroll_top - delta.y).max(px(0.0));
        self.scroll_left = (self.scroll_left - delta.x).max(px(0.0));
        cx.notify();
    }

    // ── Internal: undo coalescing ───────────────────────────────────────

    fn snapshot_break(&mut self) {
        self.buffer.push_snapshot();
        self.burst_since_snapshot = 0;
    }

    fn snapshot_burst(&mut self) {
        if self.burst_since_snapshot == 0 || self.burst_since_snapshot >= UNDO_BURST_FORCE {
            self.buffer.push_snapshot();
            self.burst_since_snapshot = 0;
        }
        self.burst_since_snapshot += 1;
    }

    fn copy_to_clipboard(&self, cx: &mut Context<Self>) {
        if let Some(s) = self.buffer.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(s.to_owned()));
        }
    }

    // ── Internal: keyboard ──────────────────────────────────────────────

    fn handle_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        let extend = m.shift;
        let word_mod = m.control || (cfg!(target_os = "macos") && m.alt);
        let cmd_or_ctrl = m.secondary();
        let key = k.key.as_str();
        let mut changed = false;

        match key {
            "s" if cmd_or_ctrl => {
                cx.emit(EditorEvent::SaveRequested);
                return;
            }
            "enter" if !self.read_only => {
                self.snapshot_break();
                self.buffer.insert_newline();
                changed = true;
            }
            "tab" if !self.read_only => {
                self.snapshot_break();
                self.buffer.insert_tab(self.soft_tabs, self.tab_width);
                changed = true;
            }
            "backspace" if !self.read_only => {
                self.snapshot_break();
                changed = if word_mod {
                    self.buffer.delete_word_left()
                } else {
                    self.buffer.backspace()
                };
            }
            "delete" if !self.read_only => {
                self.snapshot_break();
                changed = if word_mod {
                    self.buffer.delete_word_right()
                } else {
                    self.buffer.delete_forward()
                };
            }
            "left" => {
                if word_mod {
                    self.buffer.move_word_left(extend);
                } else {
                    self.buffer.move_left(extend);
                }
            }
            "right" => {
                if word_mod {
                    self.buffer.move_word_right(extend);
                } else {
                    self.buffer.move_right(extend);
                }
            }
            "up" => self.buffer.move_up(extend),
            "down" => self.buffer.move_down(extend),
            "pageup" => self.buffer.move_vertical(-self.page_rows(), extend),
            "pagedown" => self.buffer.move_vertical(self.page_rows(), extend),
            "home" => {
                if cmd_or_ctrl {
                    self.buffer.move_document_start(extend);
                } else {
                    self.buffer.move_line_start(extend);
                }
            }
            "end" => {
                if cmd_or_ctrl {
                    self.buffer.move_document_end(extend);
                } else {
                    self.buffer.move_line_end(extend);
                }
            }
            "a" if cmd_or_ctrl => self.buffer.select_all(),
            "c" if cmd_or_ctrl => {
                self.copy_to_clipboard(cx);
                return;
            }
            "x" if cmd_or_ctrl => {
                if self.read_only {
                    self.copy_to_clipboard(cx);
                    return;
                }
                self.snapshot_break();
                if let Some(s) = self.buffer.delete_selection() {
                    cx.write_to_clipboard(ClipboardItem::new_string(s));
                    changed = true;
                }
            }
            "v" if cmd_or_ctrl && !self.read_only => {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(s) = item.text() {
                        if !s.is_empty() {
                            self.snapshot_break();
                            self.buffer.insert_str(&s);
                            changed = true;
                        }
                    }
                }
            }
            "z" if cmd_or_ctrl && m.shift => {
                if self.buffer.redo() {
                    changed = true;
                }
            }
            "z" if cmd_or_ctrl => {
                if self.buffer.undo() {
                    changed = true;
                }
            }
            "y" if cmd_or_ctrl => {
                if self.buffer.redo() {
                    changed = true;
                }
            }
            _ => {
                if self.read_only {
                    return;
                }
                let candidate = k.key_char.as_deref().unwrap_or(key);
                if single_printable(candidate).is_some() {
                    self.snapshot_burst();
                    self.buffer.insert_str(candidate);
                    changed = true;
                }
            }
        }

        self.ensure_cursor_visible();
        if changed {
            cx.emit(EditorEvent::Changed(self.buffer.text().to_owned()));
        }
        cx.notify();
    }

    /// Rows per page (page-up/down), from the last painted viewport.
    fn page_rows(&self) -> i32 {
        if self.line_height <= px(0.0) {
            return 20;
        }
        ((self.viewport_height / self.line_height).floor() as i32 - 1).max(1)
    }
}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl Render for CodeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = element::EditorElement {
            editor: cx.entity(),
        };
        div()
            .id(gpui::ElementId::Name(self.element_key.clone()))
            .size_full()
            .overflow_hidden()
            .cursor_text()
            .bg(SURFACE_950)
            .font_family(FAMILY_MONO)
            .text_size(px(EDITOR_TEXT_SIZE))
            .line_height(px(EDITOR_LINE_HEIGHT))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_key(ev, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.handle_mouse_down(ev, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                this.handle_mouse_move(ev, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _w, cx| {
                    this.handle_mouse_up(ev, cx);
                }),
            )
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                this.handle_scroll(ev, cx);
            }))
            .child(body)
    }
}

/// `Some(c)` when `s` is a single non-control character (filters named keys
/// like "escape", "left", "f1" that arrive with len > 1).
fn single_printable(s: &str) -> Option<char> {
    let mut iter = s.chars();
    let c = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    if c.is_control() {
        return None;
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_printable_filters_named_keys() {
        assert!(single_printable("a").is_some());
        assert!(single_printable("ß").is_some());
        assert!(single_printable("").is_none());
        assert!(single_printable("enter").is_none());
        assert!(single_printable("\t").is_none());
    }

    #[test]
    fn editor_event_carries_text() {
        match EditorEvent::Changed("x".into()) {
            EditorEvent::Changed(s) => assert_eq!(s, "x"),
            _ => panic!(),
        }
    }
}
