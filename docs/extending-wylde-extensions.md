---
title: Extending Wylde — out-of-box MCP extensions
audience: contributors and third parties building extensions for Wylde
authored: 2026-05-27
updated: 2026-05-28
status: living reference; UI-panel API shipped in Phase 12.7
---

# Building a Wylde extension

## Executive summary

A Wylde "extension" is a plugin. It's a separate program that runs in
its own process, that Wylde discovers, starts, watches, and routes
requests to. Extensions are the right home for anything that needs to
do something Wylde itself isn't allowed to do — make arbitrary web
requests, talk to third-party services, run code in a different
programming language, or wrap a tool that we don't trust enough to bake
into the core. The Webcrawler extension fetches URLs. The Wylde_Study
extension drives a browser. N8N (a workflow engine) will eventually be
an extension. The Wylde-Passwords plugin will be an extension.

Extensions talk to Wylde using a standard protocol called MCP (Model
Context Protocol — the same one Anthropic's Claude uses for its
plugins), so any program that can speak MCP can be a Wylde extension.
That makes the extension surface the most accessible way for outside
contributors to add capabilities — you don't need to know Rust, you
don't need to understand Wylde's internals, you just need to follow
the MCP spec and drop a `mcp-server.json` manifest in the
`Extensions/` directory.

This doc shows you what the manifest looks like, gives you a 30-line
MCP server you can copy as your starting point, explains the
capability system that gates what your extension is allowed to do
(can it make web requests? can it ship a browser extension alongside?),
and describes the future UI-panel API that will let extensions
contribute their own GUI panels rather than living in a popup window.
Read [extending-wylde.md](./extending-wylde.md) first for the audience
model; this doc is the deep dive on the out-of-box pillar.

## How it works

Two existing extensions to study: `Extensions/Webcrawler/` and
`Extensions/Wylde_Study/`. Both are Python today (via a thin shim that
translates legacy `handler.py` calls into MCP) but the protocol is
language-neutral — Node, Go, Rust, anything that can speak JSON-RPC over
stdio works.

### What you'll build

An extension is just a directory containing:

* `mcp-server.json` — manifest. How to spawn the server, what transport,
  declared tools, capabilities.
* `manifest.json` — legacy companion. Browser-extension path,
  capability declarations. Read alongside the MCP manifest for backward
  compatibility with the pre-MCP extension bridge.
* The MCP server binary or script.
* Optional: tests, a `browser_extension/` subdir for MV3 chrome
  extensions, a `tools/` subdir for legacy Python tool wrappers.

That's it. Drop it under `Extensions/<YourExtension>/` and the bridge
discovers it on the next restart.

### The MCP surface Wylde consumes

Wylde is the **MCP client**. Your extension is the **MCP server**. The
host (`wylde-extension-bridge`) calls:

* `initialize` — handshake. Server declares its `protocolVersion` and
  capabilities; host advertises that it consumes `tools` and `resources`
  but does NOT offer sampling, roots, elicitation, or logging back to
  the server.
* `tools/list` — discover available tools. Server returns
  `[{ name, description, inputSchema }, ...]`.
* `tools/call` — invoke one. Server returns `{ content: [...] }` per spec
  or a structured error.
* `ping` — periodic liveness check (configurable interval, default 30 s).

That's the bare minimum. Notifications, resources, and prompts are
roadmapped but not consumed by the bridge today.

## How to extend

### Minimal MCP server (pseudo-code)

This is the smallest possible extension. Any language. JSON-RPC over
stdin/stdout. Here it is in psuedo-Python (any equivalent works):

```python
# wylde-mcp.py — a 30-line MCP server
import sys, json

def respond(id, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def handle(req):
    method, id_, params = req["method"], req.get("id"), req.get("params", {})

    if method == "initialize":
        return respond(id_, {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "hello-ext", "version": "0.1"},
        })

    if method == "ping":
        return respond(id_, {})

    if method == "tools/list":
        return respond(id_, {"tools": [
            {
                "name": "hello.echo",
                "description": "Echo the input back",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                },
            }
        ]})

    if method == "tools/call":
        if params.get("name") == "hello.echo":
            text = params.get("arguments", {}).get("text", "")
            return respond(id_, {"content": [{"type": "text", "text": text}]})
        return respond(id_, error={"code": -32601, "message": "unknown tool"})

    return respond(id_, error={"code": -32601, "message": "method not found"})

for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        handle(json.loads(line))
    except Exception as e:
        sys.stderr.write(f"error: {e}\n")
```

Pair with `Extensions/Hello/mcp-server.json`:

```json
{
  "name": "hello",
  "description": "Minimal MCP extension example.",
  "version": "0.1",
  "enabled": true,
  "transport": "stdio",
  "command": ["python", "wylde-mcp.py"],
  "cwd": "${WYLDE_ROOT}/Extensions/Hello",
  "env": {},
  "capabilities": [],
  "health": {
    "method": "ping",
    "interval_s": 30,
    "timeout_s": 5
  }
}
```

Restart `wylde-extension-bridge`. The bridge discovers the manifest,
spawns the server, performs the handshake, and `ext.list` returns
your extension. The harness model can then call
`hello.echo` and the bridge routes it via `tools/call`.

### Manifest fields explained

See `rust/crates/wylde-extension-bridge/src/manifest.rs` for the
authoritative parser.

| Field | Required? | Notes |
| --- | --- | --- |
| `name` | yes | Used as the dispatcher namespace prefix. Matching is exact and case-sensitive. |
| `description` | optional | Shown in `ext.list`. |
| `version` | optional | Free-form string; default `"1.0"`. |
| `enabled` | optional | Defaults `false`. `ext.enable` / `ext.disable` persist by rewriting this field. |
| `transport` | yes | `"stdio"` (the only supported transport today). `"http"` is reserved but validation refuses it. |
| `command` | yes for stdio | Argv. Variable expansion: `${WYLDE_PYTHON}`, `${WYLDE_ROOT}` resolve at spawn time. |
| `cwd` | optional | Working directory for the child. Relative paths resolve against the extension's root. Defaults to the extension directory. |
| `env` | optional | Extra env vars. Merged onto the host's env. |
| `capabilities` | optional | Declarative capability list (see below). Falls back to legacy `manifest.json::capabilities` if absent. |
| `tools` | optional | Declarative tool catalog. Lets the host return `ext.tools.list` for a *disabled* extension without spawning the server — handy for the GUI to show what an extension would expose. |
| `ui_panels` | optional | Array of UI panels the extension surfaces in the GUI's Tools tab. See the [UI panels](#ui-panels) section. URLs are loopback-only — anything pointing outside `127.0.0.1` / `localhost` / `::1` is rejected at manifest load. |
| `health` | optional | `{ method, interval_s, timeout_s }`. Default: ping every 30 s with a 5 s timeout. |

### Capabilities

The capability list declares what the extension is *allowed* to do. The
bridge doesn't enforce these directly — they're consumed by the gateway
egress allowlist and by future tier gates. The current vocabulary:

* `egress.web` — the extension may make outbound HTTP requests. Webcrawler
  declares this.
* `ingress.browser` — a browser extension is shipped alongside; the
  GUI knows to expose the `browser_extension_path` for the user
  to load. Wylde_Study declares this.
* (future) `egress.<domain>` — fine-grained allowlist. Not implemented.
* (future) `tools.destructive` — extension may invoke destructive
  registry actions through whatever back-channel exists. Not implemented;
  extensions today only call out, never in.

### Variable expansion

The `command`, `env`, and `cwd` fields expand `${WYLDE_PYTHON}` and
`${WYLDE_ROOT}` at spawn time. `WYLDE_PYTHON` resolves to the active
venv's interpreter (`.venv\Scripts\python.exe`); `WYLDE_ROOT` resolves to
the repo root. Add other variables in
`wylde-extension-bridge/src/host.rs::expand_vars` if your extension needs
them — keep the set small.

### The bridge's action surface

The bridge exposes ten first-class verbs on `\\.\pipe\wylde-extension-bridge`:

| Action | Purpose |
| --- | --- |
| `ext.list` | List all discovered extensions + their status. |
| `ext.get` | One extension's status by name. |
| `ext.enable` | Enable an extension; spawns the server. Persists by rewriting `enabled: true` in the manifest. |
| `ext.disable` | Stop and disable an extension. |
| `ext.tools.list` | Aggregated tool catalog (default) or per-extension via `{extension: name}`. |
| `ext.tools.call` | Invoke a tool: `{extension, tool, arguments}`. |
| `ext.resources.list` | Per-spec; deferred. |
| `ext.resources.read` | Per-spec; deferred. |
| `extensions.list_panels` | Union of every extension's `ui_panels` declarations — what the GUI's Tools tab renders. Pure read; never spawns. |
| `ext.events` | Streaming lifecycle events (`spawned`, `initialized`, `failed`, `terminated`). |

Plus a back-compat alias: `extensions.dispatch` (Phase 4 shape) which
re-projects to `ext.tools.call`. Removed in a future cleanup slice once
nothing still calls the old name.

### Routing from the harness

The harness's tool dispatcher (`wylde-harness/src/dispatch.rs`) routes
tool names to either the internal registry or the MCP bridge by checking
whether the name's first dotted segment matches a configured extension
namespace. The namespaces are read from `cfg.mcp_namespaces`, which
defaults to `["webcrawler", "wylde_study"]`. Add yours by either:

* Setting `WYLDE_HARNESS_MCP_NAMESPACES=webcrawler,wylde_study,hello`
  in the harness env, or
* Editing the default in `Config::default_for_tests` and the production
  config loader (when one lands; right now it reads env directly).

The bridge's `ext.tools.list` returns the right metadata regardless; the
namespace list is just the routing heuristic.

## UI panels

Extensions can contribute UI panels that the GUI hosts as first-class
tabs alongside Dashboard, Workflows, Devices, etc. A panel is just an
`iframe` pointing at a URL the extension owns — useful when the
underlying capability already has a perfectly good web UI (N8N's
workflow editor, Wylde-Passwords' settings, etc.) and we don't want to
reinvent it inside the GUI.

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
instead of a blank iframe. Iframe panels are hosted natively by the
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
manifest load with a clear validation error, before the GUI ever sees
it. The reason: the GUI renders the URL inside a WebView
sharing the user's session context with the rest of the app. A panel
pointing at a remote origin could exfiltrate session data or phish for
credentials. The bridge enforces this once on the way in so individual
extensions can't smuggle a remote URL into the GUI by clever spelling
(`http://127.0.0.1.evil.com/` etc.).

If you need a panel that talks to a remote service, host a thin local
proxy in your extension and point the panel at it. The proxy can apply
whatever auth / scoping you need before forwarding traffic upstream.

### A panel-only extension

If your extension's primary purpose is to surface an external service's
existing web UI (the N8N pattern), declare `enabled: false`, a no-op
stub command, and the panel — the bridge will surface the panel without
ever spawning the stub. Example: `Extensions/N8N/mcp-server.json`. When
the extension grows real MCP tools later, swap the stub for the actual
server command and flip `enabled: true`; the panel declaration stays
the same.

## Gotchas

### How extensions and services differ

| | Extension | Service |
| --- | --- | --- |
| Trust | sandboxed, capability-gated | full Wylde trust |
| Process | own process, MCP spawn | own process, lifecycle daemon spawn |
| Language | any | Rust |
| Transport | JSON-RPC over stdio | msgpack over named pipe |
| Discovery | bridge scans `Extensions/` | hard-coded slot in `Core/Lifecycle/daemon_state/_services_*.py` |
| Lifecycle | bridge owns spawn/supervise; user toggles via `ext.enable`/`disable` | daemon spawns at boot; on by default |
| State | local to the extension; bridge doesn't see it | first-class Wylde state in `data/` |
| Crash blast | one extension dies → bridge marks it failed; others unaffected | service crash can take harness with it (depending on slot) |
| Reaches into Wylde | only through the bridge's action surface (today: tools/call) | full IPC + filesystem |

Pick extension if your code shouldn't be trusted with the harness's
keys-to-the-kingdom. Pick service if you're brokering a resource that the
trusted core needs.

### When you'd contribute an extension upstream

The bar for upstreaming into `Extensions/` is much lower than for adding a
service. If your extension:

* Has stable `mcp-server.json` validation passing
* Doesn't crash the bridge on disable
* Declares its capabilities accurately
* Includes at least one test (under `tests/`)

…it's mergeable. The trust model is "the bridge sandboxes it, the user
opted in by enabling it." If the extension turns out to be malicious, the
worst it can do is what its declared capabilities permit, and the user can
disable it immediately.

The Wylde-Passwords extension currently in
[docs/wylde-passwords-self-healing-extension.md](./wylde-passwords-self-healing-extension.md)
is the most ambitious in-flight extension; read that doc for the pattern
of a "real" extension as opposed to the hello-world above.

### Gates

```
# Validate the manifest parses + the bridge starts
cargo test -p wylde-extension-bridge manifest

# Spin up the bridge and confirm your extension appears
.venv\Scripts\python.exe -m Core.shared.ipc_client wylde-extension-bridge ext.list

# Trigger a tool call
.venv\Scripts\python.exe -m Core.shared.ipc_client wylde-extension-bridge \
    ext.tools.call '{"extension": "hello", "tool": "hello.echo", "arguments": {"text": "hi"}}'
```

## Cross-links

* [extending-wylde.md](./extending-wylde.md) — overview, audience model.
* [extending-the-gui.md](./extending-the-gui.md) — UI panel design.
* [extending-wylde-services.md](./extending-wylde-services.md) — when
  in-box is the right call instead.
* `rust/crates/wylde-extension-bridge/src/` — bridge source.
* `docs/MIGRATING_EXTENSIONS.md` — historical migration from the
  pre-MCP extension model.
* `docs/mcp_surface.md` — cross-cutting MCP integration story.
* [Model Context Protocol spec](https://modelcontextprotocol.io) — upstream.

---

*Extensions are the easiest extension surface to ship and the hardest to
trust. Start small, declare capabilities honestly, test in disabled state
first, and let the user opt in.*
