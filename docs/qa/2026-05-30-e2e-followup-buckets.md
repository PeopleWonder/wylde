# E2E GUI follow-up — closing the −1.5 (2026-05-30)

Follow-up slice to the E2E GUI pass that read **Chat InferenceBar = GO; overall
GUI confidence 8.5/10**. The −1.5 split into three buckets; this slice closes
all three. This file is the QA evidence for **bucket 3** (the live-display
items) and the index for the other two.

> **Live-capture status (read this first).** Buckets 1 and 2 are fully closed
> in code and pinned by automated tests. Bucket 3's three items are
> **code-verified and patched**; the *visual* screenshots still need to be
> taken on a machine with a display. This session ran headless on a remote
> (phone) operator with a hard "never trigger UAC / never drive the desktop"
> constraint, so I could not capture the window images myself. The script in
> [`capture-bucket3.ps1`](./capture-bucket3.ps1) launches the updated
> `wylde-gui` and walks the three scenarios so the operator can drop the PNGs
> into this folder. Each item below states exactly what to look for.

---

## Bucket 1 — Settings panel write verbs (closed)

The Settings toggles were display-only. Now every control persists and
round-trips:

| Control | Verb | Side | Notes |
|---|---|---|---|
| Updates → Check for updates | `updater.set_prefs {enabled}` | lifecycle (Python) | new verb |
| Updates → Check automatically | `updater.set_prefs {auto_check}` | lifecycle (Python) | new verb |
| Updates → Frequency | `updater.set_prefs {frequency}` | lifecycle (Python) | cycles weekly→daily→monthly |
| Startup → Launch at login | `set_autostart_enabled` | OS (`auto-launch`) | already existed |
| Tool permissions → Skip every prompt | `consent.set_no_auth` | harness (Rust) | already existed |
| Tool permissions → per-tool row | `consent.set` (approved⇄denied) | harness (Rust) | already existed |
| Tool permissions → Reset all | `consent.reset` | harness (Rust) | already existed |

New backend verbs `updater.get_prefs` / `updater.set_prefs` live in
`Core/Lifecycle/updater_prefs.py` (registered from `control.py`), persisting to
`data/preferences/updater.json` — the same `data/preferences/` dir the harness
uses for `consent.json`. Kept out of `control.py` so that file stays under the
700-line `file_size_limit` cap. Privacy-first: the daemon never performs an
update check; the prefs only record stated intent.

Manifest `required_services` is unchanged (`["wylde-harness"]`): the consent
calls target `wylde-harness` (already declared) and the updater calls go through
the `wylde_gui_pipe::lifecycle_action` helper, which rule
`required_services_includes_called_services` does not treat as a raw
`pipe::call`.

**Tests:** 15 Python unit tests (`Core/Lifecycle/tests/test_updater_prefs.py`)
+ 16 Settings-crate Rust tests (incl. the frequency-cycle and tool-flip
witnesses).

## Bucket 2 — Cold-start consent reconnect (closed)

`ChatPanel::spawn_consent_subscription` previously subscribed to
`consent.stream_pending` exactly once; if the harness pipe wasn't up yet (the
cold-start race), it set an error and **returned** — the stream stayed dead and
every later pending-tool prompt was silently dropped, stalling the turn.

It now runs a single long-lived task that **reconnects with a capped
exponential backoff** (250 ms → 5 s) on both a failed subscribe and a
mid-flight stream error, exiting only when the panel entity is torn down. On a
successful reconnect it clears the cold-start error and resets the backoff.

**Test:** `consent_reconnect_backoff_doubles_and_caps` pins the schedule
(non-zero floor, doubling, saturating ceiling) — the pure, unit-testable part
of the policy. (gpui has no headless executor in-tree to drive the spawn loop.)

---

## Bucket 3 — Live-display QA (code-verified + patched)

> Re-read of the report scope: the named live-display items are **three** —
> (a) 10k-char paste wrapping, (b) dropdown focus-restore, (c) resize-mid-stream.
> There is no genuine fourth item; the prompt itself hedged "if there were
> genuinely four." Documented here so the next pass doesn't hunt for a phantom.

### (a) 10k-char paste — horizontal wrapping in the InferenceBar — **PATCHED**

**Defect (code-confirmed).** `wylde-gpui-input`'s `content_node` rendered each
logical line as a `flex_row` of text spans. A 10k-char paste with no newline is
**one** logical line, so it became a single unbounded `flex_row` child that took
its full intrinsic width — overflowing horizontally and blowing out the bar's
layout. gpui at `b3d93d44` exposes no `overflow_x_*`, and true per-glyph
soft-wrap with inline carets needs text metrics the rev doesn't surface.

**Fix.** `Core/GUI/Frontend/Input/src/lib.rs`:
- The input root and the content column are now `w_full()` (bounded width to
  wrap against).
- A line with **no** caret/selection split renders as a single **width-bounded
  text block** — a direct string child of a `w_full()` element soft-wraps to the
  input width. This is the case for a pasted blob you're looking at.
- The focused split path (inline caret/selection) is `w_full()` + `flex_wrap()`
  + `overflow_hidden()`, so even a single oversized run can't push the layout
  wide.

Regression anchor: `long_no_newline_paste_stays_one_logical_line` pins the
buffer precondition the wrap path depends on.

**Look for (screenshot `bucket3-a-paste.png`):** paste ~10k chars with no
newline into the chat prompt. The text should wrap to multiple lines inside the
bar and the window width should not change. ✗ = a single line running off the
right edge / the window growing.

### (b) Dropdown focus-restore after picker close — **PATCHED**

**Defect (code-confirmed).** `ChatPanel::select_model` / `select_workspace`
closed the dropdown but never returned keyboard focus to the prompt input —
focus stayed on the (now-hidden) dropdown row, so the next keystroke went
nowhere until the user clicked back in.

**Fix.** `Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs`: both selectors now
take the `&mut Window` already available in their click listeners and call a new
`focus_prompt` helper that focuses `prompt_input`'s handle on pick.

**Look for (screenshot `bucket3-b-focus.png`):** open the model pill, click a
model; without touching the mouse again, type — characters should appear in the
prompt. ✗ = you have to click the input first.

### (c) Resize-mid-stream — **VERIFIED CLEAN (no fix needed)**

**Analysis (code).** Streaming state (`messages`, the streaming bubble,
`active_turn_id`) lives in the panel entity and is driven by the chunk task,
fully decoupled from layout. gpui re-renders every frame, so a resize just
reflows. Message bubbles use `max_w(px(560.0))` and the column `max_w(px(720.0))`
with `flex_1` — fluid below the cap, no fixed `.w(px(..))` that would clip. The
bucket-3a change additionally makes the InferenceBar `w_full()`, so it tracks
the window width under resize.

**Look for (screenshot `bucket3-c-resize.png`):** start a turn; while tokens are
streaming, drag the window narrower and wider. Bubbles should reflow to the new
width and tokens should keep arriving without truncation or freeze.

---

## Verification run (this slice)

- `wylde_check.run_all()` → **43 rules, 0/0/0** (error/warning/info).
- `cargo test --workspace` (Core/GUI) → **424 passed, 0 failed**.
- `cargo clippy --workspace --all-targets -- -D warnings` → clean.
- `pytest Core/Lifecycle/tests/` → **74 passed** (incl. 15 new updater tests).

## Screenshots

Drop the three PNGs here once captured:

- `bucket3-a-paste.png`
- `bucket3-b-focus.png`
- `bucket3-c-resize.png`

See [`capture-bucket3.ps1`](./capture-bucket3.ps1).
