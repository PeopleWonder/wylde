---
title: Writing a Wylde extension
audience: developers adding a new extension — no prior Wylde internals assumed
updated: 2026-06-17
status: quickstart; the end-to-end walkthrough is docs/extensions/adding-an-extension.md. Supersedes the design-era notes in docs/extending-wylde-extensions.md.
---

# Writing a Wylde extension

An extension is a separate program that speaks **MCP (Model Context Protocol)
over stdio**. The `wylde-extension-bridge` discovers it, spawns it, watches it,
and routes tool calls to it. Any language that can read JSON-RPC from stdin and
write it to stdout qualifies — but per the repo's everything-Rust rule, **new
extensions are Rust binaries** (`rust/crates/wylde-ext-webcrawler/` is the
canonical example). Python is supported only through a legacy shim
(`Extensions/_shim/server.py`) for code not yet ported.

This guide is the quickstart. For the full start-to-finish walkthrough — the wire
contract in detail, a buildable hello-world, the lifecycle, trust/egress, and the
annotated Webcrawler manifest — see the canonical
**[adding-an-extension.md](./adding-an-extension.md)**; this quickstart is the
terse version of it, so don't duplicate it. Two companions cover the rest:

- [manifest-reference.md](./manifest-reference.md) — every `mcp-server.json` field.
- [harness-api-reference.md](./harness-api-reference.md) — how to call **back**
  into Wylde (memory, chat, gateway egress, consent) from your extension.

The older [docs/extending-wylde-extensions.md](../extending-wylde-extensions.md)
predates the Rust extension path and the consent gate; read it for the UI-panel
deep dive, but trust *this* directory for the current contract.

## 1. The contract (what the bridge does to you)

Wylde is the **MCP client**; your extension is the **MCP server**. After spawn,
the bridge drives exactly five methods over newline-delimited JSON-RPC 2.0 on
your stdin/stdout (see `rust/crates/wylde-extension-bridge/src/mcp/client.rs`
and the reference impl in `rust/crates/wylde-ext-webcrawler/src/mcp.rs`):

| Method | Direction | You must return |
| --- | --- | --- |
| `initialize` | call | `{protocolVersion, capabilities, serverInfo}` |
| `notifications/initialized` | notification | nothing (no `id` → ack by silence) |
| `tools/list` | call | `{tools: [{name, description, inputSchema}, …]}` |
| `tools/call` | call | `{content:[{type:"text",text}], structuredContent, isError}` |
| `ping` | call | `{}` (liveness; every `health.interval_s`, default 30s) |

**Rules that bite:**

- **`stdout` is the protocol stream.** All logs go to `stderr`, or you corrupt
  the framing. The Rust example installs a stderr-only tracing writer for
  exactly this reason (`wylde-ext-webcrawler/src/main.rs:31`).
- **Protocol version is `"2025-11-25"`** (pinned in
  `wylde-extension-bridge/src/config.rs` as `MCP_SPEC_VERSION`; the previous
  `"2025-06-18"` is also accepted, N+1 is rejected). Advertise the current one.
- **EOF on stdin = shutdown.** Loop on stdin until it closes; the bridge sends
  `kill_on_drop`.

## 2. Scaffold

An extension is a directory under `Extensions/<Name>/` containing at minimum a
manifest. The bridge scans `Extensions/` on startup, skipping `extension_bridge`,
`_shim`, and any name starting with `_` or `.`.

```
Extensions/Hello/
└── mcp-server.json        # required — how to spawn + what you expose
```

A Rust extension's *binary* lives in the cargo workspace, not here — the
manifest just points at it:

```
rust/crates/wylde-ext-hello/
├── Cargo.toml
└── src/
    ├── main.rs            # stderr logging + serve() loop
    ├── mcp.rs             # the 5-method dispatch (copy from wylde-ext-webcrawler)
    └── tools.rs           # your tool implementations
Extensions/Hello/
└── mcp-server.json
```

## 3. The manifest

Minimal Rust extension (`Extensions/Hello/mcp-server.json`):

```json
{
  "name": "Hello",
  "description": "Minimal Rust MCP extension example.",
  "version": "0.1",
  "enabled": false,
  "transport": "stdio",
  "command": ["${WYLDE_BIN}/wylde-ext-hello"],
  "cwd": "${WYLDE_ROOT}",
  "capabilities": [],
  "health": { "method": "ping", "interval_s": 30, "timeout_s": 5 }
}
```

`${WYLDE_BIN}` resolves to `rust/target/release`, `${WYLDE_PYTHON}` to the venv
interpreter, `${WYLDE_ROOT}` to the repo root (substituted in
`wylde-extension-bridge/src/mcp/client.rs`). On Windows append `.exe` to the
binary if you spawn it without a shell. Full field list:
[manifest-reference.md](./manifest-reference.md).

> The shipped `Extensions/Webcrawler/mcp-server.json` now spawns the **native
> Rust binary** — `"command": ["${WYLDE_BIN}/wylde-ext-webcrawler"]`
> (`Extensions/Webcrawler/mcp-server.json:7`); the `${WYLDE_PYTHON} -m
> Extensions._shim.server` path and `handler.py` are retired. Use it as the
> template for a first-party Rust extension. `${WYLDE_PYTHON}` survives only for
> the bridge integration test's `_shim` server and is deprecated
> (`wylde-extension-bridge/src/mcp/client.rs:241`).

## 4. Implement the server

The whole server is the 5-method dispatch. Copy
`rust/crates/wylde-ext-webcrawler/src/mcp.rs` verbatim and replace the tool
match arm:

```rust
async fn handle_tools_call(id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let result = match name {
        "hello.echo" => json!({ "text": args.get("text") }),
        other => return err(id, -32601, &format!("unknown tool `{other}`"), None),
    };
    // MCP envelope — keep this exact shape:
    ok(id, json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap() }],
        "structuredContent": result,
        "isError": false,
    }))
}
```

Declare the tool in `tools/list` with a JSON-Schema `inputSchema`
(`mcp.rs:125`). Tools are pure data — the bridge forwards `name` + `arguments`
and returns your result as-is.

### Python (legacy path only)

If you must ship Python before porting, write `Extensions/<Name>/handler.py`
with one function per tool taking `(params: dict) -> dict`, declare a legacy
`manifest.json`, and point `command` at the shim. The shim
(`Extensions/_shim/server.py`) translates the legacy manifest into MCP and wraps
each handler return in the MCP envelope. `Extensions/Wylde_Study/handler.py` is
the canonical Python example (5 tools: index/query/summarize/explain/flashcards).

## 5. Calling back into Wylde

Extensions are **not** as isolated as the older docs claim. A Rust extension
links `wylde-shared` and reaches Wylde services over named pipes:

```rust
use wylde_shared::ipc;
let data = ipc::call_action("wylde-gateway", "egress.forward", payload).await?;
```

This is how `wylde-ext-webcrawler` makes web requests — through the gateway's
allowlist, not raw sockets (`wylde-ext-webcrawler/src/egress.rs:79`). The same
helper reaches the harness for memory, chat, and tool dispatch. Full verb list
and the actual sandbox/consent model: [harness-api-reference.md](./harness-api-reference.md).

## 6. Enable, test, ship

```powershell
# Build the binary
cargo build --release -p wylde-ext-hello

# Manifest parses + bridge unit tests
cargo test -p wylde-extension-bridge manifest

# Confirm discovery (extension shows as disabled until enabled)
cargo test -p wylde-ext-hello                    # your own dispatch tests

# Drive it through the live bridge pipe
#   ext.list / ext.enable / ext.tools.call live on \\.\pipe\wylde-extension-bridge
```

The `ext.*` action surface (`ext.list`, `ext.enable`, `ext.disable`,
`ext.tools.list`, `ext.tools.call`, `ext.health`, `ext.restart`, `ext.events`)
is documented in
[docs/extending-wylde-extensions.md](../extending-wylde-extensions.md#the-bridges-action-surface).
Each Rust extension should ship dispatch tests next to `mcp.rs` (see the
`#[tokio::test]` block at `wylde-ext-webcrawler/src/mcp.rs:181`) — that's the
mergeable bar.

**Packaging:** there is no separate package format today. An extension ships as
its directory under `Extensions/` plus, for Rust, its crate in the workspace.
Discovery is filesystem-based; "install" = drop the directory in and restart the
bridge. (Flagged: no signing, no versioned bundle, no out-of-tree install path
yet.)

## Cross-links

- [adding-an-extension.md](./adding-an-extension.md) — the canonical end-to-end walkthrough this quickstart condenses.
- [manifest-reference.md](./manifest-reference.md) — manifest field reference.
- [harness-api-reference.md](./harness-api-reference.md) — pipe verbs + consent.
- [../extending-wylde-extensions.md](../extending-wylde-extensions.md) — UI panels, bridge action surface, services-vs-extensions.
- `rust/crates/wylde-ext-webcrawler/` — canonical Rust extension.
- `Extensions/Wylde_Study/` — canonical Python (legacy shim) extension.
- [MCP spec](https://modelcontextprotocol.io) — upstream protocol.
