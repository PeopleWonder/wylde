---
title: Extending Wylde — overview
audience: the Wylde user, future Claude sessions, anyone building on top of Wylde
authored: 2026-05-27
updated: 2026-06-17
status: living reference
---

# Extending Wylde

## Executive summary

Wylde is an AI assistant that lives on your computer. Under the hood it's
made of a bunch of small programs that talk to each other — one runs the
chat brain, one wraps the local LLM, one does voice, one stores memories,
and so on. "Extending Wylde" means teaching it to do something new. There
are five flavours of "something new," and the right one depends on how
deeply you need to plug into the system.

The simplest is **adding a tool the LLM can use** (e.g., "let the model
read a file" or "let the model fetch a weather forecast"). Above that
sits **adding a button or panel in the GUI**, then **adding an out-of-box
plugin** (an MCP extension — sandboxed, any programming language, untrusted
code OK), then **adding a new in-box service** (trusted, Rust, gets its
own state and lifecycle). Underneath all four is **the action registry**,
the single canonical list of "things Wylde can do" that every audience
(the model, the GUI, the plugin host) reads from. The whole system is
designed so that registering one action makes it reachable by all three
audiences without duplicating logic.

This doc is the map. It explains the vocabulary, points you at the right
deep-dive doc for what you want to build, and shows you the architecture
diagram so the per-pillar docs make sense in context. Skip ahead to the
"Where to start" section at the bottom if you already know what you want
to build.

## How it works

### TL;DR diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│                       Actions Registry                               │
│  one canonical catalog of "things Wylde can do"                      │
│  (handlers + metadata + tier flags + parameter schemas)              │
└──────────┬─────────────────┬──────────────────────┬──────────────────┘
           │                 │                      │
   ┌───────▼──────┐  ┌───────▼────────┐   ┌─────────▼─────────┐
   │ LLM          │  │ GUI            │   │ MCP-extension     │
   │ dispatcher   │  │ dispatcher     │   │ dispatcher        │
   │              │  │                │   │                   │
   │ tooling/     │  │ pipe/          │   │ wylde-extension-  │
   │ (tool calls) │  │ (gpui IPC)     │   │ bridge (jsonrpc)  │
   └──────────────┘  └────────────────┘   └───────────────────┘
        │                    │                      │
        ▼                    ▼                      ▼
   model output        GUI buttons /         out-of-box plugin
                       workflows             (any language)
```

The same `read_file` action can be invoked by the model (as `fs.read_file` in a
tool call), by the GUI (as `tools.run` with `{name: "fs.read_file"}` over the
harness pipe), or by an MCP extension (after the extension lands a `tools/call`
RPC on the bridge). One registry; three dispatchers.

### Three pillars

### 1. Actions

An **action** is "a thing the system can do" plus its metadata: id, name,
group, description, JSON-Schema-ish parameters, a `destructive: bool` tier
flag, and an `active` or `deferred` handler. Actions live in the harness's
in-process registry — see `rust/crates/wylde-harness/src/tooling/registry.rs`
for the data model and `tools/` for the active set.

Adding a new action means writing one `register_*` call. You get LLM tool
calls, GUI invocation, and (eventually) MCP exposure for free, because all
three dispatchers consume the same registry.

→ [docs/extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) — the
canonical "add a new action" recipe; the LLM dispatcher is the simplest one to
add against.

### 2. Services

A **service** is a Wylde-owned, lifecycle-managed Rust binary that owns a
named pipe and a manifest. Services are "in the box" — trusted, tight IPC
integration, indistinguishable from core after install. The lifecycle daemon
starts them, supervises them, and reaps them on shutdown.

Existing services: `wylde-vram-broker`, `wylde-gateway`, `wylde-device-gate`,
`wylde-lifecycle`, `wylde-ollama`, `wylde-voice`, `wylde-vpn`,
`wylde-extension-bridge`, `wylde-harness`. Memgraph, once a
standalone service, was folded into the harness as a submodule on 2026-05-25;
this is the pattern when a service's only consumer is one other service.

Services are pluggable and removable — the strangler-fig migration shipped each
of them by running the Rust port alongside its Python predecessor and flipping a
default once parity was proven. The same pattern works for net-new services.

→ [docs/extending-wylde-services.md](./extending-wylde-services.md) — adding a
new in-box service.

### 3. Extensions

An **extension** is an out-of-box MCP server (any language, any process) that
the `wylde-extension-bridge` discovers, spawns, supervises, and exposes through
a 12-action surface on `\\.\pipe\wylde-extension-bridge`
(`wylde-extension-bridge/src/service.rs:16`, `ALL_ACTIONS`). The bridge owns the
extension's **spawn, supervision, and shutdown** — but "sandboxed" here means
**lifecycle + advisory capabilities, not an OS sandbox**. Two old claims that
this overview used to make are wrong and now corrected in
[docs/extensions/](./extensions/):

- **Extensions *do* have IPC into other Wylde services.** A Rust extension links
  `wylde-shared` and can `ipc::call_action` over named pipes — e.g.
  `wylde-ext-webcrawler` forwards its web fetches through the Gateway's
  `egress.forward` rather than opening raw sockets
  (`wylde-ext-webcrawler/src/egress.rs:79`), and the same helper reaches the
  harness for memory/chat/tools/consent. The real guardrails are the **Gateway
  egress allowlist** and the **per-tool consent gate**, not transport isolation.
- **There is no filesystem sandbox.** A Rust extension runs with the process's
  full FS access; capability declarations (`egress.web`, `ingress.browser`, …)
  are **advisory** — they feed the gateway/consent gates, they are not
  transport-enforced.

Extensions speak JSON-RPC 2.0 over stdio (HTTP transport is reserved but not
implemented). For the authoritative contract see
[docs/extensions/adding-an-extension.md](./extensions/adding-an-extension.md)
and [harness-api-reference.md](./extensions/harness-api-reference.md).

Extensions are the right home for anything that leaves the enclosed system —
web browsing, third-party SaaS connectors, the N8N integration once it ports.

→ [docs/extending-wylde-extensions.md](./extending-wylde-extensions.md) —
adding a new MCP extension. Covers both the action surface (existing) and the
future UI-panel surface (designed, not built).

### Audiences and dispatchers

The three pillars are *what* gets registered; the three dispatchers are
*who's calling*. Every action carries metadata for each audience that should
see it:

| Metadata | LLM dispatcher | GUI dispatcher | MCP dispatcher |
| --- | --- | --- | --- |
| `id`, `name` | canonical id + dotted name; the alias map resolves both | id is what the GUI's `tools.run` envelope uses | tool name in the MCP `tools/list` payload |
| `description` | shown to the model in the tool catalog | shown as the GUI button tooltip / inspector text | shown to MCP clients (other models, dev consoles) |
| `parameters` | JSON-Schema-ish; the model reads it before emitting a call | rendered as inputs by the gpui Tools panel (`Core/GUI/Frontend/Panels/Tools/src/tools_panel.rs`) | flattened to `inputSchema` in MCP `tools/list` |
| `destructive: bool` | the registry tier gate checks against the turn's `device_tier` | the GUI hides destructive tools on read-only tiers | the extension bridge refuses destructive calls unless the extension declares the capability |
| `kind: Active \| Deferred` | deferred → `phase_<n>_deferred` error returned to the model | deferred → GUI shows "coming soon" state | deferred actions are not advertised to MCP clients |

Today the LLM dispatcher (`tooling/runner.rs`) is the most mature — it has the
tier gate, the alias map, and the deferred-stub mechanism. The GUI dispatcher
(`pipe/`) is thinner: it currently *just* re-projects the registry into the
pipe envelope shape (`tools.list`, `tools.run`). The MCP dispatcher
(`wylde-extension-bridge`) is *almost* symmetrical with the LLM dispatcher
but the data flows the other way — the bridge consumes external tools rather
than exposing internal ones.

### Audience-specific metadata: today and tomorrow

Today every action carries one description string used by all audiences.
That works because every active action is LLM-shaped. A future enhancement
will allow per-audience overrides:

```rust
description: "search the long-term memory",
gui_description: Some("Search saved notes"),
mcp_description: None,  // fall back to the LLM description
```

Worth adding when (a) the GUI wants user-friendly verbs that aren't the same as
the model-facing description, or (b) the MCP surface starts exposing internal
actions to outside clients (which it doesn't today).

### Where the dispatchers live

| Dispatcher | Lives at | Wire format | Audience |
| --- | --- | --- | --- |
| LLM | `rust/crates/wylde-harness/src/tooling/runner.rs` + `tools/` | in-process `Value` | the model, called from the turn loop |
| GUI (in-process) | `Core/GUI/Frontend/Pipe/src/` over `wylde_harness::HarnessApi` | in-process `Value` → `Reply` | the gpui shell + panels (unary verbs) |
| GUI (over-the-wire) | `rust/crates/wylde-harness/src/pipe.rs` (registers the same trait) | msgpack-over-named-pipe | non-GUI pipe clients (MCP probes, CLI, parity tests) + streaming verbs |
| MCP | `rust/crates/wylde-extension-bridge/src/` | JSON-RPC 2.0 over stdio | external MCP servers (Wylde is the client) |

**Shipped (Phase 12.1, 2026-05-27):** the harness exposes a `HarnessApi`
trait (`rust/crates/wylde-harness/src/api.rs`) that both the harness
binary's pipe (for external clients) and the gpui GUI (in-process,
via `Core/GUI/Frontend/Pipe/src/`) dispatch against. The GUI path no
longer takes the IPC hop for the 18 unary verbs. Streaming verbs
(`chat.stream_turn`, `chat.stream_tools`) and unknown verbs still
fall through to the wire path so the strangler-fig fallback keeps
working. See [docs/extending-the-gui.md](./extending-the-gui.md) for
the per-verb routing tables.

## How to extend

### Where to start

* **Adding a new tool the model can call** →
  [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md). This is the
  simplest extension surface and the recommended starting point.
* **Adding a new GUI workflow / panel** →
  [extending-the-gui.md](./extending-the-gui.md). Covers pipe verbs (today) and
  the future UI-panel API (so extensions can contribute panels).
* **Adding a new in-box service** →
  [extending-wylde-services.md](./extending-wylde-services.md). Use this when
  the new thing needs its own pipe, its own lifecycle, and its own state. The
  bar is high — the strangler-fig + manifest + lifecycle discipline is a lot of
  surface area for something that could be a tool.
* **Adding an out-of-box plugin** →
  [extending-wylde-extensions.md](./extending-wylde-extensions.md). MCP server,
  any language. This is the right home for anything that needs the open web,
  third-party APIs, or untrusted code.

## Gotchas

### Design principles for extenders

1. **One canonical registry.** If you're adding metadata about an action,
   add it once, in the registry entry. The three dispatchers project from
   there. Don't duplicate the action list in a GUI constants file.
2. **Audience-shaped wire envelopes, not audience-shaped logic.** The
   `tools.run` pipe verb returns `{ok, data, error}` because that's what the
   GUI expects. The internal `dispatch_tool` returns `DispatchOutcome` because
   that's what the turn loop expects. Both call into the same handler.
3. **Tier first, audience second.** A `destructive: true` action is destructive
   for everyone. The GUI hiding it on `read_only` is downstream of the runner
   blocking it on `read_only`. Don't gate at the dispatcher only.
4. **In-box trust, out-of-box sandbox.** Services run in our address space,
   share our pipes, are linted by `wylde_check`. Extensions get spawned in
   their own process and the LLM reaches their tools over JSON-RPC, but a Rust
   extension can still `ipc::call_action` other Wylde services (gateway egress,
   harness callbacks) — bounded by the gateway allowlist + consent gate, not by
   transport isolation. They declare their capabilities up front. When in doubt,
   build it as an extension first; promote to a service only if you need deeper
   IPC integration.
5. **Default deferred, ship active.** A registered-but-deferred action is fine
   — the model sees a clean `phase_N_deferred` error and chooses a different
   tool. A surprise `unknown_tool` error from the model's perspective is much
   worse. Cataloguing forward is cheap.

## Cross-links

* `docs/wylde-repo-organization.md` — the canonical map of where everything
  lives. Read this first if you're new.
* `docs/manifest_ownership.md` — manifest write/heartbeat conventions enforced
  by `wylde_check`.
* `docs/mcp_surface.md` — the broader MCP integration story.
* `docs/wylde-rust-migration-master-plan.md` — phase numbers (5, 6, 7, 9, 11)
  trace back here.

---

*This is a navigation doc — the per-pillar docs are the source of truth. When
those drift from the code, update them, then update this overview.*
