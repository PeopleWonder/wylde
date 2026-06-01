---
title: Extending the Wylde GUI — adding a panel
audience: anyone (human or LLM) wiring a new tab into the Wylde left nav
authored: 2026-05-28
updated: 2026-05-30
status: living reference; pairs with wylde-gui-panel-architecture-plan.md
---

# Extending the Wylde GUI

This doc is the recipe. Read
[wylde-gui-panel-architecture-plan.md](./wylde-gui-panel-architecture-plan.md)
if you want the why; this doc is the how.

> **Post-cutover note.** The Wylde GUI is a native **gpui** (Rust)
> desktop app as of the slice-11 cutover (2026-05-29). The old
> Tauri 2 + Svelte 5 client — `Core/GUI/src/` and `Core/GUI/src-tauri/`,
> npm/Vite, custom-element panels, `npm run tauri dev` — was deleted.
> Panels are now gpui **Views**, not Svelte components. If you have an
> older copy of this guide that teaches the `<svelte:options
> customElement>` workflow, it is obsolete; follow this one. The
> migration rationale lives in
> [wylde-gpui-rewrite-plan.md](./wylde-gpui-rewrite-plan.md).

A **panel** is one entry in the Wylde left nav. Clicking it swaps the
main content area (the "slot") to your UI. Two kinds exist:

- **gpui View** — your panel is a Rust crate that compiles a
  `gpui::Render` View. The Shell mints the View into its panel slot.
  There is no CSS, no Shadow DOM, no DOM at all — styling is local to
  each element. This is the only path for first-party (Core) panels.
- **Iframe** — your service has its own webserver running on loopback.
  The Shell mounts a native `wry` WebView (WebView2 / WKWebView /
  WebKitGTK) as a child of the gpui window, sized to the slot. Use this
  for N8N, anything with a mature browser UI you don't want to rebuild.
  Iframe panels are the path extensions use; see the alternate recipe
  near the end.

Pick **gpui View** by default. Pick **iframe** only when you have an
existing browser UI you can't rebuild (N8N) or your panel genuinely
needs to run in a WebView.

---

## Operating rules

1. First-party panels live under `Core/GUI/Frontend/Panels/<Name>/`,
   one sub-crate per panel. Don't reach into another panel's crate.
2. No cross-panel imports. Each panel ships its own `ipc.rs` and holds
   its own state. The one shared crate is `wylde-theme` (colour/type
   tokens); importing it is encouraged so the visual identity stays
   cohesive, but a panel may declare its own colour constants instead.
3. Loopback only. Iframe URLs must be `http(s)://127.0.0.1`,
   `localhost`, or `[::1]`. The manifest parser refuses anything else
   at build time, and the runtime overlay refuses it again at load.
4. Cross-platform. Use forward slashes in manifest paths; the
   aggregator normalises them anyway. Don't bake in `\\?\` or `C:\`.
5. A panel owns only its own View tree and may only call pipe verbs.
   It must not write the window title, push to the toast queue, or hold
   a handle to the sidebar — the Shell owns those. Rust's borrow checker
   enforces most of this; `wylde_check` enforces the rest (see the
   anti-patterns section).

---

## Step 1 — make your panel crate

For a Core panel, add a sub-crate under `Core/GUI/Frontend/Panels/`:

```
Core/GUI/Frontend/Panels/PhotoOrganize/
  Cargo.toml
  manifest.json
  src/
    lib.rs
    photo_panel.rs        the View + the `view` factory fn
    ipc.rs                per-panel pipe-call helpers
    state.rs              (optional) reactive state types
```

This mirrors the shape of every existing panel
(`Core/GUI/Frontend/Panels/Settings/`,
`Core/GUI/Frontend/Panels/Memory/`, …). Forward slashes everywhere,
`PascalCase` directory name to match the existing panels.

If your panel belongs to a non-Core service (rare today; the canonical
home is `Core/GUI/Frontend/Panels/`), the per-service variant lives at
`<Service>/Frontend/` with a `manifest.json` — the build-time
aggregator scans both `Core/GUI/Frontend/Panels/*/manifest.json` and
`<Service>/Frontend/manifest.json` under `Core/` and `Services/`. See
`Core/GUI/Manifest/Extension_handlers/src/bin/wylde_panel_aggregator.rs`
for the exact discovery roots.

---

## Step 2 — write `Cargo.toml`

A first-party panel is a library crate that pulls in gpui, the theme,
and the pipe surface:

```toml
[package]
name = "wylde-panel-photo-organize"
description = "Wylde Photos panel — gpui View over the photo-organize pipe."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true

[lib]
path = "src/lib.rs"

[dependencies]
# Colour + typography tokens. The one deliberately shared crate.
wylde-theme = { path = "../../Theme" }

# Pipe surface — reach the harness/services over the wire (and via the
# in-process HarnessApi short-circuit for harness verbs).
wylde-gui-pipe = { path = "../../Pipe" }

# gpui itself, pinned through the workspace dep so a rev bump is a
# single-file change.
gpui.workspace = true

serde.workspace      = true
serde_json.workspace = true
anyhow.workspace     = true
```

`gpui.workspace = true` resolves to the pinned git rev in
`Core/GUI/Cargo.toml` (`rev = "b3d93d44"` today). Never re-pin gpui in
a panel crate; always inherit the workspace pin so the whole GUI moves
together when the rev bumps.

---

## Step 3 — write `manifest.json` (schema v2)

The minimal gpui-View manifest:

```json
{
  "schema_version": 2,
  "service": "core",
  "panels": [
    {
      "id": "photos",
      "title": "Photos",
      "icon": "image",
      "order": 75,
      "version": "0.1.0",
      "required_services": ["wylde-harness"],
      "source": {
        "kind": "gpui_view",
        "factory": "wylde_panel_photo_organize::PhotoPanel::view"
      }
    }
  ]
}
```

Rules (parser enforced — see
`Core/GUI/Manifest/Extension_handlers/src/manifest.rs`):

- `schema_version` is mandatory and must be `2`. Anything else fails
  loud at parse. (v1 was the deleted Svelte custom-element schema.)
- `source.kind` is `"gpui_view"` for first-party panels or `"iframe"`
  for the WebView path. The old `"custom_element"` value is gone; the
  parser rejects it as an unknown variant.
- `source.factory` is a path-like string `crate::Type::method`. The
  parser sanity-checks that it contains `::`. It must resolve to a Rust
  function with the View-factory signature (Step 4) and be wired in
  `factories.rs` (Step 6).
- `order` slots your panel into the left nav. See the ordering table
  below; the existing Core panels run Chat=5, Dashboard=8, Memory=20,
  Workspaces=30, Models=40, Tools=50, Devices=60, RemoteAccess=65,
  Photos≈75, Images=80, Training=85, Settings=95.
- `required_services` lists pipe-service slugs that must be healthy
  before the Shell mounts your panel. If any are down the slot shows a
  "Required service not running" stub with a per-service "Start
  service" button instead of mounting the View (see
  `Core/GUI/Shell/src/slot.rs`).
- `icon` is a Lucide icon name (the Shell's icon set's lookup key).

---

## Step 4 — write your panel as a gpui View

A panel is a struct that implements `gpui::Render`, plus a
`view(window, cx) -> AnyView` factory the registry calls to mint it.
The factory signature is fixed:

```rust
pub fn view(window: &mut gpui::Window, cx: &mut gpui::App) -> gpui::AnyView
```

(matching `ViewFactory` in
`Core/GUI/Manifest/Extension_handlers/src/registry.rs`).

The Settings panel
(`Core/GUI/Frontend/Panels/Settings/src/settings_panel.rs`) is the
reference. A minimal version:

```rust
// Core/GUI/Frontend/Panels/PhotoOrganize/src/photo_panel.rs
use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, Context, IntoElement,
    Render, SharedString, Window,
};
use wylde_theme::colors::{SURFACE_900, TEXT_PRIMARY, TEXT_SECONDARY};
use wylde_theme::typography::{size, FAMILY_INTER};

use crate::ipc::{list_photos, Photo};

pub struct PhotoPanel {
    pub photos: Vec<Photo>,
    pub loading: bool,
}

impl PhotoPanel {
    pub fn new() -> Self {
        Self { photos: Vec::new(), loading: true }
    }

    /// The factory named by `manifest.json`'s `factory:` string
    /// (`wylde_panel_photo_organize::PhotoPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            // Kick off the panel's own data load; the View redraws
            // when the spawned task calls cx.update / cx.notify.
            Self::spawn_refresh(cx);
            panel
        })
        .into()
    }

    fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            if let Ok(photos) = list_photos().await {
                let _ = this.update(cx, |panel, cx| {
                    panel.photos = photos;
                    panel.loading = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for PhotoPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .size_full()
            .bg(rgb(0x0a0e17)) // or pack(SURFACE_900); see the theme note
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .font_family(FAMILY_INTER)
            .text_color(rgb(0xe2e8f0))
            .child(
                div()
                    .text_size(px(size::LG))
                    .text_color(rgb(0xe2e8f0))
                    .child(SharedString::from("Photos")),
            );

        if self.loading {
            root = root.child(
                div()
                    .text_color(rgb(0x94a3b8))
                    .child(SharedString::from("Loading…")),
            );
        } else if self.photos.is_empty() {
            root = root.child(
                div()
                    .text_color(rgb(0x94a3b8))
                    .child(SharedString::from("No photos indexed yet.")),
            );
        } else {
            // gpui has no CSS-grid primitive. For small static grids use
            // flex_wrap + fixed child widths; for large lists use
            // gpui::uniform_list (the existing Memory / Tools panels do
            // this). See the rewrite plan §4.3.
            let mut grid = div().flex().flex_wrap().gap_2();
            for p in &self.photos {
                grid = grid.child(
                    div()
                        .w(px(120.0))
                        .child(SharedString::from(p.caption.clone())),
                );
            }
            root = root.child(grid);
        }

        root
    }
}
```

Notes:

- **Theme tokens.** `wylde-theme` exposes colour constants
  (`wylde_theme::colors::SURFACE_900`, `TEXT_PRIMARY`, `BRAND`, …) and
  typography helpers (`wylde_theme::typography::{size, weight,
  FAMILY_INTER}`). The colour constants are `gpui::Rgba`; existing
  panels pass them to `.bg(...)` / `.text_color(...)` via the Shell's
  `pack()` helper (`rgb(pack(SURFACE_900))`) — look at
  `Core/GUI/Shell/src/slot.rs` for the exact idiom in use. Prefer the
  tokens over hardcoded hex so a palette change touches one file.
- **State + redraw.** Hold state as plain fields on the View struct.
  When async work updates it, call `cx.notify()` inside
  `this.update(cx, …)` to trigger a re-render. This is gpui's
  reactive-entity model; there is no `$state` rune.
- **No lifecycle hooks.** There is no `onMount`. The factory itself is
  where you kick off the initial data load (as `view` does above via
  `spawn_refresh`).

Re-export the View and factory from `src/lib.rs`:

```rust
// Core/GUI/Frontend/Panels/PhotoOrganize/src/lib.rs
pub mod ipc;
pub mod photo_panel;
pub use photo_panel::PhotoPanel;
```

---

## Step 5 — write your IPC client (`ipc.rs`)

Each panel ships its own thin adapter over the pipe surface. The
universal caller is `wylde_gui_pipe::call`:

```rust
pub async fn call(
    service: &str,
    http_verb: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String>
```

The convention is the `/__action__` envelope
(`{ "action": "<verb>", "payload": <value> }`), matching what the
Settings panel does
(`Core/GUI/Frontend/Panels/Settings/src/ipc.rs`):

```rust
// Core/GUI/Frontend/Panels/PhotoOrganize/src/ipc.rs
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Photo {
    pub id: String,
    pub caption: String,
}

pub async fn list_photos() -> Result<Vec<Photo>, String> {
    let v = wylde_gui_pipe::call(
        "wylde-photo-organize",         // your service's pipe name
        "POST",
        "/__action__",
        Some(json!({ "action": "photos.list", "payload": {} })),
    )
    .await?;
    Ok(parse_photos(&v))
}

pub async fn index_folder(path: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-photo-organize",
        "POST",
        "/__action__",
        Some(json!({ "action": "photos.index", "payload": { "path": path } })),
    )
    .await
}

fn parse_photos(v: &Value) -> Vec<Photo> {
    v.get("photos")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(Photo {
                        id: p.get("id")?.as_str()?.to_owned(),
                        caption: p.get("caption")?.as_str()?.to_owned(),
                        // round-trip defensively; ignore unknown fields
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
```

Rules:

- Use `wylde_gui_pipe::call`. Don't reach across to another panel's
  `ipc.rs` to talk to its service — that's a layering violation
  (`wylde_check` rule `no_cross_panel_imports`).
- The `action` name must match what the target service registers in its
  `ALL_PIPE_ACTIONS`. For harness verbs see the harness pipe dispatcher
  (`rust/crates/wylde-harness/src/pipe/`). Harness verbs additionally
  short-circuit in-process via `wylde_harness::HarnessApi` — you don't
  need to know that as a caller; `call` handles the fast path.
- These functions are `async`. The View drives them with
  `cx.spawn(...)` (Step 4). The pipe crate bridges to a long-lived
  tokio runtime the Shell installs at startup, so wire IO works the
  same inside a `cx.spawn` task as inside a tokio one — you don't wire
  the runtime yourself.
- **Streaming verbs** (`chat.stream_turn`, `chat.stream_tools`) use the
  `wylde-gui-pipe::stream_call` path (a `ChunkFrame` loop with
  abort-on-drop), consumed in the View via a `cx.spawn` loop. The Chat
  panel (`Core/GUI/Frontend/Panels/Chat/`) is the worked example. If
  your panel streams, `wylde_check` rule `stream_call_must_handle_cancel`
  requires you to handle cancellation.

---

## Step 6 — register the panel with the registry

Two edits in the panel-registry crate
(`Core/GUI/Manifest/Extension_handlers/`, package
`wylde-panel-registry`):

**a) Add your crate as a dependency** in that crate's `Cargo.toml`
`[dependencies]` block, alongside the existing panels:

```toml
wylde-panel-photo-organize = { path = "../../Frontend/Panels/PhotoOrganize" }
```

(And add the same path to the workspace `members` list in
`Core/GUI/Cargo.toml` — see Step 7.)

**b) Wire the factory string to the real closure** in
`Core/GUI/Manifest/Extension_handlers/src/factories.rs`, inside
`default_first_party()`:

```rust
m.register(
    "wylde_panel_photo_organize::PhotoPanel::view",
    Box::new(wylde_panel_photo_organize::PhotoPanel::view),
);
```

The factory map is **hand-maintained on purpose**: the aggregator reads
JSON and can't introspect Rust exports, so this table is what lets the
compiler catch a typo in a factory name. The string here must match the
`source.factory` in your `manifest.json` exactly. If they drift, the
binary refuses to start with a `MissingFactory` error rather than
shipping a silent missing tab.

**c) Re-run the aggregator** to regenerate the static registry source:

```
cargo run -p wylde-panel-registry --bin wylde-panel-aggregator
```

The aggregator
(`src/bin/wylde_panel_aggregator.rs`) globs every panel `manifest.json`
under the discovery roots, validates each against schema v2, and emits
`Core/GUI/Manifest/Extension_handlers/src/generated.rs` — a
`register_all(registry, factories)` function with one block per
discovered panel. **`generated.rs` is generated; never hand-edit it.**
The output prints how many panels it wrote; your new one should be in
the count, and a `// ── core / photos … ──` block should appear in
`generated.rs`.

At startup the Shell builds a `FactoryMap` from `default_first_party()`,
calls `register_all` to populate the process-wide `PanelRegistry`
(installed via `PanelRegistry::install_global`), then unions in
extension panels via the runtime overlay (below). The left nav reads
the unified registry.

---

## Step 7 — make the crate a workspace member

Add your crate's path to the `members` list in `Core/GUI/Cargo.toml`:

```toml
[workspace]
members = [
    "Shell",
    "Frontend/Theme",
    # …
    "Frontend/Panels/Settings",
    "Frontend/Panels/PhotoOrganize",   # ← your panel
    # …
    "Manifest/Extension_handlers",
]
```

`wylde_check` rule `panel_crate_must_be_workspace_member` enforces this:
a panel crate that exists on disk but isn't a workspace member fails the
check. (Recall `Core/GUI/` is its *own* Cargo workspace, deliberately
not nested in the backend `rust/` workspace — see the comment at the top
of `Core/GUI/Cargo.toml`.)

---

## Step 8 — test it

Unit-test the panel's pure logic (IPC parsing, state transitions)
directly — that's plain Rust:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_photos_payload() {
        let v = serde_json::json!({ "photos": [{ "id": "1", "caption": "x" }] });
        let photos = parse_photos(&v);
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].caption, "x");
    }
}
```

The View itself is constructable in a headless gpui test context for a
render-without-crashing smoke test (the existing panel crates do this).
The registry path is covered by the aggregator's own tests
(`manifest.rs`, `registry.rs`, `factories.rs`, `overlay.rs`,
`list_tabs.rs` all have `#[cfg(test)]` suites).

To see it in the real Shell, run the GUI from `Core/GUI/`:

```
cargo run -p wylde-gui
```

The window opens; your panel appears in the left nav at `order: 75`.
Click it; the slot mints your View. If the slot shows a "Required
service not running" stub instead, every slug in `required_services`
must be alive — `service.health <slug>` against `wylde-lifecycle`
confirms.

> Building is out of scope for doc edits, but for reference: dev launch
> is `cargo run -p wylde-gui`; the release binary is
> `Core/GUI/target/release/wylde-gui.exe` (per `Core/GUI/manifest.json`
> `entry_point`).

---

## Iframe panel — the alternate recipe

When you have an existing webserver (N8N is the canonical example) and
don't want to rebuild its UI as a gpui View, declare an `iframe`
source. The Shell mounts a native `wry` WebView (the same engine Tauri
used) as a child of the gpui window, sized to the slot.

For a **first-party iframe panel**, `manifest.json`:

```json
{
  "schema_version": 2,
  "service": "n8n",
  "panels": [
    {
      "id": "editor",
      "title": "Workflows",
      "icon": "workflow",
      "order": 60,
      "version": "0.1.0",
      "required_services": ["n8n"],
      "source": {
        "kind": "iframe",
        "url": "http://127.0.0.1:5678",
        "sandbox": "allow-scripts allow-same-origin"
      }
    }
  ]
}
```

Rules:

- `url` must be loopback (`127.0.0.1`, `localhost`, `[::1]`). The
  manifest parser refuses anything else at build time
  (`loopback::is_loopback_url`), and the runtime overlay's
  `filter_extension_panels` drops non-loopback URLs again at load. There
  is no escape hatch.
- An iframe panel has no `factory`; its `RegistryRow` factory is `None`.
  The slot renders a transparent placeholder and the Shell mounts the
  WebView over it (see `Core/GUI/Shell/src/slot.rs` and the WebView host
  at `Core/GUI/Frontend/Extension_handlers/WebView/src/lib.rs`).
- `sandbox` is optional. The WebView host translates iframe sandbox
  tokens to the closest `wry` capabilities; tokens with no `wry`
  analogue are surfaced as unsupported rather than silently dropped.
- A URL health probe (async HTTP HEAD with a timeout) runs before the
  WebView is shown. While it's in flight the slot shows a "Checking
  <url>…" strip; on failure it shows the "Required service not running"
  stub. (The optional `health_check` field is reserved in the schema for
  per-panel probe tuning.)
- The WebView always renders on top of the gpui elements behind it
  (a known wry/gpui interop quirk). For a full-tab iframe this is
  invisible — nothing in the app overlaps the slot rect. Overlapping
  surfaces (the consent modal) are handled by the Shell separately.

**Extensions** don't ship a `Frontend/manifest.json`. Instead they
declare iframe panels in the `ui_panels` field of their
`mcp-server.json` (slice 12.7). The extension bridge surfaces them at
runtime via the `extensions.list_panels` pipe action; the Shell unions
them into the registry through the runtime overlay
(`overlay::union_for_runtime`). Extension panels are **iframe-only** —
the bridge can't ship gpui View factories over IPC — and carry an
`extension_id` for origin tracking. See
[extending-wylde-extensions.md](./extending-wylde-extensions.md) and
[extending-the-gui.md](./extending-the-gui.md) for that path; it's
already wired, so you write no Rust for it.

---

## Conventions

### Tab ordering

| Band | Use |
|---|---|
| 0–9 | Chat / primary workspace (Chat=5, Dashboard=8) |
| 10–29 | Core context (Memory=20) |
| 30–59 | Core tooling (Workspaces=30, Models=40, Tools=50) |
| 60–89 | First-party non-Core + device/media (Devices=60, RemoteAccess=65, Images=80, Training=85) |
| 90–99 | Settings (lives last, =95) |
| 100+ | Extensions (overlaid at runtime, sorted by extension then panel id) |

The numbers above are the live values in the shipped manifests. Pick the
lowest band that fits. Ties on `order` break on `service`/`id`
alphabetically (see `PanelRegistry::entries` in `registry.rs`).

### Icons

Lucide icon names. The Shell ships the Lucide set; specify by name in
the manifest (`"icon": "image"`, not `"icon": "image.svg"`). Live
examples: `chat`, `gauge`, `brain`, `folder`, `cpu`, `wrench`,
`smartphone`, `globe`, `image`, `graduation-cap`, `settings`.

### Theme tokens

Import `wylde_theme::colors::*` and `wylde_theme::typography` and use
the constants; don't hardcode hex. The Theme crate mirrors the old
`app.css` palette one-to-one (`SURFACE_900`, `BRAND`, `TEXT_PRIMARY`,
…), so a palette change touches one file. A panel that wants a different
look may declare its own colour constants instead of importing Theme,
but the typical case imports Theme so the visual identity stays
cohesive.

There is no per-element dark/light theming primitive analogous to
`:host([data-theme="light"])` — the alpha ships the dark palette. If
light-mode support lands it will be a Theme-crate concern, not a
per-panel one.

### Crate + factory naming

- Panel crate name: `wylde-panel-<name>` (kebab-case), e.g.
  `wylde-panel-photo-organize`.
- Factory string: `<crate_snake>::<Type>::view`, e.g.
  `wylde_panel_photo_organize::PhotoPanel::view`. Must contain `::`
  (parser-checked) and must match the `factories.rs` registration
  exactly.
- Service slug (in `manifest.json` `service` and in `required_services`
  / pipe names): `[a-z0-9-]+`. For Core panels `service` is `"core"`.

### Cross-platform paths

Forward slashes in manifests, repo-relative. The aggregator normalises
to forward slashes on the way in (it strips Windows prefixes), so what
you write should already be portable.

---

## Anti-patterns

**Don't import another panel's crate.**

```rust
// WRONG
use wylde_panel_memory::ipc::list_memories;
```

Memory's IPC is Memory's. If you need memory data, call the memory pipe
yourself from your own `ipc.rs`. (`wylde_check` rule
`no_cross_panel_imports`.)

**Don't import the deleted Svelte/Tauri surface.**

```rust
// WRONG — these don't exist post-cutover
use crate::tauri::invoke;          // no Tauri
// and there is no @tauri-apps/api, no svelte, no vite, no package.json
```

The GUI talks pipes directly from Rust via `wylde-gui-pipe`. There is no
`invoke`, no JS bridge, no dev server. (`wylde_check` rule
`no_legacy_gui_imports_in_panels`.)

**Don't declare a `custom_element` (or any non-v2) source.**

```json
{ "schema_version": 1, "source": { "kind": "custom_element", "tag": "x-panel" } }
```

The parser rejects both the `schema_version: 1` and the
`custom_element` variant. First-party panels are `gpui_view`; the only
other valid kind is `iframe`. (`wylde_check` rule
`first_party_manifest_must_be_gpui_view`.)

**Don't reach outside your View.**

```rust
// WRONG
window.set_title("Photos");
// WRONG — pushing to the global toast queue, holding the sidebar handle
```

A panel owns its own View tree and may call pipe verbs. The Shell owns
the window title, the toast queue, and the sidebar. If you need to
coordinate, call a pipe action. (The cross-panel nav bus,
`wylde-gui-pipe::nav_bus`, is the sanctioned way to request navigation
to another tab.)

**Don't bake non-loopback URLs into iframe panels.**

```json
{ "kind": "iframe", "url": "https://my-saas.example.com/" }
```

The parser refuses this and the runtime overlay refuses it again. There
is no override. If you need a remote UI, host a loopback proxy in your
own service that owns the credential and the egress policy.

**Don't put a WebView anywhere but the WebView handler crate.** Iframe
hosting lives in `Core/GUI/Frontend/Extension_handlers/WebView/`. A
panel crate must not take a direct `wry` dependency. (`wylde_check` rule
`webview_only_in_extension_handlers`.)

**Don't skip `required_services`.** If your panel needs `wylde-harness`
to render, list it. The slot shows a clear "Start service" stub when
it's down; without the declaration your panel mounts and then errors on
its first pipe call.

**Don't hand-edit `generated.rs`.** It's emitted by the aggregator and
overwritten on the next run. Add a manifest + a `factories.rs` entry and
re-run the aggregator instead.

**Don't fork the schema.** If you want a new manifest field, propose a
`schema_version` bump in the architecture plan first. The parser is
strict; unknown shapes fail loud.

---

## Reference (every id this doc cites)

**Crates (under `Core/GUI/`):**
- `wylde-gui` (`Shell/`) — the shipped binary; owns the window, tray,
  sidebar, panel slot, registry bootstrap.
- `wylde-theme` (`Frontend/Theme/`) — colour + typography tokens.
- `wylde-gui-pipe` (`Frontend/Pipe/`) — `call`, `stream_call`, the
  in-process `HarnessApi` short-circuit, the `nav_bus`.
- `wylde-panel-registry` (`Manifest/Extension_handlers/`) — manifest
  schema v2, factory map, build-time aggregator, runtime registry +
  overlay, `gui.list_tabs`. Ships the `wylde-panel-aggregator` binary.
- `wylde-webview` (`Frontend/Extension_handlers/WebView/`) — the
  `wry`-backed WebView host for iframe panels.

**Pipe actions the recipe touches:**
- `service.health` / `service.start` (against `wylde-lifecycle`) — the
  `required_services` health gate and the "Start service" stub button.
- `extensions.list_panels` (against the extension bridge) — the runtime
  overlay that surfaces extension iframe panels.

**GUI-internal verb:**
- `gui.list_tabs` — reads the unified panel registry (the Shell uses it;
  panels shouldn't need to). Implemented as a Rust call in
  `Manifest/Extension_handlers/src/list_tabs.rs`.

**Aggregator binary:**
- `wylde-panel-aggregator` — run via
  `cargo run -p wylde-panel-registry --bin wylde-panel-aggregator` to
  regenerate `generated.rs`.

---

## Cross-links

- [wylde-gui-panel-architecture-plan.md](./wylde-gui-panel-architecture-plan.md)
  — the manifest/aggregator/registry architecture rationale (its
  rendering-primitive sections predate the gpui swap).
- [wylde-gpui-rewrite-plan.md](./wylde-gpui-rewrite-plan.md) — the
  migration design that produced this client; canonical source of truth
  for the gpui-era architecture. (It is a *migration* doc, so its
  references to Tauri/Svelte describe what was replaced.)
- [extending-the-gui.md](./extending-the-gui.md) — the extension iframe
  panel story (`extensions.list_panels`); still valid in gpui.
- [extending-wylde-extensions.md](./extending-wylde-extensions.md) —
  iframe panels declared via `mcp-server.json::ui_panels`.
- [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) —
  registering the action your panel calls.

---

*Add a crate. Add a manifest. Implement `Render` and a `view` factory
(or point at a loopback URL). Wire the factory in `factories.rs`,
re-run the aggregator. The Shell finds it. That's the whole story.*
