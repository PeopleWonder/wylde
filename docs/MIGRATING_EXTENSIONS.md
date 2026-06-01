# Migrating Wylde Extensions to the MCP-server Contract

**Phase:** 4 (Rust migration master plan §5). **Status:** active.
**Audience:** anyone authoring a Wylde extension — Wylde-shipped or
user-installed.

---

## TL;DR

The Phase 3 contract — `manifest.json` + `handler.py` loaded into
the bridge process via `importlib` — is going away. Extensions are
now **Model Context Protocol servers**: separate processes the
Rust `wylde-extension-bridge` host spawns and talks to over MCP
(JSON-RPC 2.0 / stdio). You can author in any language.

You have two migration paths per extension:

1. **Wrap your existing Python `handler.py` with the in-tree
   `wylde_mcp_py_shim`.** Zero code changes; you add a tiny
   `mcp-server.json` next to your existing `manifest.json`. This is
   the recommended path during the strangler window.
2. **Rewrite the extension as a native MCP server** in any language
   (Rust, Python, Node, Go, …). Recommended after the strangler
   default flips and you want the install-footprint win.

The legacy Python bridge keeps working until the
`WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL` env var defaults to `rust`. That
flip will not happen until at least one week of dogfooding has
shipped clean.

---

## Why the change

The old dispatcher loaded your `handler.py` into the bridge process's
Python interpreter. That tied every extension to Python, and made the
bridge process itself a Python process. The new contract:

* lets extensions ship in any language;
* sandboxes each extension in its own OS process (one crashing
  extension can't take the others down);
* gives the host a standard MCP wire to talk to — extensions can
  reuse third-party MCP servers verbatim.

See `docs/wylde-rust-migration-master-plan.md` §5 (Phase 4) for the
architectural justification.

---

## Path 1 — wrap your existing handler via the shim

Most existing Python extensions can migrate with **one new file** and
zero changes to `handler.py`.

### Step 1. Add `mcp-server.json` next to your `manifest.json`.

```jsonc
{
  "name": "<YourExtensionName>",
  "description": "...",
  "version": "1.0",
  "enabled": false,
  "transport": "stdio",
  "command": [
    "${WYLDE_PYTHON}",
    "-m",
    "Extensions._shim.server",
    "--extension",
    "<YourExtensionName>"
  ],
  "cwd": "${WYLDE_ROOT}",
  "env": { "PYTHONPATH": "${WYLDE_ROOT}" },
  "capabilities": ["egress.web"],
  "health": { "method": "ping", "interval_s": 30, "timeout_s": 5 }
}
```

`${WYLDE_PYTHON}` and `${WYLDE_ROOT}` are placeholders the host
substitutes at spawn time (so the manifest stays portable across
machines). `${WYLDE_PYTHON}` defaults to the `.venv` interpreter
under `WYLDE_ROOT`.

`capabilities` is read from `mcp-server.json` if present, otherwise
from your legacy `manifest.json`. Gateway egress allowlists still
consume this field.

### Step 2. Confirm your `handler.py` is shim-compatible.

The shim expects each tool's `endpoint` function to take **one
parameter** — a `dict` of arguments — and return a JSON-serialisable
value. This matches the Phase 3 contract exactly; if your handler
already worked under `Extensions.extension_bridge.dispatcher`, no
changes are needed.

### Step 3. (Optional) Test the shim locally.

```bash
WYLDE_PYTHON="$(pwd)/.venv/Scripts/python.exe" \
  "$WYLDE_PYTHON" -m Extensions._shim.server --extension YourExtensionName
```

The shim reads JSON-RPC on stdin and writes responses to stdout.
A `Ctrl+D` (EOF) cleanly exits. For an end-to-end test, run the
crate's `cargo test -p wylde-extension-bridge --test integration`
which exercises the shim against a synthetic in-tree extension.

### Step 4. Toggle the strangler env var.

```bash
export WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL=rust
# restart the Wylde stack
```

Both your Python handler and the shim will live behind the new
contract — the host is Rust, the extension code is unchanged.

---

## Path 2 — author a native MCP server

If you'd rather drop the Python dependency entirely, ship an MCP
server in any language. The wire protocol is JSON-RPC 2.0 over stdio
(see https://modelcontextprotocol.io/specification/2025-11-25). At
minimum your server must implement:

* `initialize` — handshake; reply with `protocolVersion`,
  `capabilities`, `serverInfo`.
* `notifications/initialized` — host sends this to signal the
  handshake is complete; no response.
* `tools/list` — return your tool catalog.
* `tools/call` — execute one tool.
* `ping` — used by the host's health-check loop.

The host's MCP spec version policy is **N / N-1 / N+1**:

* **N** = `2025-11-25` (the current spec) — accepted.
* **N-1** = `2025-06-18` — accepted.
* **N+1** or unknown — rejected with a clear log line; the
  extension is marked unhealthy in `ext.list`.

Update both constants in `rust/crates/wylde-extension-bridge/src/config.rs`
when bumping.

### Things the host **does** consume

* `tools` capability (your tool catalog).
* `resources` capability (planned — host will route resource reads
  through `ext.resources.*` actions in a follow-up phase).

### Things the host **does not** consume (in this phase)

* `sampling` — the 2026-07-28 spec deprecates this.
* `roots` — the 2026-07-28 spec deprecates this.
* `logging` (MCP `logging/setLevel`) — the 2026-07-28 spec
  deprecates this. Use stderr for log lines; the host captures it.

If your extension uses any of those, raise it in a PR — the host
needs to add the matching capability advertisement.

### Stdio framing rules

* **Stdout is JSON-RPC only.** One JSON message per line. Any
  non-JSON line is logged and dropped by the host.
* **Stderr is logs.** The host captures it through its tracing
  surface; don't print to stderr in tight loops.
* **No banner.** Don't print anything before the first JSON-RPC
  response — the host treats the first non-JSON line as a framing
  error.

---

## Lifecycle, restarts, and health

The host supervises every enabled extension:

* **Spawn:** at startup if `enabled=true`. Failure to spawn marks
  the extension `crashed` and surfaces in `ext.list`.
* **Health:** every 30s (configurable per extension via
  `health.interval_s`), the host sends `ping`. Timeouts mark the
  extension `unhealthy`.
* **Restart:** on crash, exponential backoff (1s, 2s, 4s, … 60s),
  capped at 5 attempts. After the cap, the extension is `broken`
  and stays down until `ext.restart` is called or its manifest
  changes.
* **Shutdown:** SIGTERM (or platform equivalent), wait up to 5s,
  SIGKILL.

Subscribe to `ext.events` (streaming action) for `spawn / exit /
restart / crash / enabled / disabled / unhealthy / healthy` events.

---

## Reference: action surface

The Rust bridge exposes nine first-class actions plus the
`extensions.dispatch` back-compat alias. From the harness or the
GUI, call them via `wylde_shared::ipc::send_action("wylde-extension-bridge", …)`:

| Action | Streaming | Payload |
|---|---|---|
| `ext.list` | no | `{}` |
| `ext.get` | no | `{name}` |
| `ext.enable` | no | `{name}` |
| `ext.disable` | no | `{name}` |
| `ext.tools.list` | no | `{extension?}` |
| `ext.tools.call` | no | `{extension, tool, arguments?}` |
| `ext.health` | no | `{extension}` |
| `ext.restart` | no | `{extension}` |
| `ext.events` | **yes** | `{}` |
| `extensions.dispatch` *(legacy)* | no | `{extension, endpoint, params}` |

---

## FAQ

**Q. My extension's `manifest.json` had a `transport: "http"` field.
Does that matter?**
A. No — Phase 3's `transport` field was reserved-for-future and
never actually changed dispatch behaviour. The Phase 4
`mcp-server.json` `transport` field IS real: `"stdio"` is the only
value implemented today; `"http"` is reserved for a follow-up.

**Q. Per-tool overlay manifests (`tools/<id>/manifest.json`).**
A. Master plan Q-E2: dropped. The shim does not honour them. If
your extension uses overlays, fold them back into the top-level
`manifest.json` (which feeds the shim's tool catalog) before
migrating.

**Q. My handler raises exceptions sometimes — does the shim swallow
them?**
A. The shim translates handler exceptions into JSON-RPC errors with
code `-32000` and includes a 10-line truncated traceback in
`error.data.traceback`. The host then surfaces those as
`mcp_server_error` (for `ext.tools.call`) or `extension_error` (for
the `extensions.dispatch` alias the Gateway still uses).

**Q. Will the Python bridge be deleted?**
A. Yes, but not in this phase. The strangler default stays `python`
for at least a week of dogfooding the Rust impl, then flips to
`rust`. Once Rust is stable in production, the Python
`Extensions/extension_bridge/` package will be reduced to a
deprecation-warning stub that forwards `extensions.dispatch` to the
new Rust pipe, and eventually deleted.
