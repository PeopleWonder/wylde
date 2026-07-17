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
   the top of `call_with_deadline` / `stream_call`. With the feature off
   (every normal build) the module and hooks don't exist. (`call` is a thin
   wrapper that forwards the default `RESPONSE_TIMEOUT` to
   `call_with_deadline`, so calls made through either entry point hit the
   same seam; deadline-tuned callers like `workspaces.reindex` use
   `call_with_deadline` directly.)

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

### The other seam: fixture pipes (`PipeNameOverride`)

`ScriptedBackend` short-circuits *before* the transport, which is what you want
for a panel test — but it means a test that exists to verify the **wire format
itself** (msgpack framing, length prefix) can't use it. Those tests stand up a
real named-pipe server and need a real pipe.

They must bind a **private** one. Binding `\\.\pipe\wylde-<service>` claims the
endpoint the live service owns, so the test fails with `ERROR_ACCESS_DENIED` /
`ERROR_PIPE_BUSY` on any machine actually running Wylde — while passing in CI,
which never runs the stack. That inverted flake is #75; `integration_graph_ipc`
had it.

```rust
use wylde_gui_pipe::test_backend::{unique_pipe_name, PipeNameOverride};

let pipe = unique_pipe_name(SERVICE);                    // per-process name
let _route = PipeNameOverride::install(SERVICE, &pipe);  // reverts on drop
```

`pipe_name()` consults the override, so `wylde_gui_pipe::call` targets the
fixture for the life of the guard. Unlike the fake backend this override is a
**process-global**, not a thread-local — the real transport connects on a tokio
worker, not the thread that installed it, and a thread-local would silently not
apply there. The lookup is `#[cfg(feature = "test-support")]`, so the shipped
Shell has no override path at all: no env var, no runtime switch.

`Workspaces/tests/fixture_pipes_are_private.rs` enforces this by scanning the
GUI tree for literal production binds — a *static* check because CI, having no
live stack, structurally cannot observe the failure. The `rust/` workspace has
followed the equivalent convention since #29 (`unique_service_name()` plus the
`WYLDE_LIFECYCLE_PIPE_NAME` / `WYLDE_WORKSPACES_PIPE_NAME` / `WYLDE_LSP_PIPE_NAME`
service-side overrides).

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
| `.on_path(path, json)` | path-routed call → `Ok(json)` — for the **action-less** panels (RemoteAccess issues `GET /api/link/*` with no `"action"` envelope) |
| `.on_path_err(path, "code: msg")` | path-routed call → `Err` |
| `.on_stream(action, vec![chunk, …])` | streaming `action` replays chunks then ends |
| `.conversations(rows)` | shortcut for `conversations.list` |
| `.calls()` / `.calls_for(a)` / `.last_call_for(a)` / `.count_for(a)` / `.count_for_path(p)` | inspect recorded calls |
| `RecordedCall::payload_str(k)` / `.workspace_id()` | read a payload field |

Routing order: action-error → action-ok → path-error → path-ok → soft default
(`Ok({})`). Action maps only match when the call carries a `body["action"]`;
the action-less HTTP-style calls (RemoteAccess) fall through to the path maps.

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

**The L7 panel-walk (`tests/panel_walk.rs` in every panel crate — issue #35).**
The Tier-B answer to "does *every* page load?" Each of the 9 panels (and the
Workspaces subtabs) has a `panel_walk.rs` that mounts the real view the way the
Shell does (`new` + the panel's `spawn_*` loader) and asserts it loads without
panic and isn't in a wrong/stuck error state — under **four backend
conditions**: healthy, backend **down** (every call `on_err` — the daemon-in-
no-spawn-mode case), backend **error envelope**, and **empty** (the default
fake's `Ok({})` — degraded services answer ok/empty, not errors). "Error state"
is per-panel and read from the code, not a uniform notion: Models/Tools/Devices/
Memory/Workspaces expose `error: Option<String>` + a `loading` flag;
RemoteAccess uses `last_error` (only status failures surface); Dashboard has no
error field and *degrades per card* (assert `initial_load_done` + per-service
`HealthStatus`); Settings degrades every section to defaults (`voice_offline`
flags the optional voice service). Run the whole gate with **`cargo panel-walk`**
(from `Core/GUI/`); it runs headless in CI as the `gui panel-walk (L7)` job.

> ### ⚠ `cargo test --workspace` does not run these tests
>
> The windowed gpui tests sit behind a required feature (`test-support`, enabled
> by the `panel-walk` alias). A plain `cargo test --workspace` from `Core/GUI/`
> compiles and reports **`0 passed` for all 8 binaries** — it does not skip
> loudly, it does not error, it **looks green while testing nothing.**
>
> That is a trap worth naming: the habit of "run `--workspace`, see green" is
> correct in `rust/` and silently wrong here. It was hit during the KI-6
> enumeration (2026-07-17) and nearly caused the GUI tree to be reported clean
> without a single GUI test having run. **Always use `cargo panel-walk`.**

**Covered (behavioural, panel-specific):** `tests/dock_scoping.rs` — the docked
ChatPanel's enter→scoped list / leave→restore, docked turn carries
`workspace_id`, Global stays workspace-free (D1), the three C6 empty-state enter
cases; plus `conversations.rs`, `virtualization.rs`, `processing_indicator.rs`
(Chat), `copy_in.rs` (Memory), `cancel_pairing.rs` (Devices), `prefs_dispatch.rs`
(Settings), and the Workspaces subtab suites.

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
