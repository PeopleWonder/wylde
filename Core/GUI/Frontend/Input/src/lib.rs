//! Native gpui TextInput widget — single-line + multi-line variants.
//!
//! Why an in-tree input crate, not `gpui-component`?
//!   * `gpui-component` tracks `gpui` main; pinning a single widget from
//!     it would tie the GUI workspace to a moving rev that drifts from
//!     `b3d93d44` within ~a week.  Slice 4 documented that risk; this
//!     crate is the slice-5.1 follow-on that closes the "no opinionated
//!     TextInput" gap without paying that cost.
//!   * The widget surface needed by Wylde panels is small (prompt input,
//!     search box, future settings fields).  Owning ~700 lines of code
//!     is cheaper than owning a moving-rev cone-of-influence on a
//!     30-crate library.
//!
//! Opt-in, not blessed.  Per the Wylde user's "nothing shared" rule, a panel
//! that needs a richer (or weirder) input is free to hand-roll one.  This
//! crate is just the well-trodden path.
//!
//! ## What ships in this slice
//!
//!   * Single-line and multi-line variants.
//!   * Full keyboard-driven cursor + selection: arrows, shift+arrows,
//!     home/end, ctrl/cmd+arrows (word jump), ctrl/cmd+home/end (doc
//!     jump), ctrl/cmd+a (select all).
//!   * Backspace / delete / word-backspace / word-delete.
//!   * Copy / cut / paste via the OS clipboard.
//!   * Undo / redo with a 100-entry ring; typing bursts coalesce into
//!     one undo step (forced break every `UNDO_BURST_FORCE` chars).
//!   * Placeholder text.
//!   * Submit chord (Enter on single-line; Ctrl/Cmd+Enter on multi-line).
//!   * `EventEmitter<InputEvent>` → parents subscribe for Submit /
//!     Changed without piercing the input's state.
//!   * Theme-integrated chrome (border, background, focus ring) sourced
//!     from `wylde_theme::colors::*`.
//!
//! ## What's deferred (with externality reasons)
//!
//!   * **Click-to-position cursor + drag-to-select + double-click word /
//!     triple-click line.**  Mapping a `(x, y)` to a UTF-8 byte offset
//!     needs text metrics gpui doesn't expose on a `div`'s child run at
//!     `b3d93d44`.  A custom layout-pass slice unblocks them; until then,
//!     cursor positioning is keyboard-only.  Single mouse-click anywhere
//!     in the input focuses it.
//!   * **IME / dead-key composition input.**  Platform composition events
//!     don't reach arbitrary elements at this rev.  Asian-language users
//!     get a degraded experience until this lands.
//!   * **Spell-check underlines + accessibility hooks.**  Both depend on
//!     platform APIs gpui's element model doesn't yet thread through.
//!   * **Blinking caret.**  Solid caret only — a per-frame blink loop
//!     adds a background task per input.  Cosmetic; revisit if needed.

pub mod buffer;

use gpui::{
    div, prelude::*, px, rgb, App, ClipboardItem, Context, ElementId, EventEmitter, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_EMPHASIS, BORDER_FOCUSED, BORDER_SUBTLE, SURFACE_700, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY,
};
use wylde_theme::typography::{size as text_size, FAMILY_INTER};

pub use buffer::TextBuffer;

/// Cap on the number of consecutive typed-char inserts that share a
/// single undo snapshot before a forced break.  Without it a 5000-char
/// paragraph would undo in one step (annoying).
const UNDO_BURST_FORCE: usize = 200;

// ── Public configuration ─────────────────────────────────────────────

/// When the input fires `InputEvent::Submit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitMode {
    /// Bare Enter submits.  Newlines never reach the buffer.
    EnterSubmits,
    /// Bare Enter inserts a newline; Ctrl/Cmd+Enter submits.
    ModEnterSubmits,
    /// Submission is never emitted by Enter.  The parent drives
    /// submission from a button click or similar.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    SingleLine,
    MultiLine,
}

/// Event the input emits to its parent.
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// The buffer text changed.  Carries the new value so the parent
    /// doesn't need to reach into the input handle on every change.
    Changed(String),
    /// User pressed the submit chord (Enter / Mod+Enter depending on
    /// `SubmitMode`).
    Submit(String),
}

// ── The View itself ──────────────────────────────────────────────────

pub struct TextInput {
    pub focus_handle: FocusHandle,
    buffer: TextBuffer,
    placeholder: SharedString,
    submit: SubmitMode,
    variant: Variant,
    disabled: bool,
    read_only: bool,
    /// Min height in CSS pixels.  36 px single-line, 80 px multi-line.
    min_height: f32,
    /// Optional max height.  Multi-line scrolls inside its bounds.
    max_height: Option<f32>,
    /// Draw the standard border + rounded background.
    chrome: bool,
    /// Stable id used in `ElementId` keys.  Defaults to "wylde-input";
    /// set per-instance when multiple inputs co-exist in one Entity tree.
    element_key: SharedString,
    /// Snapshot bookkeeping — collapse typing bursts into one undo step.
    burst_inserts_since_snapshot: usize,
}

impl TextInput {
    /// Build a single-line input via `cx.new(TextInput::single_line)`.
    pub fn single_line(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: TextBuffer::new(true),
            placeholder: SharedString::default(),
            submit: SubmitMode::EnterSubmits,
            variant: Variant::SingleLine,
            disabled: false,
            read_only: false,
            min_height: 36.0,
            max_height: None,
            chrome: true,
            element_key: SharedString::from("wylde-input"),
            burst_inserts_since_snapshot: 0,
        }
    }

    /// Build a multi-line input via `cx.new(TextInput::multi_line)`.
    pub fn multi_line(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: TextBuffer::new(false),
            placeholder: SharedString::default(),
            submit: SubmitMode::ModEnterSubmits,
            variant: Variant::MultiLine,
            disabled: false,
            read_only: false,
            min_height: 80.0,
            max_height: Some(200.0),
            chrome: true,
            element_key: SharedString::from("wylde-input"),
            burst_inserts_since_snapshot: 0,
        }
    }

    // ── Builder-style configuration ─────────────────────────────────

    pub fn with_placeholder(mut self, text: impl Into<SharedString>) -> Self {
        self.placeholder = text.into();
        self
    }

    pub fn with_submit_mode(mut self, mode: SubmitMode) -> Self {
        self.submit = mode;
        self
    }

    pub fn with_min_height(mut self, h: f32) -> Self {
        self.min_height = h;
        self
    }

    pub fn with_max_height(mut self, h: f32) -> Self {
        self.max_height = Some(h);
        self
    }

    pub fn without_chrome(mut self) -> Self {
        self.chrome = false;
        self
    }

    pub fn with_initial_text(mut self, text: impl Into<String>) -> Self {
        let s = text.into();
        let single = matches!(self.variant, Variant::SingleLine);
        self.buffer = TextBuffer::with_text(s, single);
        self
    }

    pub fn with_element_key(mut self, key: impl Into<SharedString>) -> Self {
        self.element_key = key.into();
        self
    }

    // ── Imperative API parents call ────────────────────────────────

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn set_text(&mut self, s: impl Into<String>, cx: &mut Context<Self>) {
        self.buffer.set_text(s);
        cx.emit(InputEvent::Changed(self.buffer.text().to_owned()));
        cx.notify();
    }

    /// Set the buffer text **without** emitting [`InputEvent::Changed`].
    ///
    /// For syncing an input's value *from* backend state (e.g. the
    /// Settings → Ollama section repainting a field when the model
    /// changes): emitting `Changed` there would feed the parent's own
    /// persist-on-change handler and loop a write back to the store.
    pub fn set_text_silent(&mut self, s: impl Into<String>, cx: &mut Context<Self>) {
        self.buffer.set_text(s);
        cx.notify();
    }

    /// Update the placeholder shown when the buffer is empty. Lets a
    /// parent repaint the "what value would apply" hint at runtime (the
    /// Settings → Ollama section swaps it as the selected model changes)
    /// without rebuilding the input entity.
    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = text.into();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.buffer.set_text("");
        cx.emit(InputEvent::Changed(String::new()));
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    pub fn set_disabled(&mut self, v: bool, cx: &mut Context<Self>) {
        self.disabled = v;
        cx.notify();
    }

    pub fn set_read_only(&mut self, v: bool, cx: &mut Context<Self>) {
        self.read_only = v;
        cx.notify();
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// Direct buffer access — escape hatch for parents that want to
    /// drive editing programmatically (e.g. inserting a token at the
    /// cursor from a completions UI).  After mutating, call
    /// `emit_changed` + `cx.notify()`.
    pub fn buffer_mut(&mut self) -> &mut TextBuffer {
        &mut self.buffer
    }

    pub fn emit_changed(&self, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Changed(self.buffer.text().to_owned()));
    }

    // ── Internal: key dispatch ─────────────────────────────────────

    fn handle_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let k = &ev.keystroke;
        let m = k.modifiers;
        let extend = m.shift;
        let word_mod = m.control || (cfg!(target_os = "macos") && m.alt);
        let cmd_or_ctrl = m.secondary();

        let key = k.key.as_str();
        let mut changed = false;

        match key {
            "enter" => {
                if self.read_only {
                    return;
                }
                if enter_submits(self.submit, self.variant, m.shift, cmd_or_ctrl) {
                    cx.emit(InputEvent::Submit(self.buffer.text().to_owned()));
                    return;
                }
                // Newline: bare Enter under `ModEnterSubmits`, or
                // Shift+Enter under `EnterSubmits` on a multi-line input.
                // Single-line buffers strip the `\n` themselves.
                self.snapshot_break();
                self.buffer.insert_str("\n");
                changed = true;
            }
            "backspace" => {
                if self.read_only {
                    return;
                }
                self.snapshot_break();
                if word_mod {
                    changed = self.buffer.delete_word_left();
                } else {
                    changed = self.buffer.backspace();
                }
            }
            "delete" => {
                if self.read_only {
                    return;
                }
                self.snapshot_break();
                if word_mod {
                    changed = self.buffer.delete_word_right();
                } else {
                    changed = self.buffer.delete_forward();
                }
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
            "up" => {
                self.buffer.move_up(extend);
            }
            "down" => {
                self.buffer.move_down(extend);
            }
            "home" => {
                if cmd_or_ctrl {
                    self.buffer.set_cursor(0, extend);
                } else {
                    self.buffer.move_line_start(extend);
                }
            }
            "end" => {
                if cmd_or_ctrl {
                    let len = self.buffer.len();
                    self.buffer.set_cursor(len, extend);
                } else {
                    self.buffer.move_line_end(extend);
                }
            }
            "a" if cmd_or_ctrl => {
                self.buffer.select_all();
            }
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
            "v" if cmd_or_ctrl => {
                if self.read_only {
                    return;
                }
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(s) = item.text() {
                        self.snapshot_break();
                        if !s.is_empty() {
                            self.buffer.insert_str(&s);
                            changed = true;
                        }
                    }
                }
            }
            "z" if cmd_or_ctrl && m.shift => {
                // Cmd/Ctrl+Shift+Z — redo (matches gnome / windows; macOS
                // accepts this dialect alongside Cmd+Shift+Z).
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

        if changed {
            cx.emit(InputEvent::Changed(self.buffer.text().to_owned()));
        }
        cx.notify();
    }

    fn copy_to_clipboard(&self, cx: &mut Context<Self>) {
        if let Some(s) = self.buffer.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(s.to_owned()));
        }
    }

    /// Snapshot before a "break" op (backspace, delete, paste, undo) so
    /// undo rolls back the full preceding state.
    fn snapshot_break(&mut self) {
        self.buffer.push_snapshot();
        self.burst_inserts_since_snapshot = 0;
    }

    /// Snapshot lazily during a typing burst.  First-of-burst triggers
    /// a snapshot; the next `UNDO_BURST_FORCE` chars share that snapshot
    /// so undo collapses a sentence into one step.
    fn snapshot_burst(&mut self) {
        if self.burst_inserts_since_snapshot == 0
            || self.burst_inserts_since_snapshot >= UNDO_BURST_FORCE
        {
            self.buffer.push_snapshot();
            self.burst_inserts_since_snapshot = 0;
        }
        self.burst_inserts_since_snapshot += 1;
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<InputEvent> for TextInput {}

// ── Rendering ────────────────────────────────────────────────────────

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);

        // Focus ring.  Passed straight through (no `pack`) so the token's
        // alpha survives: a near-invisible hairline when unfocused, a
        // clear brand-hue outline when focused.  `pack` would drop the
        // alpha and make both states an identical opaque line.
        let chrome_border = if focused {
            BORDER_FOCUSED
        } else {
            BORDER_SUBTLE
        };
        let bg = if self.disabled { SURFACE_700 } else { SURFACE_900 };

        // Solid caret whenever the input holds focus — drawn at the cursor
        // (column 0 in an empty input, via `empty_body`).  An earlier
        // blink loop gated this on a `caret_visible` phase that could sit
        // "off" on an idle empty input, so a freshly-clicked empty field
        // showed no caret until the first keystroke reset the phase. A
        // solid caret is the reliable always-there affordance.
        let show_caret = focused;

        let body = if self.buffer.is_empty() {
            empty_body(&self.placeholder, show_caret)
        } else {
            content_node(&self.buffer, show_caret)
        };

        let mut root = div()
            .id(ElementId::Name(self.element_key.clone()))
            .flex()
            .flex_col()
            .w_full()
            .cursor_text()
            .min_h(px(self.min_height))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_key(ev, window, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, window, cx| {
                    this.focus_handle.focus(window, cx);
                    cx.notify();
                }),
            )
            .child(body);

        if self.chrome {
            root = root
                .px_3()
                .py_2()
                .rounded(px(8.0))
                .border_1()
                // `chrome_border` is an alpha-bearing `Rgba`; pass it as-is
                // (`Rgba: Into<Hsla>`) so the focus alpha is composited over
                // the surface instead of being flattened by `pack`.
                .border_color(chrome_border)
                .bg(rgb(pack(bg)));
        }

        if let Some(max_h) = self.max_height {
            root = root.max_h(px(max_h)).overflow_y_scroll();
        }

        root
    }
}

fn placeholder_node(placeholder: &SharedString) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(text_size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(placeholder.clone())
}

/// Body shown when the buffer is empty: the caret (at offset 0) followed
/// by the muted placeholder.  Without this an empty focused input — the
/// chat prompt's resting state — would render the placeholder with *no*
/// caret, so focus had no visible affordance at all.
fn empty_body(placeholder: &SharedString, show_caret: bool) -> gpui::Div {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .min_h(px(20.0));
    if show_caret {
        // `flex_none` so the 2px caret keeps its width — this row has no
        // `flex_wrap`, so a shrinkable child could otherwise be squeezed
        // toward zero next to the placeholder.
        row = row.child(caret_bar().flex_none());
    }
    row.child(placeholder_node(placeholder))
}

/// Render the buffer's text broken into lines, with caret + selection
/// highlight inserted at the relevant byte offsets.  `show_caret` gates
/// the caret bar (true whenever the input holds focus).
fn content_node(buffer: &TextBuffer, show_caret: bool) -> gpui::Div {
    let text = buffer.text();
    let cursor = buffer.cursor();
    let selection = buffer.selection();

    let mut col = div().flex().flex_col().w_full().gap(px(0.0));

    if buffer.is_single_line() {
        col = col.child(line_row(text, 0, text.len(), cursor, selection, show_caret));
        return col;
    }

    let mut line_start = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            col = col.child(line_row(text, line_start, i, cursor, selection, show_caret));
            line_start = i + 1;
        }
    }
    // Trailing line.
    col = col.child(line_row(
        text,
        line_start,
        text.len(),
        cursor,
        selection,
        show_caret,
    ));
    col
}

/// Render one line as a horizontal flex of text spans + an optional
/// caret bar.  Selection ranges that overlap this line draw a coloured
/// background behind the affected slice.
fn line_row(
    text: &str,
    line_start: usize,
    line_end: usize,
    cursor: usize,
    selection: Option<(usize, usize)>,
    show_caret: bool,
) -> Stateful<gpui::Div> {
    let line_id = ElementId::Name(format!("wylde-input-line::{line_start}").into());

    // Build the sorted list of split points that fall inside this line:
    // selection start, selection end, cursor.
    let mut splits: Vec<(usize, SplitKind)> = Vec::new();
    if let Some((s, e)) = selection {
        let s = s.max(line_start);
        let e = e.min(line_end);
        if s < e {
            splits.push((s, SplitKind::SelStart));
            splits.push((e, SplitKind::SelEnd));
        }
    }
    if show_caret && cursor >= line_start && cursor <= line_end {
        splits.push((cursor, SplitKind::Cursor));
    }
    splits.sort_by_key(|(b, _)| *b);

    // Fast path — no caret/selection split on this line.  Render the run
    // as a single width-bounded text block: a direct string child of a
    // `w_full()` element soft-wraps to the input's width.  This is what
    // stops a long pasted line (10k chars, no newline) from rendering as
    // one unbounded `flex_row` that overflows horizontally and blows out
    // the InferenceBar's layout.
    if splits.is_empty() {
        let mut block = div().id(line_id).w_full().min_h(px(20.0));
        if line_start < line_end {
            block = block
                .font_family(FAMILY_INTER)
                .text_size(px(text_size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(text[line_start..line_end].to_owned()));
        }
        return block;
    }

    // Split path — the focused line carries an inline caret and/or a
    // selection highlight, so it stays a span row.  It's `w_full()` +
    // `flex_wrap()` so multi-span lines wrap, and `overflow_hidden()` so
    // even a single oversized run can't push the layout wide.  (True
    // soft-wrap *with* inline carets needs per-glyph text metrics gpui
    // doesn't expose at `b3d93d44` — see the crate-level docs.)
    let mut row = div()
        .id(line_id)
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .overflow_hidden()
        .items_center()
        .min_h(px(20.0));

    let mut in_sel = false;
    let mut run_start = line_start;

    for (boundary, kind) in &splits {
        if run_start < *boundary {
            row = row.child(span_run(&text[run_start..*boundary], in_sel));
        }
        match kind {
            SplitKind::SelStart => {
                in_sel = true;
                run_start = *boundary;
            }
            SplitKind::SelEnd => {
                in_sel = false;
                run_start = *boundary;
            }
            SplitKind::Cursor => {
                row = row.child(caret_bar());
                run_start = *boundary;
            }
        }
    }

    if run_start < line_end {
        row = row.child(span_run(&text[run_start..line_end], in_sel));
    }

    // The empty trailing-line caret (`text` ends with `\n`, cursor on the
    // blank last line) is handled by the split path above: a cursor at
    // `line_start == line_end` pushes a `Cursor` split, so the loop emits
    // the caret bar.  No extra branch needed here.
    row
}

#[derive(Debug, Clone, Copy)]
enum SplitKind {
    SelStart,
    SelEnd,
    Cursor,
}

fn span_run(text: &str, in_sel: bool) -> gpui::Div {
    let mut d = div()
        .font_family(FAMILY_INTER)
        .text_size(px(text_size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(text.to_owned()));
    if in_sel {
        d = d.bg(rgb(pack(BORDER_EMPHASIS)));
    }
    d
}

/// The insertion caret: a slim, high-contrast vertical bar.  Uses
/// `TEXT_PRIMARY` (near-white) rather than the brand teal so it reads as
/// the familiar OS-style text cursor against the dark input, not a
/// decorative accent.
fn caret_bar() -> gpui::Div {
    div().w(px(2.0)).h(px(18.0)).bg(rgb(pack(TEXT_PRIMARY)))
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Decide whether an Enter keystroke submits (vs. inserts a newline)
/// for the given mode / variant / modifier state.  Pure so the chord
/// matrix is unit-testable without a `Window` or `Context`.
///
///   * `EnterSubmits` — bare Enter submits.  On a *multi-line* input
///     Shift+Enter is the newline escape hatch (the conventional chat
///     UX); single-line inputs have nowhere to put a newline, so Shift
///     is ignored and Enter always submits.
///   * `ModEnterSubmits` — only Ctrl/Cmd+Enter submits; bare Enter (and
///     Shift+Enter) insert a newline.
///   * `Never` — Enter never submits.
fn enter_submits(mode: SubmitMode, variant: Variant, shift: bool, mod_enter: bool) -> bool {
    match mode {
        SubmitMode::EnterSubmits => !(matches!(variant, Variant::MultiLine) && shift),
        SubmitMode::ModEnterSubmits => mod_enter,
        SubmitMode::Never => false,
    }
}

/// `Some(c)` when `s` is a single non-control character; filters named
/// keys ("escape", "left", "f1") which arrive with len > 1.
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

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts.
pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    // `BRAND` is no longer used by prod code (the caret moved to
    // `TEXT_PRIMARY`); the pack round-trip test still pins it as a known
    // value, so import it here rather than in the prod `use`.
    use wylde_theme::colors::BRAND;

    #[test]
    fn single_printable_filters_named_keys() {
        assert!(single_printable("a").is_some());
        assert!(single_printable("ß").is_some());
        assert!(single_printable("").is_none());
        assert!(single_printable("escape").is_none());
        assert!(single_printable("left").is_none());
        assert!(single_printable("\t").is_none());
        assert!(single_printable("\n").is_none());
    }

    #[test]
    fn pack_round_trips_brand() {
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn submit_mode_variants_distinct() {
        assert_ne!(SubmitMode::EnterSubmits, SubmitMode::ModEnterSubmits);
        assert_ne!(SubmitMode::EnterSubmits, SubmitMode::Never);
    }

    #[test]
    fn variant_distinct() {
        assert_ne!(Variant::SingleLine, Variant::MultiLine);
    }

    #[test]
    fn enter_submits_chat_chord_matrix() {
        use SubmitMode::*;
        use Variant::*;
        // Chat's mode: bare Enter sends, Shift+Enter is a newline,
        // Ctrl/Cmd+Enter also sends (we only divert on Shift).
        assert!(enter_submits(EnterSubmits, MultiLine, false, false)); // Enter → send
        assert!(!enter_submits(EnterSubmits, MultiLine, true, false)); // Shift+Enter → newline
        assert!(enter_submits(EnterSubmits, MultiLine, false, true)); // Ctrl+Enter → send
        // Single-line EnterSubmits ignores Shift — Enter always submits.
        assert!(enter_submits(EnterSubmits, SingleLine, false, false));
        assert!(enter_submits(EnterSubmits, SingleLine, true, false));
        // ModEnterSubmits (Images' mode): only the chord submits.
        assert!(!enter_submits(ModEnterSubmits, MultiLine, false, false)); // Enter → newline
        assert!(!enter_submits(ModEnterSubmits, MultiLine, true, false)); // Shift+Enter → newline
        assert!(enter_submits(ModEnterSubmits, MultiLine, false, true)); // Ctrl+Enter → send
        // Never never submits.
        assert!(!enter_submits(Never, SingleLine, false, false));
        assert!(!enter_submits(Never, MultiLine, false, true));
    }

    #[test]
    fn long_no_newline_paste_stays_one_logical_line() {
        // Regression anchor for the InferenceBar horizontal-overflow QA
        // item.  A 10k-char paste with no newline is a *single* logical
        // line, so `content_node` emits exactly one `line_row`.  That row
        // now soft-wraps (width-bounded text block) instead of rendering
        // as an unbounded `flex_row` that overflowed horizontally and
        // blew out the bar's layout.  Pin the precondition the wrap path
        // relies on: the buffer keeps a no-newline blob as one line.
        let blob = "x".repeat(10_000);
        let mut b = TextBuffer::new(false); // multi-line variant
        b.insert_str(&blob);
        assert_eq!(b.text().len(), 10_000);
        assert!(!b.text().contains('\n'));
        assert_eq!(b.text().split('\n').count(), 1);
    }

    #[test]
    fn input_event_carries_text() {
        match InputEvent::Submit("hello".into()) {
            InputEvent::Submit(s) => assert_eq!(s, "hello"),
            _ => panic!(),
        }
        match InputEvent::Changed("world".into()) {
            InputEvent::Changed(s) => assert_eq!(s, "world"),
            _ => panic!(),
        }
    }
}
