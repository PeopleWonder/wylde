# wylde-gpui-input

Native gpui `TextInput` widget for Wylde panels. Single-line and
multi-line variants, theme-integrated styling, keyboard-driven editing,
clipboard, undo/redo. Backs the Chat panel's prompt input and the Memory
panel's long-term search.

## Opt-in, not blessed

Per Wylde's "nothing shared" rule (the Wylde user's standing guidance against
forbidden shared infrastructure), this crate is **opt-in** for any panel
that wants a stock text-input experience. Future panels are free to
hand-roll their own input widgets if they have weird requirements; this
is just the well-trodden path that the slice-5 hand-rolled
keyboard-dispatch panels can graduate to.

What makes it NOT a violation of "nothing shared":

  * It's not Wylde first-party code reaching into another service's
    panel — it's a third-party-style library that any panel can choose
    to import.
  * Adopting it is one Cargo.toml line + a constructor call; there is
    no global state, no required init, no service contract.
  * Dropping it is the same: stop importing the crate, hand-roll
    whatever the panel needs.

## What ships

  * Single-line and multi-line variants.
  * Full keyboard-driven cursor + selection: arrows, shift+arrows,
    home/end, ctrl/cmd+arrows (word jump), ctrl/cmd+home/end (doc jump),
    ctrl/cmd+a (select all).
  * Backspace / delete / word-backspace (ctrl+bksp) / word-delete
    (ctrl+del).
  * Copy (ctrl/cmd+c), cut (ctrl/cmd+x), paste (ctrl/cmd+v) via the OS
    clipboard.
  * Undo (ctrl/cmd+z) / redo (ctrl/cmd+y or ctrl/cmd+shift+z). 100-entry
    ring; typing bursts coalesce into one undo step (forced break every
    200 chars).
  * Placeholder text.
  * Submit chord: Enter (single-line) or Ctrl/Cmd+Enter (multi-line).
  * `EventEmitter<InputEvent>` for `Submit` / `Changed` — parents
    subscribe via `cx.subscribe(&input, |this, _, ev, cx| { ... })`.
  * Theme-integrated chrome sourced from `wylde_theme::colors::*`.

## What's deferred (with externality reasons)

Each item below is something a complete text input would have but that
gpui at the pinned rev (`b3d93d44`) doesn't make cheap. They are
deferred deliberately — not because they don't matter, but because
shipping them in this slice would burn budget on plumbing instead of
panel coverage.

| Item | Why deferred | Unblocked by |
|------|--------------|--------------|
| Click-to-position cursor / drag-to-select / double-click word / triple-click line | gpui doesn't expose text metrics on a `div`'s child runs at this rev — we can't map a mouse `(x, y)` to a UTF-8 byte offset without building a custom layout pass. Click anywhere focuses the input; cursor positioning is keyboard-only. | A layout-pass slice (or a gpui bump that exposes per-glyph hit testing). |
| IME / dead-key composition input | Platform composition events don't reach arbitrary elements at this rev. Asian-language users get a degraded experience until this lands. | A gpui release that threads composition through user-built widgets. |
| Spell-check underlines | Same as above — platform spell-check API is not exposed. | A gpui release that adds the surface. |
| Accessibility hooks (screen-reader role, announcements) | Same — gpui's element model doesn't thread accessibility metadata for custom widgets. | A gpui release that adds an accessibility surface. |
| Blinking caret | Solid caret only. A per-frame blink loop adds a background task per input — cosmetic, not load-bearing. | A redraw-driven scheduler that doesn't need a spawned task per input. |

## Usage

```rust
use gpui::Entity;
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};

// In your panel's constructor:
let input = cx.new(|cx| {
    TextInput::multi_line(cx)
        .with_placeholder("Send a message")
        .with_submit_mode(SubmitMode::ModEnterSubmits)
        .with_min_height(60.0)
});
let sub = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
    if let InputEvent::Submit(text) = event {
        this.send_message(text.clone(), cx);
    }
});

// Store `input` and `sub` on your panel; render the entity directly:
// .child(self.input.clone())
```

`Subscription` must be held by the panel struct to stay alive — drop the
field and the subscription drops with it.

## Test surface

Pure-Rust buffer in `src/buffer.rs` is exhaustively covered: cursor
movement (grapheme/word/line), selection (extend/collapse), edits
(backspace/delete/word-deletes/insert/paste), undo/redo (snapshot,
ring-cap, dedup), UTF-8 boundary safety.

The View itself has a smaller unit-test surface — most of its behaviour
depends on the gpui event loop which we don't spin up. Compile-time
witnesses pin that the `Render` and `EventEmitter` impls are intact.
