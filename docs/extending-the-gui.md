---
title: Extending Wylde — adding GUI workflows and panels
audience: contributors adding GUI functionality, extension authors with GUI surface
authored: 2026-05-27
updated: 2026-05-29 — rewritten for the gpui cutover (slice 11)
status: living reference; future-state sections marked DESIGN
---

# Extending the GUI

## Executive summary

Wylde's user interface is a normal desktop app. It looks and feels like
any other window — you have a sidebar, panels for chat, tools, memory,
training, workflows; you click buttons; the app does things. Behind the
scenes it's a native Rust desktop app built on
[gpui](https://github.com/zed-industries/zed) (the GPU-rendered UI
framework from the Zed editor); each panel is a gpui `View`, and the
buttons all eventually pull on the same backend services that the AI
model uses. When you click "search my notes" in the GUI, the exact same
code runs as when the AI asks itself to search the notes — that's the
whole point of having one shared registry of actions.

There are two ways to "extend the GUI." The common one is **add a new
workflow**: a page or component that lets the user trigger something
the backend can already do (e.g. a "Reflect on yesterday's chats" button
that calls into the memory service). The harder, future one is **let an
external plugin contribute its own panel** so things like the N8N
workflow editor can live as a first-class tab inside Wylde instead of
popping up in a separate window. Today only the first works; the second
is scoped but unbuilt.

This doc covers both. The "Today" section is the practical recipe — you
add a verb to the registry, call it from Rust via `wylde-gui-pipe`, and
render it in a gpui panel `View`. The "DESIGN" section sketches the
future extension UI-panel API beyond what slice 12.7 already shipped.
Read the DESIGN section if you're planning that work; skip it otherwise.

## How it works

There are two kinds of "extend the GUI" requests:

1. **Add a new workflow** that uses existing services (e.g. a "Reflect on
   yesterday's chats" button that calls `memory.reflect` + renders the
   summary). This is the common case — a gpui panel `View` (or a widget on
   an existing one) plus a pipe verb. Today.
2. **Let an out-of-box extension contribute a GUI panel** (e.g. N8N's
   workflow editor as a first-class panel inside Wylde rather than a popup
   window). DESIGN-only — not built. Documented here so the shape is known.

## How to extend (today)

### Find the right pipe verb

Every backend mechanic the GUI uses already has a pipe verb. The verb list
lives in `data/contracts/actions/<service>.json` (rule 9 lints GUI calls
against it). For the harness specifically, the canonical list is in
`rust/crates/wylde-harness/src/pipe.rs::ALL_PIPE_ACTIONS`:

* `chat.*` — turn driver (run, start, cancel, stream, stream tools)
* `tools.list` / `tools.run` — direct tool invocation by id
* `memory.long_term.*` — list / save / update / delete / history
* `memory.workspaces.*` — registry, MRU cap, persona

For services beyond the harness:

* `wylde-lifecycle` — `service.list`, `service.start`, `service.stop`, `service.health`
* `wylde-gateway` — HTTP routes (browser uses these directly via fetch)
* `wylde-voice` — `voice.transcribe`, `voice.synthesize`, streaming variants
* `wylde-extension-bridge` — `ext.list`, `ext.tools.list`, `ext.tools.call`, …
* `wylde-vram-broker` — `lease.acquire`, `lease.release`

If the verb you need doesn't exist, the right move is usually to add an action
to the registry (see [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md))
rather than to add a one-off pipe handler. The registry-first path means the
LLM gets the same capability for free.

### Wire the GUI call

The GUI talks to the Wylde pipes directly from Rust through the
`wylde-gui-pipe` crate (`Core/GUI/Frontend/Pipe/`) — there is no
JavaScript bridge and no over-the-wire hop for unary harness verbs. The
crate exposes `wylde_gui_pipe::call(service, http_verb, path, body)` as the
catch-all wire client, plus a `try_dispatch_harness_default(verb, payload)`
in-process short-circuit that runs harness verbs against the
`wylde_harness::HarnessApi` trait without touching a pipe (Phase 12.1). For
streaming verbs there's `wylde_gui_pipe::stream_call(...)`, which returns a
`PipeStream` that cancels on drop.

The convention is a small per-panel `ipc.rs` adapter that wraps the bare
verbs into typed reads/writes the `View` consumes (see
`Core/GUI/Frontend/Panels/Tools/src/ipc.rs` for the canonical shape). For
harness verbs, prefer the in-process path:

```rust
// Core/GUI/Frontend/Panels/<YourPanel>/src/ipc.rs
use serde_json::{json, Value};

/// List the tool catalog via the harness `tools.list` verb. Harness verbs
/// run in-process through `try_dispatch_harness_default`; the wire path is
/// only the fallback for verbs the short-circuit doesn't know.
pub async fn list_tools() -> Result<Value, String> {
    match wylde_gui_pipe::try_dispatch_harness_default("tools.list", Value::Null).await {
        Some(result) => result,
        // Unknown / streaming verb — fall through to the wire.
        None => {
            wylde_gui_pipe::call(
                "wylde-harness",
                "POST",
                "/__action__",
                Some(json!({ "action": "tools.list", "payload": null })),
            )
            .await
        }
    }
}

/// Run a tool by name. Same dual path; the `device_tier` rides in the payload.
pub async fn run_tool(name: &str, args: Value, device_tier: &str) -> Result<Value, String> {
    let payload = json!({ "name": name, "args": args, "device_tier": device_tier });
    match wylde_gui_pipe::try_dispatch_harness_default("tools.run", payload.clone()).await {
        Some(result) => result,
        None => {
            wylde_gui_pipe::call(
                "wylde-harness",
                "POST",
                "/__action__",
                Some(json!({ "action": "tools.run", "payload": payload })),
            )
            .await
        }
    }
}
```

Services other than the harness (e.g. `wylde-extension-bridge`) have no
in-process short-circuit — every call goes over the wire via
`wylde_gui_pipe::call(...)`, exactly as `Tools/src/ipc.rs` does for the
`ext.*` verbs.

In a gpui panel `View`, kick the IPC read off with `cx.spawn` and update the
`View`'s state when it resolves (the framework re-renders on `update`):

```rust
use gpui::{div, prelude::*, AsyncApp, Context, Render, Window};
use serde_json::Value;
use crate::ipc::list_tools;

pub struct ToolsPanel {
    tools: Vec<Value>,
    error: Option<String>,
}

impl ToolsPanel {
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_tools().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(reply) => {
                        panel.error = None;
                        panel.tools = reply["tools"].as_array().cloned().unwrap_or_default();
                    }
                    Err(err) => panel.error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
```

Each first-party panel is its own workspace-member crate under
`Core/GUI/Frontend/Panels/<Name>/`, and its manifest `factory:` string
resolves to a gpui `View` constructor (enforced by `wylde_check` rule
`first_party_manifest_must_be_gpui_view`).

### Lint discipline

`wylde_check` rule 9 (`gui_action_contract`) checks that every
`wylde_gui_pipe::call` / `try_dispatch_harness*` invocation hits a real
handler in the corresponding service's action contract
(`data/contracts/actions/<service>.json`). Adding a new GUI call against a
non-existent action will fail the gate. Either register the action first or
change the call.

The gpui-era rules are stricter about GUI boundaries. Notably:

* `no_cross_panel_imports` — a panel crate may not import another panel
  crate; cross-panel navigation goes through `wylde-gui-pipe`'s `nav_bus`.
* `no_legacy_gui_imports_in_panels` — panels must not reach back into the
  deleted Tauri/Svelte surface.
* `webview_only_in_extension_handlers` — only the `wylde-webview` crate may
  pull in `wry`.
* `first_party_manifest_must_be_gpui_view` /
  `panel_crate_must_be_workspace_member` — a first-party panel is a gpui
  `View` and a member of the `Core/GUI/` workspace.
* `stream_call_must_handle_cancel` — any `stream_call` consumer must wire up
  cancellation (the `PipeStream` aborts on drop).

The inference loop is still **not** in the GUI: new GUI code should call
`chat.run_turn` / `chat.stream_turn` and let the harness own the loop.

### Test it

`cargo run -p wylde-gui` from `Core/GUI/` launches the app; iterate against a
live harness. Because the whole client is Rust, panel logic is testable the
usual way — put `#[test]` / `#[tokio::test]` units in your crate (the
`ipc.rs` adapters in `Tools/`, `Settings/`, and `Workspaces/` carry payload-
parsing and "daemon-down surfaces a structured error" tests you can mirror).
Pure-function pipe helpers (envelope shaping, error projection) are covered in
`Core/GUI/Frontend/Pipe/src/lib.rs`'s test module without needing a live
daemon.

## SHIPPED: in-process harness dispatch (Phase 12.1, 2026-05-27)

The Phase 9 pipe handlers used to live in `rust/crates/wylde-harness/src/pipe/`
(4 files, ~20 active verbs). Phase 12.1 picked the **Option A — in-process
trait** path: the harness exposes a `HarnessApi` trait that both the
standalone harness binary's pipe and the GUI's pipe layer dispatch against, so
the GUI no longer takes the IPC hop for the unary verbs.

Phase 12.1 originally landed the GUI-side dispatcher inside the (now-deleted)
Tauri shell. At the gpui cutover (slice 11, 2026-05-29) that module moved
verbatim into the `wylde-gui-pipe` crate; the trait and the in-process
short-circuit are *cleaner* in a single Rust binary — there's no
Tauri-command wrapping, just direct trait calls.

### Where things live

**The trait** lives at
[`rust/crates/wylde-harness/src/api.rs`](../rust/crates/wylde-harness/src/api.rs)
(`HarnessApi`, default impl `DefaultHarnessApi`). One method per verb in
`ALL_PIPE_ACTIONS`. The JSON-shaping that used to live in `pipe/tools.rs` and
`pipe/memory_long_term.rs` moved into the trait's default impl so both the
harness binary and the GUI share one implementation.

**The harness binary's pipe registration** is a single file
[`rust/crates/wylde-harness/src/pipe.rs`](../rust/crates/wylde-harness/src/pipe.rs)
— `install_all_against(api)` registers each verb on the global IPC registry
as a thin closure that calls a trait method. The 4 sub-files
(`pipe/chat.rs`, etc.) are gone; their behaviour now lives behind the trait.
The harness binary still listens on `\\.\pipe\wylde-harness` for non-GUI
clients (MCP, CLI, parity tests).

**The GUI-side dispatcher** is the `wylde-gui-pipe` crate at
`Core/GUI/Frontend/Pipe/`:
* `lib.rs` — the wire client (`call` / `stream_call` / `list_wylde_pipes`)
  plus `try_dispatch_harness(api, verb, payload)` and the convenience
  `try_dispatch_harness_default(verb, payload)`, which fan out to:
* `chat.rs`, `tools.rs`, `memory_long_term.rs`, `memory_workspaces.rs` —
  each a verb-name → trait-method routing table. Each returns `None` for
  unknown verbs so callers can fall through to the wire.

Panel code calls `try_dispatch_harness_default` directly (Rust → Rust); if
the verb is unrecognised (or streaming), it falls back to
`wylde_gui_pipe::call(...)` over the wire. There is no JavaScript surface;
`reply_to_result` projects the in-process `Reply` into the same
`Result<Value, String>` shape the wire path returns, byte-identical, so
callers can't tell which path served them.

### Cargo deps

`wylde-gui-pipe`'s `Cargo.toml` depends on:
* `wylde-harness = { path = "../../../rust/crates/wylde-harness" }`
* `wylde-shared = { path = "../../../rust/crates/wylde-shared" }`

This pulls the harness's full dep tree (tokio, neo4rs, sha2, bincode, etc.)
into the GUI binary. Because `Core/GUI/` is its own Cargo workspace
(deliberately not nested in the backend `rust/` workspace), gpui's heavy
graphics deps don't ripple back into the backend lockfile.

The shared IPC registry (`wylde_shared::ipc`) is still process-local — the
harness binary and the GUI process each have their own. Disk-backed state
(long-term memory store, workspaces registry) IS shared across both;
process-local state (the turn registry) is not, but that's fine because the
two processes serve different audiences (GUI users vs MCP/CLI clients).

### Limitations

* **Streaming verbs stay over the wire.** `chat.stream_turn` and
  `chat.stream_tools` aren't dispatched in-process;
  `try_dispatch_harness_default` returns `None` for them. The GUI reaches
  them via `wylde_gui_pipe::stream_call(...)`, which returns a `PipeStream`
  that cancels on drop.
* **Verbs not yet ported to Rust** (the deferred punchlist in `pipe.rs`)
  keep flowing through the Python pipe via the strangler-fig fallback. The
  in-process dispatch returns `None` for them; the wire path picks them up.

## DESIGN: extension UI panels

The **iframe** transport below shipped in slice 12.7: an extension declares
panels via the `ui_panels` manifest field, the bridge surfaces them through
`extensions.list_panels`, the panel-registry runtime overlay turns each into a
sidebar tab, and the `wylde-webview` crate (`wry`-backed) hosts the URL,
loopback-validated. N8N's workflow editor is the proof-of-life. This section
keeps the original transport comparison and sketches the richer transports
(web component / native panel) that remain unbuilt.

### Panel transport

Three options, in order of escalating ambition:

**Iframe (shipped, slice 12.7)** — The extension's manifest declares a
`ui_panels` entry with a `url`. The GUI host renders it through the
`wylde-webview` crate, which wraps `wry` to create a WebView2/WKWebView child
window parented to gpui's `Window`, runs an HTTP HEAD health probe, and
translates the iframe `sandbox` attrs to wry capability flags. The URL can be
local (a static file served by the extension's MCP server) or external (N8N's
bundled server at `http://127.0.0.1:5678`); loopback-only is enforced. Pros:
zero changes to the extension's existing UI; the N8N case works immediately.
Cons: full browser context — the extension's JS can do anything a WebView can,
including arbitrary fetches.

**Web component / shared-theme bundle** — The extension ships a JS bundle that
the host loads inside a `wylde-webview` instance pre-injected with Wylde's
theme tokens and a thin event bus. More integrated than a raw iframe but
requires extension authors to build against Wylde's component API. Realistic
for first-party extensions, less realistic for third-party. DESIGN-only.

**Native gpui panel** — The extension declares a native panel in its manifest;
the GUI host renders a fully-Wylde-themed gpui `View` populated from the
extension's MCP `resources/` and `tools/` calls (no WebView at all). Best UX,
highest authoring cost — only useful for extensions where the per-extension UI
is a thin presentation layer over MCP data, and it means shipping a gpui crate
rather than a web bundle. DESIGN-only.

**Recommendation:** the iframe path shipped first and is the default. Promote
an extension to a shared-theme bundle or a native gpui panel only if its UX
justifies the authoring cost.

### Manifest surface (proposed)

Add to `mcp-server.json`:

```json
{
  "name": "n8n",
  "transport": "stdio",
  "command": ["node", "wylde-mcp.js"],
  "capabilities": ["egress.web"],
  "ui_panel": {
    "kind": "iframe",
    "url": "http://127.0.0.1:5678",
    "title": "Workflows",
    "icon": "workflow",
    "auth": {
      "kind": "init_script_inject",
      "script_template": "...email/password fill...",
      "secrets_from": "Wylde_passwords:n8n"
    }
  }
}
```

Fields:
* `kind` — `iframe` (shipped) / `web_component` / `gpui_native` (design-only).
* `url` — for iframe; required.
* `title` — sidebar nav label.
* `icon` — icon name, falls back to a default.
* `auth` — optional. The host can inject a one-shot WebView script (via
  `wylde-webview`) to autofill credentials from the password manager.

> The shipped slice-12.7 schema uses the plural `ui_panels` array on the
> extension manifest; the single-object `ui_panel` example above predates
> that and is kept as the conceptual shape. See
> [extending-wylde-extensions.md](./extending-wylde-extensions.md) for the
> live field.

### How the GUI host embeds it

The panel-registry aggregator (`Core/GUI/Manifest/Extension_handlers/`,
`wylde-panel-registry`) reads the bridge's `extensions.list_panels` and folds
each declared panel into the sidebar via the runtime overlay (`gui.list_tabs`).
The Shell's sidebar gets the resulting tabs; clicking an `iframe`-kind entry
mounts a `wylde-webview` instance pointed at the URL inside the main window.

For a future native gpui panel, the manifest would name a gpui `View` factory
(same mechanism first-party panels use) rather than a web bundle, and the
`View` would talk to the extension only through the extension-bridge's
`ext.tools.call` — no direct access to other Wylde services. DESIGN-only.

### Tier-gating implications

An extension's UI panel can see things. Specifically, it can:

* Make arbitrary network requests (in iframe mode — sandboxed by the
  iframe's `sandbox` attribute, but with `allow-same-origin` for the
  extension's own URL).
* Render anything the user types (XSS risk in the extension's own UI is the
  extension's problem, not Wylde's).
* Trigger any MCP `tools/call` the extension has declared.

Things it **cannot** do:

* Call into other extensions' panels.
* Reach the harness pipe, the gateway, or any other Wylde service.
* Read arbitrary filesystem paths outside the extension's declared `cwd`.

The threat model: an iframe-based extension is roughly as trusted as the
extension's MCP server itself. If you wouldn't let the MCP server make
egress calls, don't grant `egress.web` in its capabilities. If you wouldn't
let it touch a particular Wylde service, don't bridge the action through
the extension-bridge. The panel doesn't escalate trust — it just gives the
extension a UI surface for the trust it already had.

Future work: a per-panel CSP allow-list, declared in the manifest, so even
the iframe transport can be locked down to the extension's own origin plus
declared safe domains.

### Open questions

* Multi-window vs in-app tab. The shipped iframe path mounts the panel
  in-app as a sidebar tab. A separate native sub-window remains an option
  (the rewrite plan §6 keeps it as a fallback if the always-on-top WebView
  limitation makes the in-app experience bad). The right default needs UX
  validation — some users may prefer a separate window for screen-estate
  reasons.
* Auth integration. The current `init_script_inject` pattern (email/password
  autofill via JS injection) is fragile. The Wylde passwords extension
  ([docs/wylde-passwords-self-healing-extension.md](./wylde-passwords-self-healing-extension.md))
  has a more robust path via cookie/header injection; consider waiting on
  it before formalising the `auth` block.
* Lifecycle. If an extension is disabled, does its panel disappear from the
  sidebar immediately or stay greyed out? Probably the former, mirroring
  the action surface.

## Cross-links

* [extending-wylde.md](./extending-wylde.md) — audience model overview.
* [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) — adding an
  action; the registry-first path to GUI invocation.
* [extending-wylde-extensions.md](./extending-wylde-extensions.md) — the
  manifest schema, including the shipped `ui_panels` field (slice 12.7).
* `Core/GUI/docs/inference-bar-migration-plan.md` — the historical
  "evacuate the inference loop into the harness" refactor (the GUI no longer
  owns the loop).
* `Core/GUI/Frontend/Pipe/src/lib.rs` (`wylde-gui-pipe`) — the GUI's pipe
  surface: `call`, `stream_call`, `try_dispatch_harness*`, the lifecycle
  helpers.

---

*Today's GUI extension story is "register an action, call it from a gpui panel
via `wylde-gui-pipe`." The richer extension story — "register an action,
declare a `ui_panels` entry, let the GUI host it" — shipped its iframe
transport in slice 12.7; web-component and native-gpui transports remain
design-only.*
