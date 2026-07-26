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
//! ## The glyph-metrics pass (TBS follow-on slice)
//!
//! The renderer is a custom [`element::TextArea`] that shapes the buffer
//! through gpui's text system — the single source of truth for glyph
//! geometry. That unlocked, in one pass:
//!
//!   * **Click-to-position + drag-to-select + double-click word /
//!     triple-click line** (point → byte offset via the shaped layout).
//!   * **True soft-wrap with the inline caret** (the old span-row
//!     limitation is gone).
//!   * **Highlight spans** ([`HighlightSpan`]) — colour/background/
//!     (wavy-)underline ranges painted as decoration runs, wrap-aware.
//!     The composer's IDE-style word squiggle rides this.
//!   * **The glyph-metrics API** — [`TextInput::rects_for_range`] /
//!     [`TextInput::index_at_point`] / [`TextInput::caret_screen_rect`],
//!     window-absolute, describing the last painted frame. The bubble
//!     layer's tethers anchor to these.
//!
//! ## Still deferred (with externality reasons)
//!
//!   * **IME / dead-key composition input.**  Needs an
//!     `EntityInputHandler` + `window.handle_input` wiring pass;
//!     mechanical now that the element owns shaping, but out of this
//!     slice's scope.
//!   * **Spell-check + accessibility hooks.**  Platform APIs gpui's
//!     element model doesn't yet thread through.
//!   * **Blinking caret.**  Solid caret only — a per-frame blink loop
//!     adds a background task per input.  Cosmetic; revisit if needed.

pub mod buffer;
pub mod element;

use std::ops::Range;

use gpui::{
    div, prelude::*, px, rgb, App, Bounds, ClipboardItem, Context, ElementId, EventEmitter,
    FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, Rgba, SharedString, Window,
};
use wylde_theme::colors::{BORDER_FOCUSED, BORDER_SUBTLE, SURFACE_700, SURFACE_900};
use wylde_theme::typography::{size as text_size, FAMILY_INTER};

pub use buffer::TextBuffer;
use element::LayoutInfo;

/// Cap on the number of consecutive typed-char inserts that share a
/// single undo snapshot before a forced break.  Without it a 5000-char
/// paragraph would undo in one step (annoying).
const UNDO_BURST_FORCE: usize = 200;

/// The shared undo timeline's clock. Text snapshots (this crate) and any
/// sibling op stacks (the chat panel's bubble ops) stamp from the SAME
/// counter, so an external arbiter can interleave "undo the newest thing"
/// across stacks by comparing stamps (Plan §5.9's unified Ctrl+Z).
static UNDO_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Take the next position on the shared undo timeline.
pub fn next_undo_seq() -> u64 {
    UNDO_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

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

// ── Highlight spans (glyph-metrics slice) ────────────────────────────

/// An underline decoration for a [`HighlightSpan`]. `wavy: true` is the
/// IDE-style squiggle — gpui paints it natively, wrap-aware.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnderlineSpec {
    pub color: Rgba,
    /// Thickness in CSS pixels.
    pub thickness: f32,
    pub wavy: bool,
}

/// A styled byte range painted as part of the text itself (a shaping
/// decoration run, not an overlay) — so it wraps exactly with the glyphs.
/// Ranges may be stale relative to the live buffer (recognition runs
/// async): out-of-range and mid-character boundaries are clamped/snapped
/// at paint, never panicking. On overlap, the later span in the `Vec`
/// wins. Colours come from the CALLER's theme — this crate styles nothing
/// itself.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    /// Text colour override.
    pub color: Option<Rgba>,
    /// Background fill behind the glyphs.
    pub background: Option<Rgba>,
    pub underline: Option<UnderlineSpec>,
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
    /// Styled ranges painted as decoration runs (glyph-metrics slice).
    highlights: Vec<HighlightSpan>,
    /// The last painted frame's shaped layout — what the metrics API and
    /// the mouse handlers read. Written by `element::TextArea::paint`.
    last_layout: Option<LayoutInfo>,
    /// A left-button drag-selection is in progress.
    dragging: bool,
    /// Unified-undo mode (§5.9): this input does NOT handle Ctrl+Z /
    /// Ctrl+Shift+Z / Ctrl+Y itself — the chords bubble to an ancestor
    /// arbiter that interleaves text undo with sibling op stacks via
    /// [`TextInput::undo`]/[`TextInput::redo`] and the seq peeks. Every
    /// other input keeps self-contained text undo.
    external_undo: bool,
    /// Explicit colour for the typed buffer glyphs. `None` inherits the
    /// ambient text-style colour from the element cascade (gpui's default,
    /// which is dark — invisible on the app's dark surfaces unless an
    /// ancestor sets one). Set per-instance where the input must read on
    /// its own surface (the chat composer wants true white). Placeholder
    /// (`TEXT_MUTED`, italic) and highlight-span colours are unaffected.
    text_color: Option<Rgba>,
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
            highlights: Vec::new(),
            last_layout: None,
            dragging: false,
            external_undo: false,
            text_color: None,
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
            highlights: Vec::new(),
            last_layout: None,
            dragging: false,
            external_undo: false,
            text_color: None,
        }
    }

    // ── Builder-style configuration ─────────────────────────────────

    /// Opt into unified undo (§5.9): the input stops handling the undo /
    /// redo chords itself and lets them bubble to an ancestor arbiter,
    /// which drives text undo imperatively via [`Self::undo`]/[`Self::redo`]
    /// after comparing [`Self::top_undo_seq`] against its sibling stacks.
    pub fn with_external_undo(mut self) -> Self {
        self.external_undo = true;
        self
    }

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

    /// Override the colour of the typed buffer glyphs. Only the real text
    /// takes this colour — the placeholder stays `TEXT_MUTED`/italic and
    /// any highlight spans keep their per-span colours.
    pub fn with_text_color(mut self, color: Rgba) -> Self {
        self.text_color = Some(color);
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

    // ── Unified undo (§5.9) — the arbiter-facing surface ───────────

    /// Imperative text undo (the arbiter decided the newest op is ours).
    /// Emits `Changed` like a keyboard undo would.
    pub fn undo(&mut self, cx: &mut Context<Self>) -> bool {
        if self.buffer.undo() {
            cx.emit(InputEvent::Changed(self.buffer.text().to_owned()));
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Imperative text redo.
    pub fn redo(&mut self, cx: &mut Context<Self>) -> bool {
        if self.buffer.redo() {
            cx.emit(InputEvent::Changed(self.buffer.text().to_owned()));
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Timeline stamp of the newest undoable text snapshot.
    pub fn top_undo_seq(&self) -> Option<u64> {
        self.buffer.top_undo_seq()
    }

    /// Timeline stamp of the newest redoable text snapshot.
    pub fn top_redo_seq(&self) -> Option<u64> {
        self.buffer.top_redo_seq()
    }

    /// Seal the current typing burst: the next insert opens a NEW undo
    /// snapshot. The arbiter calls this when a sibling-stack op lands
    /// mid-burst, so "hel‹bubble op›lo" undoes as lo → bubble → hel
    /// instead of bubble → hello.
    pub fn seal_undo_burst(&mut self) {
        self.burst_inserts_since_snapshot = 0;
    }

    /// Unified linear history: a NEW op on a sibling stack invalidates
    /// this input's redo branch too.
    pub fn clear_redo(&mut self) {
        self.buffer.clear_redo();
    }

    // ── Highlights (glyph-metrics slice) ───────────────────────────

    /// Replace the styled-range set. Ranges are byte offsets into the
    /// CURRENT text; stale ranges are clamped/snapped at paint, so an
    /// async producer (the composer's recognition pass) can never panic
    /// the renderer. Later spans win on overlap.
    pub fn set_highlights(&mut self, spans: Vec<HighlightSpan>, cx: &mut Context<Self>) {
        if self.highlights != spans {
            self.highlights = spans;
            cx.notify();
        }
    }

    pub fn clear_highlights(&mut self, cx: &mut Context<Self>) {
        if !self.highlights.is_empty() {
            self.highlights.clear();
            cx.notify();
        }
    }

    // ── Glyph metrics (read the last painted frame) ────────────────
    //
    // All coordinates are WINDOW-absolute. The snapshot describes the
    // most recently painted frame, so values are at most one frame
    // stale — the right contract for mouse handling and for chrome
    // (underlines, bubble tethers) that repaints alongside the text.

    /// Screen rects covering `range` — one per visual row touched
    /// (a soft-wrapped word yields multiple). Empty when nothing has
    /// painted yet, the buffer is empty, or the range is collapsed.
    pub fn rects_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        self.last_layout
            .as_ref()
            .map(|l| l.rects_for_range(range))
            .unwrap_or_default()
    }

    /// The byte offset closest to a window point. `None` before the
    /// first paint.
    pub fn index_at_point(&self, point: Point<Pixels>) -> Option<usize> {
        self.last_layout
            .as_ref()
            .and_then(|l| l.index_at_point(point))
    }

    /// The caret's screen rect (2px × line height). `None` before the
    /// first paint.
    pub fn caret_screen_rect(&self) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        layout.caret_rect(if self.buffer.is_empty() {
            0
        } else {
            self.buffer.cursor()
        })
    }

    /// Window-absolute bounds of the painted text area. `None` before
    /// the first paint.
    pub fn layout_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last_layout.as_ref().map(|l| l.bounds)
    }

    // ── Internal: mouse dispatch (glyph-metrics slice) ─────────────

    /// Left press: position the caret at the click (Shift extends);
    /// double-click selects the word, triple-click the logical line.
    fn handle_mouse_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(ix) = self.index_at_point(ev.position) else {
            return; // nothing painted yet — focus alone is fine
        };
        match ev.click_count {
            2 => {
                // Word select: collapse at the click, then span the word.
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

    /// Drag: extend the selection toward the pointer.
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
            // Unified-undo mode: the chords are NOT consumed here — they
            // bubble to the ancestor arbiter (§5.9), which interleaves
            // text undo with its sibling op stacks and calls back via
            // `undo()`/`redo()`.
            "z" | "y" if cmd_or_ctrl && self.external_undo => {
                return;
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
        let bg = if self.disabled {
            SURFACE_700
        } else {
            SURFACE_900
        };

        // The shaped-text element (element.rs) paints text, selection and
        // the solid caret itself; the root supplies the text style it
        // shapes under (family/size/line height) and the event handlers.
        let body = element::TextArea { input: cx.entity() };

        // wylde-check: control-ok: the TextInput root is a focus + keyboard
        // surface (the editable field), not a click-button. Routing it through
        // `control()` would enrol it in the per-frame registry, so every panel
        // embedding a text input would "walk" it and demand a click effect it
        // has no reason to produce (focusing a field moves no backend/nav/state).
        let mut root = div()
            // wylde-check: control-ok: focus/keyboard surface, not a click-button (see above)
            .id(ElementId::Name(self.element_key.clone()))
            .flex()
            .flex_col()
            .w_full()
            .cursor_text()
            .min_h(px(self.min_height))
            .font_family(FAMILY_INTER)
            .text_size(px(text_size::SM))
            .line_height(px(20.0))
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
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                this.handle_mouse_move(ev, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _window, cx| {
                    this.handle_mouse_up(ev, cx);
                }),
            )
            .child(body);

        // Per-instance typed-text colour. Set on the root so the
        // `TextArea` element's `window.text_style()` shapes the buffer
        // glyphs under it (same cascade the family/size refinements ride);
        // the placeholder branch in `element::shape` overrides to
        // `TEXT_MUTED` regardless, so only real text takes this colour.
        if let Some(color) = self.text_color {
            root = root.text_color(color);
        }

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

        // White-font pass (W3): disabled inputs got a darker bg (SURFACE_700)
        // but, with the text tokens lifted toward white, that alone risks
        // reading as merely-secondary rather than inert. Add a NON-colour cue
        // — reduced opacity over the whole control — so disabled is
        // unambiguous and the brightened tokens don't flatten the state.
        if self.disabled {
            root = root.opacity(0.55);
        }

        root
    }
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
