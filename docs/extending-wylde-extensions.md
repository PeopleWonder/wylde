---
title: Extending Wylde — out-of-box MCP extensions (supplement)
audience: contributors and third parties building extensions for Wylde
authored: 2026-05-27
updated: 2026-06-03
status: supplement to docs/extensions/*; design-era quickstart redirected post-bridge
---

# Building a Wylde extension

> **Read the `docs/extensions/` set first — it is the source of truth.**
> This file predates the Rust extension path, the harness callback bridge,
> and the per-tool consent gate. Its quickstart, manifest table, and
> capability vocabulary have been **superseded** and now redirect to:
>
> * [extensions/writing-an-extension.md](./extensions/writing-an-extension.md) — the canonical quickstart (Rust-first, the 5-method MCP contract, scaffold, enable/test/ship).
> * [extensions/manifest-reference.md](./extensions/manifest-reference.md) — every `mcp-server.json` field, including `resources[]`.
> * [extensions/harness-api-reference.md](./extensions/harness-api-reference.md) — how an extension **calls back** into Wylde (gateway egress, memory, chat, tools, consent).
>
> What still lives **here** (not yet covered in the new set): the UI-panel
> deep dive, the full bridge action surface, the extensions-vs-services
> comparison, and harness routing. Everything else below points you at the
> docs above.

## Executive summary

A Wylde "extension" is a plugin: a separate program that runs in its own
process, that Wylde discovers, starts, watches, and routes requests to.
Extensions are the right home for anything that needs to do something
Wylde's trusted core shouldn't do directly — wrap a third-party service,
run a tool we don't fully trust, or stage a capability behind the
gateway's egress allowlist and the consent gate. The Webcrawler
extension fetches URLs. The Wylde_Study extension drives a browser. N8N
(a workflow engine) ships as a panel-only extension. Wylde-Passwords
will be an extension.

Extensions speak **MCP (Model Context Protocol)** over stdio, so any
program that can read JSON-RPC from stdin and write it to stdout can be a
Wylde extension. Per the repo's everything-Rust rule, **new extensions
are Rust binaries** (`rust/crates/wylde-ext-webcrawler/` is the canonical
example); Python is supported only through the legacy shim
(`Extensions/_shim/server.py`) for code not yet ported. The full
quickstart — scaffold, the five MCP methods the bridge drives, the
minimal manifest, and the enable/test loop — now lives in
[extensions/writing-an-extension.md](./extensions/writing-an-extension.md).
The minimal-server walkthrough and the per-field manifest table that used
to live in this section have moved there and to
[extensions/manifest-reference.md](./extensions/manifest-reference.md);
they are no longer maintained here.

### Correcting the design-era sandbox claim

Earlier revisions of this doc said extensions *"only call out, never
in"* — that an extension reaches Wylde solely through the bridge's
`tools/call`. **That is no longer true.** A Rust extension links
`wylde-shared` and can open a named pipe to any Wylde service:
`wylde-ext-webcrawler` makes its web requests by forwarding through the
**gateway** (`egress.forward`), and the same `ipc::call_action` helper
reaches the **harness** for memory, chat, tool dispatch, and consent. The
real guardrails are the **gateway egress allowlist** and the **per-tool
consent gate**, not transport isolation. The verb list and the actual
sandbox/consent model are documented in
[extensions/harness-api-reference.md](./extensions/harness-api-reference.md).

## How it works

Two existing extensions to study: `rust/crates/wylde-ext-webcrawler/`
(canonical Rust) and `Extensions/Wylde_Study/` (canonical Python, via the
legacy shim). The protocol is language-neutral, but new work is Rust.

The contract the bridge drives over your stdin/stdout — `initialize`,
`notifications/initialized`, `tools/list`, `tools/call`, `ping` — plus
the protocol version it pins (`MCP_SPEC_VERSION = "2025-11-25"`, with the
previous `"2025-06-18"` still accepted) is documented in
[extensions/writing-an-extension.md §1](./extensions/writing-an-extension.md#1-the-contract-what-the-bridge-does-to-you).
The manifest fields, placeholder tokens (`${WYLDE_ROOT}`, `${WYLDE_BIN}`,
`${WYLDE_PYTHON}`), the declared-tool catalog, capabilities vocabulary,
and the `resources[]` block are all in
[extensions/manifest-reference.md](./extensions/manifest-reference.md).

### The bridge's action surface

The bridge registers **twelve actions** on
`\\.\pipe\wylde-extension-bridge` (verified against
`rust/crates/wylde-extension-bridge/src/service.rs`):

| Action | Purpose |
| --- | --- |
| `ext.list` | List all discovered extensions + their status. |
| `ext.get` | One extension's status by name. |
| `ext.enable` | Enable an extension; spawns the server. Persists by rewriting `enabled: true` in the manifest. |
| `ext.disable` | Stop and disable an extension. |
| `ext.tools.list` | Aggregated tool catalog (default) or per-extension via `{extension}`. |
| `ext.tools.call` | Invoke a tool: `{extension, tool, arguments?}`. |
| `ext.resources.list` | `[{extension?}]` — the declared `resources[]` blocks for the harness verb overlay (Slice 5a). Pure read; answers for *disabled* extensions; never spawns. |
| `ext.health` | `{extension}` — send MCP `ping` to the extension's server. |
| `ext.restart` | `{extension}` — stop + start one extension's MCP server. |
| `extensions.list_panels` | Union of every enabled extension's `ui_panels`. Pure read; never spawns. Consumed by the GUI's Tools tab. |
| `ext.events` | Streaming lifecycle events (`spawn`, `exit`, `restart`, `crash`, `enabled`, `disabled`). |

Plus a back-compat alias: `extensions.dispatch` (`{extension, endpoint,
params}`) which forwards to `ext.tools.call` with `tool = endpoint`. Kept
until the Gateway switches to `ext.tools.call`, then removed.

> Note: `ext.resources.list` here is the **resource-declaration**
> listing added by tool-registry consolidation Slice 5a — *not* the
> MCP-spec `resources/list` surface, which remains unimplemented. An
> earlier draft of this table listed a deferred `ext.resources.read`;
> no such verb exists.

### Routing from the harness

The harness's tool dispatcher (`wylde-harness/src/dispatch.rs`) routes
tool names to either the internal registry or the MCP bridge by checking
whether the name's first dotted segment matches a configured extension
namespace. The namespaces are read from `cfg.mcp_namespaces`, which
defaults to `["webcrawler", "wylde_study"]` (`wylde-harness/src/config.rs`).
Add yours by either:

* Setting `WYLDE_HARNESS_MCP_NAMESPACES=webcrawler,wylde_study,hello`
  in the harness env (the production config reads this CSV directly), or
* Editing the default in `Config::default` / `Config::default_for_tests`.

The bridge's `ext.tools.list` returns the right metadata regardless; the
namespace list is just the routing heuristic. (Once the verb-tool cutover
flag `WYLDE_HARNESS_VERB_TOOLS` is on, an extension's `resources[]`-claimed
tools are reached through the harness verb layer instead — see
[extensions/manifest-reference.md](./extensions/manifest-reference.md#resources-resourcedeclaration--slice-5a).)

## UI panels

Extensions can contribute UI panels that the GUI hosts as first-class
tabs alongside Dashboard, Workflows, Devices, etc. A panel points at a
loopback URL the extension owns — useful when the underlying capability
already has a perfectly good web UI (N8N's workflow editor,
Wylde-Passwords' settings, etc.) and we don't want to reinvent it inside
the GUI.

Declare panels under `ui_panels` in `mcp-server.json`:

```json
{
  "name": "n8n",
  "transport": "stdio",
  "command": ["${WYLDE_PYTHON}", "-c", "import sys; sys.exit(0)"],
  "capabilities": [],
  "ui_panels": [
    {
      "id": "workflows",
      "title": "Workflows",
      "icon": "🔗",
      "source": { "kind": "iframe", "url": "http://127.0.0.1:5678" }
    }
  ]
}
```

The runtime sees an extension's panels via the read-only
`extensions.list_panels` action; the gpui GUI's "Tools" tab
(`Core/GUI/Frontend/Panels/Tools/`) renders the union of every
extension's declarations, lets the user switch between them, and probes
each URL before mounting so an offline service shows a placeholder
instead of a blank panel. Iframe panels are hosted natively by the
wry-based WebView crate
(`Core/GUI/Frontend/Extension_handlers/WebView/`), which mounts a child
WebView over the panel slot rather than embedding an HTML `<iframe>`
inside a web app — there is no browser-level CSP `frame-src` to
maintain. The loopback restriction below is enforced by the bridge at
manifest load instead.

`ui_panels` is also harvested from the legacy `manifest.json` if
`mcp-server.json` doesn't declare any, so extensions that haven't been
migrated to the new manifest can still surface a panel.

### Source kinds

| `kind` | Status | Notes |
| --- | --- | --- |
| `iframe` | shipped | Hosts the URL in a native wry WebView mounted over the panel slot. URL must be loopback. |
| `native_view` | deferred | In-process panel — a first-party gpui View compiled into the GUI. Better UX, more trust. Not implemented. |

The enum is tagged so adding a new kind later doesn't break existing
`iframe` manifests.

### Security: loopback only

Panel source URLs **must** point at the local loopback interface
(`127.0.0.1`, `localhost`, or `::1`) — anything else is rejected at
manifest load with a clear validation error
(`manifest.rs::validate_ui_panels`), before the GUI ever sees it. The
reason: the GUI renders the URL inside a WebView sharing the user's
session context with the rest of the app. A panel pointing at a remote
origin could exfiltrate session data or phish for credentials. The bridge
enforces this once on the way in so individual extensions can't smuggle a
remote URL into the GUI by clever spelling (`http://127.0.0.1.evil.com/`
etc.).

If you need a panel that talks to a remote service, host a thin local
proxy in your extension and point the panel at it. The proxy can apply
whatever auth / scoping you need before forwarding traffic upstream.

### A panel-only extension

If your extension's primary purpose is to surface an external service's
existing web UI (the N8N pattern), declare `enabled: false`, a no-op
stub command, and the panel — the bridge surfaces the panel without ever
spawning the stub. Example: `Extensions/N8N/mcp-server.json`. When the
extension grows real MCP tools later, swap the stub for the actual server
command and flip `enabled: true`; the panel declaration stays the same.

## Gotchas

### How extensions and services differ

| | Extension | Service |
| --- | --- | --- |
| Trust | sandboxed by capability + consent gating | full Wylde trust |
| Process | own process, MCP spawn | own process, lifecycle daemon spawn |
| Language | any (new work: Rust) | Rust |
| Transport | JSON-RPC over stdio | msgpack over named pipe |
| Discovery | bridge scans `Extensions/` | hard-coded slot in `Core/Lifecycle/daemon_state/_services_*.py` |
| Lifecycle | bridge owns spawn/supervise; user toggles via `ext.enable`/`disable` | daemon spawns at boot; on by default |
| State | local to the extension; bridge doesn't see it | first-class Wylde state in `data/` |
| Crash blast | one extension dies → bridge marks it failed; others unaffected | service crash can take harness with it (depending on slot) |
| Reaches into Wylde | spawn/lifecycle owned by the bridge, but a Rust extension can `ipc::call_action` other services — gated by the gateway allowlist + consent, not transport isolation (see [harness-api-reference.md](./extensions/harness-api-reference.md)) | full IPC + filesystem |

Pick extension if your code shouldn't be trusted with the harness's
keys-to-the-kingdom. Pick service if you're brokering a resource that the
trusted core needs.

### When you'd contribute an extension upstream

The bar for upstreaming into `Extensions/` is much lower than for adding a
service. If your extension:

* Has stable `mcp-server.json` validation passing
* Doesn't crash the bridge on disable
* Declares its capabilities accurately
* Ships dispatch tests next to its `mcp.rs` (the `#[tokio::test]` block in
  `wylde-ext-webcrawler/src/mcp.rs` is the bar)

…it's mergeable. The trust model is "the bridge sandboxes its spawn, the
gateway and consent gate bound what it can reach, and the user opted in by
enabling it." If the extension turns out to be malicious, the worst it can
do is what its declared capabilities permit and the user approves, and the
user can disable it immediately.

The Wylde-Passwords extension currently in
[docs/wylde-passwords-self-healing-extension.md](./wylde-passwords-self-healing-extension.md)
is the most ambitious in-flight extension; read that doc for the pattern
of a "real" extension as opposed to the hello-world in
[extensions/writing-an-extension.md](./extensions/writing-an-extension.md).

### Gates

```powershell
# Validate the manifest parses + the bridge starts
cargo test -p wylde-extension-bridge manifest

# Spin up the bridge and confirm your extension appears
.venv\Scripts\python.exe -m Core.shared.ipc_client wylde-extension-bridge ext.list

# Trigger a tool call
.venv\Scripts\python.exe -m Core.shared.ipc_client wylde-extension-bridge `
    ext.tools.call '{"extension": "Webcrawler", "tool": "fetch", "arguments": {"url": "https://example.com"}}'
```

## Cross-links

* [extensions/writing-an-extension.md](./extensions/writing-an-extension.md) — canonical quickstart (Rust-first).
* [extensions/manifest-reference.md](./extensions/manifest-reference.md) — every `mcp-server.json` field, incl. `resources[]`.
* [extensions/harness-api-reference.md](./extensions/harness-api-reference.md) — calling back into Wylde.
* [extending-wylde.md](./extending-wylde.md) — overview, audience model.
* [extending-the-gui.md](./extending-the-gui.md) — UI panel design.
* [extending-wylde-services.md](./extending-wylde-services.md) — when in-box is the right call instead.
* `rust/crates/wylde-extension-bridge/src/` — bridge source.
* `docs/MIGRATING_EXTENSIONS.md` — historical migration from the pre-MCP extension model.
* `docs/mcp_surface.md` — cross-cutting MCP integration story.
* [Model Context Protocol spec](https://modelcontextprotocol.io) — upstream.

---

*Extensions are the easiest extension surface to ship and the hardest to
trust. Start small, declare capabilities honestly, test in disabled state
first, and let the user opt in.*
