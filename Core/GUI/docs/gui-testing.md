# GUI testing — windowed gpui tests

This is how we machine-test GUI **behavior** (panels mounted in a real gpui
window, driven through their flows) without a running backend stack and
without GUI automation. It exists so the owed "feel-tests" can be retired
incrementally as deterministic, in-repo tests.

The first tests built on it live in
`Frontend/Panels/Chat/tests/dock_scoping.rs` — read them alongside this.

---

## The shape of it

Three pieces, all **dev-only** — none of them reach the shipped Shell binary:

1. **`wylde-gui-pipe`'s `test-support` feature** — adds a thread-local
   injectable backend (`wylde_gui_pipe::test_backend`) and two hook sites at
   the top of `call` / `stream_call`. With the feature off (every normal
   build) the module and hooks don't exist.

2. **`wylde-gui-test-support`** (`Frontend/test-support/`, EXCLUDED from the
   workspace) — a `ScriptedBackend` that answers calls with canned JSON and
   *records* every call, plus an RAII install guard. Only ever a
   `dev-dependency`.

3. **The panels' `dev-dependencies`** — `gpui` with its `test-support`
   feature (for `#[gpui::test]` / `TestAppContext`), `wylde-gui-test-support`,
   and `wylde-gui-pipe` with `test-support`.

### Why this seam (and not a per-panel client trait)

Every panel funnels its IPC through exactly two functions —
`wylde_gui_pipe::call` and `wylde_gui_pipe::stream_call`. Injecting a fake at
that single chokepoint makes **all** panels testable at once with **zero
panel rewrites**, and the canned bytes still flow through each panel's real
`ipc.rs` deserialization. A per-panel `trait IpcClient` would have meant
threading a client through every panel and rewriting call sites for no extra
fidelity.

### Why it can't reach the release binary

- The `test-support` feature is requested **only** from `dev-dependencies`.
  With `resolver = "2"`, dev-dependency features are **not** unified into a
  package's normal lib — so the Shell links `wylde-gui-pipe` and `gpui`
  *without* `test-support`.
- `wylde-gui-test-support` is **excluded** from the workspace (`exclude` in
  `Core/GUI/Cargo.toml`), so even `cargo build --workspace` never builds it
  as a member and can't feature-unify the seam into the Shell.

Verify any time:

```sh
cargo tree -p wylde-gui -e normal -i wylde-gui-test-support   # → "did not match any packages"
cargo tree -p wylde-gui -e normal,features -i wylde-gui-pipe  # → only feature "default"
cargo tree -p wylde-gui -e normal,features -i gpui            # → no "test-support"
```

### Determinism

In gpui test mode the `TestDispatcher` polls every task — foreground and
background — on the thread that calls `run_until_parked`. So:

- the fake backend lives in a **thread-local** → each `#[gpui::test]` gets its
  own backend, isolated even under `cargo test`'s parallelism;
- the fake answers synchronously (no `.await`, no tokio runtime), so a single
  `cx.run_until_parked()` drives every spawned effect to quiescence before you
  assert.

Drive the panel with `apply_*` / `send_*` methods directly rather than the
process-wide buses/singletons (`ChatPanel::docked()`, `publish_active_*`) —
those are shared statics and would leak between tests. The direct methods are
exactly what the bus drains call, so you still test the real logic.

---

## Writing a test

```rust
use gpui::TestAppContext;
use serde_json::json;
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

#[gpui::test]
fn my_panel_does_x(cx: &mut TestAppContext) {
    // 1. Script the backend: action → canned `ok` data. Unscripted unary
    //    actions return `{}` (a soft default the panels tolerate); script
    //    only what the panel reads or what you assert on.
    let fake = ScriptedBackend::new()
        .conversations(vec![/* ConversationMeta rows as JSON */])
        .on("conversations.new", json!({ "id": "c-fresh" }))
        .on("chat.start_turn", json!({ "turn_id": "t1", "conversation_id": "c-fresh" }));
    let _guard = fake.clone().install();   // thread-local; cleared on drop

    // 2. Mount the real view in a test window.
    let window = cx.add_window(|_w, cx| ChatPanel::new(ChatScope::Docked, cx));
    cx.run_until_parked();                 // let mount-time loads settle

    // 3. Drive it. `update` hands you `&mut View, &mut Window, &mut Context<View>`.
    window
        .update(cx, |panel, _w, cx| panel.apply_workspace_scope(Some("ws-a".into()), cx))
        .unwrap();
    cx.run_until_parked();                 // drive the spawned flow to quiescence

    // 4. Assert on observable state (read via `update`; `WindowHandle::read`
    //    wants `&App`, which a `&mut TestAppContext` isn't).
    window
        .update(cx, |panel, _w, _cx| {
            assert_eq!(panel.active_workspace_id.as_deref(), Some("ws-a"));
        })
        .unwrap();

    // 5. Or assert on what the panel SENT.
    let send = fake.last_call_for("chat.start_turn").unwrap();
    assert_eq!(send.workspace_id().as_deref(), Some("ws-a"));
}
```

### `ScriptedBackend` cheat-sheet

| Method | Effect |
|---|---|
| `.on(action, json)` | unary `action` → `Ok(json)` |
| `.on_err(action, "code: msg")` | unary `action` → `Err` |
| `.on_stream(action, vec![chunk, …])` | streaming `action` replays chunks then ends |
| `.conversations(rows)` | shortcut for `conversations.list` |
| `.calls()` / `.calls_for(a)` / `.last_call_for(a)` / `.count_for(a)` | inspect recorded calls |
| `RecordedCall::payload_str(k)` / `.workspace_id()` | read a payload field |

Harness action strings live in each panel's `ipc.rs` (`grep '"action":'`).

### gpui test API available at this rev (`zed` `b3d93d44`)

- `#[gpui::test]` injects `cx: &mut TestAppContext` (and `&mut StdRng` for
  seeded randomized tests).
- `TestAppContext`: `add_window` / `open_window` / `add_empty_window`,
  `run_until_parked`, `dispatch_action`, `simulate_keystrokes` /
  `simulate_input`, `update` / `read`, globals, `background_executor().advance_clock(..)`.
- `WindowHandle<V>`: `update(cx, |view, window, cx| …)` (gives `&mut Context<V>`),
  `read(&App)`, `root(cx)`.
- `VisualTestContext` (from `add_empty_window` / `add_window_view`):
  `simulate_click` / `simulate_mouse_*`, `simulate_keystrokes`, `draw`,
  `debug_bounds(selector)`, `simulate_resize` — for input/hit-testing and
  render-bounds assertions.

---

## What's covered, and what to add next

**Covered** (`tests/dock_scoping.rs`): the docked ChatPanel's enter→scoped
list / leave→restore, docked turn carries `workspace_id`, Global stays
workspace-free (D1), and the three C6 empty-state enter cases.

**Good next windowed tests** (retire more owed feel-tests with the same
recipe):

- **Readiness chip** (Workspaces panel). The *logic* (`Readiness::compute`)
  is already pure-tested in `workspaces_panel.rs`; a windowed test would mount
  `WorkspacesPanel`, inject an `error` / entered-workspace index state, `draw`,
  and assert the chip via `debug_bounds` or by exposing `readiness()` as
  `pub(crate)`.
- **Consent prompt** lifecycle on the Global panel (use `.on_stream(
  "consent.stream_pending", …)`).
- **Conversation switcher** select / new / delete on a docked dock.
- **Input-driven** sends via `VisualTestContext::simulate_keystrokes` to cover
  the TextInput → submit path end-to-end (vs. calling `send_user_message`).
