# `wylde-images` — the first out-of-tree Service (worked example)

Status: extracted 2026-06-17. This is the durable in-repo record of how the
Images suite was carved out of Core into a standalone **Service** repo,
riding the [out-of-tree runtime foundation](../out-of-tree-runtime.md). It
doubles as the worked example for the **Services tier**: a removable
sibling that **Core builds and boots with or without**.

> Where the code lives: `Services/wylde-images/` is **git-ignored** by Core
> (it ships empty) and is its **own** git repo + Cargo workspace. The
> iframe stub `Extensions/wylde-images/mcp-server.json` likewise lives in
> the git-ignored `Extensions/` bucket. Neither is tracked in Core.

## What moved, and what stayed

The Images backend was never a crate — it was the gateway-embedded route
module `rust/crates/wylde-gateway/src/routes/images.rs` (a ComfyUI proxy +
a `data/images/` library reader). The native gpui panel was
`Core/GUI/Frontend/Panels/Images/`. Both are gone from Core; the capability
now lives in `Services/wylde-images/`:

- **ComfyUI stays external/user-managed** — same posture as n8n / Ollama.
  Only the *proxy + library I/O* moved. ComfyUI is not packaged.
- **License: GPL-3.0-or-later.** The service is carved out of, and links,
  the GPLv3 Core (`wylde-shared`), so it inherits the copyleft and ships a
  verbatim `LICENSE`.

## The two surfaces

```
  GUI iframe panel  ── http ──▶  wylde-images loopback gallery (127.0.0.1:8015)
        (Extensions/wylde-images stub, transport:"none")        │
                                                                 ├─ comfy.rs   ── http ──▶ ComfyUI :8014 (external)
  Core / other consumers ── action verbs ──▶ \\.\pipe\wylde-images
                                                                 └─ library.rs ── fs ──▶ WYLDE_IMAGES_DATA_DIR
```

- **Action-verb pipe** (`\\.\pipe\wylde-images`) — the native service
  idiom, the contract for Core/other consumers:
  `images.generate`, `images.library.{list,get,delete}`,
  `images.models.list`, `images.loras.list`. Declared at
  `Services/wylde-images/contracts/actions/wylde-images.json`; the live
  contract is re-written to `data/contracts/actions/wylde-images.json` at
  boot.
- **Loopback gallery UI** (`http://127.0.0.1:8015`, loopback-bound = the
  auth boundary) — the service serves its own gallery; the GUI surfaces it
  as an **iframe** via the N8N-style stub
  `Extensions/wylde-images/mcp-server.json` (`transport:"none"`,
  `ui_panels` iframe). **No gpui panel is compiled into Core** — that is
  what makes it truly removable.

Both surfaces call one set of logic (`comfy` + `library`), so they cannot
drift.

## How it rides the foundation (zero new Core code)

- **Discovery + supervision** — `Services/wylde-images/manifest.json`
  (`tier:"standard"`, `enabled:true`, `pipe:"wylde-images"`) is found by
  the registry's `Services/*` walk; the daemon spawns/heartbeats/stops it
  via `start_discovered` / `stop_discovered`. No accept-list edit.
- **Binary** — `cargo xtask build-all` builds the sibling and stages
  `Services/wylde-images/wylde-images.exe` beside the manifest, the exact
  path `sibling_binary_path` reads.
- **Data dir** — `library_dir()` is env-driven: the daemon injects
  `WYLDE_IMAGES_DATA_DIR` (default `<root-parent>/WyldeData/images/`,
  first-open picker override) at spawn; the service reads it, falling back
  to the old `$WYLDE_ROOT/data/images` when run without a daemon.

## The removability contract (acceptance test)

Move `Services/wylde-images` (and the `Extensions/wylde-images` stub)
aside, and Core still builds, boots, and the sidebar simply lacks Images —
no `MissingFactory`, no dangling pipe:

- the gateway no longer declares an `/api/images` route or the `base64` dep
  (its only consumer);
- no `wylde_panel_images` factory, workspace member, or `generated.rs` row
  remains, so the panel registry assembles cleanly without it;
- the registry walk returns no `wylde-images` sibling when the bucket is
  absent, so the boot/shutdown loops iterate zero times for it.

Verified at extraction time: rust/ + Core/GUI/ build green; gateway and
`wylde-panel-registry` test suites pass; the sibling's own 21 tests pass;
and the bucket-moved-aside build + discovery no-op both hold.

## Installing / removing it (operator)

- **Install**: drop the `wylde-images` repo into `Services/`, drop the
  iframe stub into `Extensions/wylde-images/`, run `cargo xtask build-all`,
  start the daemon. The Images tab appears once the service's loopback port
  is serving.
- **Remove**: delete (or move) the two folders. Nothing in Core references
  them; the tab disappears on next boot.
