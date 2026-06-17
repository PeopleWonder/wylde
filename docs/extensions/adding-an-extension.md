---
title: Adding an Extension (developer guide)
audience: developers shipping a new Wylde extension end-to-end — no prior Wylde internals assumed
updated: 2026-06-17
status: canonical end-to-end walkthrough. Ties together writing-an-extension.md (quickstart), manifest-reference.md (field tables), and harness-api-reference.md (callback verbs). When this doc and the Rust source disagree, the source wins — every claim below cites the file:line it came from.
---

# Adding an Extension

This is the start-to-finish guide: what an extension *is*, the exact wire
contract the host drives, a copy-paste **hello-world** you can build and run,
and the build → drop-in → enable → discovery lifecycle. It is grounded in the
real code as of trunk `feat/thought-bubble-system @ 6f46f0d`; every non-obvious
claim points at the file and line that backs it.

If you just want the field tables or the callback-verb list, the two companion
references are deeper and you should not duplicate them:

- **[manifest-reference.md](./manifest-reference.md)** — every `mcp-server.json` field, type, default, validation rule.
- **[harness-api-reference.md](./harness-api-reference.md)** — calling *back* into Wylde (memory, chat, gateway egress, consent).
- **[writing-an-extension.md](./writing-an-extension.md)** — the terse quickstart this guide expands on.

---

## 1. What an extension is (and what it is not)

Wylde has a **three-tier** extensibility taxonomy (authoritative as of the reorg
at `2f27975`). Pick the right tier before you write a line of code:

| Tier | What it is | Where it lives | Trust | Build it when… |
| --- | --- | --- | --- | --- |
| **Extension** | A separate **process** that speaks MCP over stdio. The `wylde-extension-bridge` discovers, spawns, supervises, and routes to it. **This is the "leaves-the-ecosystem" tier.** | `Extensions/<Name>/` (manifest) + `rust/crates/wylde-ext-<name>/` (binary) | Out-of-box. Own process, own lifecycle. | The thing reaches the **open web, a third-party SaaS, a browser, or otherwise leaves the enclosed system**. |
| **Plugin** | Core-internal, **compiled into** the GUI/harness. Not a separate process. | `Core/Plugins/` | In-box, trusted, in our address space. | You're extending Core itself and ship as part of the Core build. |
| **Service** | A Wylde-owned, lifecycle-managed Rust binary with its own pipe + manifest. A **sibling** process the core works *without*. | `rust/crates/wylde-<name>/` | In-box, trusted, tight IPC. | The new thing needs its own pipe, state, and lifecycle, and is a peer of the core (e.g. `wylde-ollama`, `wylde-gateway`). |

> **Heuristic (from `docs/extending-wylde.md` §"Design principles"):** *"When in
> doubt, build it as an extension first; promote it to a service only if you need
> IPC integration."* An extension is the lightest, most sandboxed surface.

This guide is **only** about the Extension tier. For the other two see
[extending-wylde-services.md](../extending-wylde-services.md) and
[plugins.md](../plugins.md).

> **Note on older docs.** `docs/extending-wylde.md` and
> `docs/extending-wylde-extensions.md` predate two changes and are stale on these
> points: (1) they say extensions have *"no direct IPC into other Wylde
> services"* — false; a Rust extension links `wylde-shared` and reaches the
> gateway/harness over named pipes (corrected in
> [harness-api-reference.md](./harness-api-reference.md), reference impl at
> `rust/crates/wylde-ext-webcrawler/src/egress.rs:79`). (2) They call this tier a
> "plugin"; the post-reorg taxonomy reserves "Plugin" for `Core/Plugins/`. Trust
> *this* directory (`docs/extensions/`) for the current contract.

---

## 2. Anatomy

An extension is **two things in two places**:

```
Extensions/Hello/
└── mcp-server.json          # the manifest — how to spawn + what you expose

rust/crates/wylde-ext-hello/ # the binary (Rust, per the everything-Rust rule)
├── Cargo.toml
└── src/
    ├── main.rs              # stderr-only logging + serve() entrypoint
    ├── mcp.rs               # the stdio MCP dispatch loop (hand-rolled — see §5)
    └── tools.rs            # your tool implementations
```

- The **manifest** (`Extensions/Hello/mcp-server.json`) is what the bridge scans.
  Its `command` points at the built binary via a placeholder token (§6).
- The **binary** lives in the cargo workspace. It is a normal `[[bin]]` crate
  registered in `rust/Cargo.toml` `members`.

Discovery is **filesystem-based**: the bridge walks `Extensions/` for direct
subdirectories that contain an `mcp-server.json`
(`rust/crates/wylde-extension-bridge/src/discovery.rs:37`). It skips reserved
folders — `extension_bridge`, `_shim`, and anything starting with `_` or `.`
(`discovery.rs:84`). The map is keyed by the manifest's `name` field, **not** the
folder name (`discovery.rs:58`).

---

## 3. The wire contract (what the host does to you)

Wylde is the **MCP client**; your extension is the **MCP server**. After spawn,
the host (`rust/crates/wylde-extension-bridge/src/mcp/client.rs`) drives exactly
this handshake and method set over **newline-delimited JSON-RPC 2.0** on your
stdin/stdout:

| # | Method | Direction | Host sends | You must return |
| --- | --- | --- | --- | --- |
| 1 | `initialize` | request | `{protocolVersion, capabilities:{tools:{},resources:{}}, clientInfo}` (`mcp/wire.rs:59`) | `{protocolVersion, capabilities, serverInfo}` (`client.rs:118`) |
| 2 | `notifications/initialized` | notification | sent right after a good `initialize` (`client.rs:138`) | nothing — no `id`, ack by silence |
| 3 | `tools/list` | request | `{}` (`client.rs:156`) | `{tools:[{name, description, inputSchema}, …]}` |
| 4 | `tools/call` | request | `{name, arguments}` (`client.rs:186`) | `{content:[{type:"text",text}], structuredContent, isError}` |
| 5 | `ping` | request | `{}` — the health probe, every `health.interval_s` (`client.rs:207`) | `{}` |

### The handshake in detail

1. The host spawns your `command` argv as a child with piped stdin/stdout/stderr
   and `kill_on_drop(true)` (`client.rs:82`).
2. It sends `initialize` with `protocolVersion` pinned to **`"2025-11-25"`**
   (`config.rs:12`, `MCP_SPEC_VERSION`). The host accepts your reply if you
   advertise **N or N-1** (`"2025-11-25"` or `"2025-06-18"`); **N+1 is rejected**
   with a clear log line and the spawn fails (`client.rs:120`, `version.rs`).
3. On a good reply it sends `notifications/initialized`. The handshake is now
   complete and `ext.list` reports the extension `running` (`host.rs:239`).
4. Health: every `health.interval_s` (default 30s) the host sends `ping`; a
   `timeout_s` miss (default 5s) marks the extension unhealthy.
5. **EOF on your stdin = shutdown.** Loop until stdin closes; the host kills the
   child on drop.

### Rules that bite

- **`stdout` is the protocol stream — logs go to `stderr`, or you corrupt the
  framing.** The reference binary installs a stderr-only tracing writer for
  exactly this reason (`wylde-ext-webcrawler/src/main.rs:31`).
- **One JSON object per line.** No pretty-printing across newlines on stdout.
- **Notifications (no `id`) are acked by silence** — never write a response for
  `notifications/initialized` (`wylde-ext-webcrawler/src/mcp.rs:57`).

### Error shaping

Return JSON-RPC errors with the standard codes. The reference server uses
`-32601` (method/tool not found) and `-32602` (bad params)
(`wylde-ext-webcrawler/src/mcp.rs:85,92,104`). The host maps any server-side
error to an `IpcError` for the pipe layer
(`wylde-extension-bridge/src/actions/surface.rs:14`): `mcp_server_error`,
`extension_call_timeout`, `mcp_spec_version_unsupported`, etc.

The successful `tools/call` envelope shape is **non-negotiable** — the host and
the Python `_shim` both expect it (`wylde-ext-webcrawler/src/mcp.rs:107`):

```json
{
  "content": [{ "type": "text", "text": "<json-stringified result>" }],
  "structuredContent": { "...": "your structured result object" },
  "isError": false
}
```

---

## 4. The lifecycle (build → drop-in → enable → discovery)

```
 ┌──────────┐   cargo build    ┌──────────┐   drop manifest   ┌──────────┐
 │  crate   │ ───release────▶  │  binary  │   in Extensions/  │ manifest │
 └──────────┘                  └──────────┘                   └────┬─────┘
                                                                   │ bridge scans (mtime-cached)
                                                                   ▼
 ┌─────────────┐  ext.enable   ┌─────────────┐  initialize    ┌─────────────┐
 │  disabled   │ ────────────▶ │  starting   │ ─handshake───▶ │   running   │
 │ (discovered)│   persists    │ (spawn argv)│                │ (tools live)│
 └─────────────┘  enabled=true └─────────────┘                └─────────────┘
```

- **Discovery is cached** by an `(path, mtime, size)` signature over every
  manifest; an unchanged tree returns the cached list with no re-parse
  (`discovery.rs:37`). `ext.enable`/`ext.disable` invalidate the cache
  (`host.rs:375`).
- **A broken manifest does not take the host down** — its parse error is logged
  and that one extension is elided (`discovery.rs:66`).
- **New extensions start `disabled`.** The default `enabled` is `false`
  (`manifest.rs:259`). Nothing spawns until you enable it.
- Lifecycle states: `disabled` · `starting` · `running` · `unhealthy` ·
  `crashed` · `broken` (`host.rs:38`). After `restart_max_attempts` (default 5,
  `config.rs:42`) consecutive spawn failures the extension is marked `broken`.

### Enabling / disabling — where the switch lives

Two paths flip the same bit:

1. **The pipe** — `ext.enable {name}` / `ext.disable {name}` on
   `\\.\pipe\wylde-extension-bridge`. `Host::set_enabled` (`host.rs:360`)
   **persists** the flag by rewriting the `enabled` field in `mcp-server.json`
   (`manifest::write_enabled`, `manifest.rs:714` — round-trips through a
   `serde_json::Value` to preserve field order), invalidates the discovery
   cache, refreshes the catalog, then spawns (enable) or SIGTERMs (disable).
2. **The GUI** — the **Tools → Extensions** panel calls the same `ext.*` actions
   and renders `ext.list` status + `extensions.list_panels` UI panels.

Because the flag is persisted to disk, it survives restarts (`manifest.rs:257`).

### How your tools reach the LLM catalog

This is the part most authors get wrong. There are **two surfaces**, and which
one your tools land on depends on whether you declare a `resources[]` block:

- **Named-tool surface (default for a tools-only manifest).** The harness calls
  `ext.tools.list` with no `{extension}` filter; `Host::aggregate_tools`
  (`host.rs:403`) fans `tools/list` across every running extension and returns
  each tool tagged `{extension, id, name, description, input_schema,
  service:"extension"}`. The model sees flat tool names.
- **Verb / resource surface (when you declare `resources[]`).** Verb mode is
  **ON by default** (`WYLDE_HARNESS_VERB_TOOLS`, `lib.rs:41` — default flipped on
  at the Slice-6 cutover 2026-06-03). With it on, every MCP tool named by a
  resource op is **claimed**: hidden from the flat catalog
  (`host.rs:422`, `named_tool_hidden`) and surfaced instead through the harness
  verb layer. The harness reads your declarations via `ext.resources.list`
  (`host.rs:531`), which namespaces each resource to `ext:<Extension>:<slug>` so
  it can never collide across extensions. The model then calls
  `wylde_execute("ext:Webcrawler:url", "fetch", {url:"…"})` and the harness
  reshapes that into one `ext.tools.call` hop. **A tool is either claimed by one
  resource op or stays named — never both** (`lib.rs:26`).

The full `resources[]` schema lives in
[manifest-reference.md](./manifest-reference.md#resources-resourcedeclaration--slice-5a);
don't re-derive it here.

### The bridge's full action surface (12 actions)

Registered in `wylde-extension-bridge/src/service.rs:16`
(`ALL_ACTIONS`), on `\\.\pipe\wylde-extension-bridge`:

| Action | Payload | Does |
| --- | --- | --- |
| `ext.list` | — | every extension + MCP-server status |
| `ext.get` | `{name}` | one extension's manifest + status |
| `ext.enable` | `{name}` | persist `enabled=true` + spawn |
| `ext.disable` | `{name}` | persist `enabled=false` + SIGTERM |
| `ext.tools.list` | `{extension?}` | aggregate or single-extension live catalog |
| `ext.tools.call` | `{extension, tool, arguments?}` | invoke a tool |
| `ext.resources.list` | `{extension?}` | declared `resources[]` for the verb overlay (answers for disabled) |
| `ext.health` | `{extension}` | send MCP `ping` |
| `ext.restart` | `{extension}` | stop + start |
| `extensions.list_panels` | — | union of every extension's `ui_panels` (never spawns) |
| `ext.events` | — (streaming) | lifecycle event stream: spawn/exit/restart/crash/enabled/disabled |
| `extensions.dispatch` | `{extension, endpoint, params}` | **back-compat alias** → `ext.tools.call`; removed once the Gateway switches |

---

## 5. Hello-world extension (copy this)

> **Resolved open question — is there a shared SDK crate?**
> **No.** There is no `wylde-ext-kit` / `wylde-ext-sdk` / `wylde-ext-common`
> crate. The only extension-related crates in the workspace are
> `wylde-extension-bridge` (the host), `wylde-ext-webcrawler` (the working
> example), and `wylde-ext-study` (excluded — see §8). **Authors hand-roll the
> MCP stdio scaffold** by copying `wylde-ext-webcrawler/src/mcp.rs`. There is no
> `serve()`-loop library, no trait to implement, no generated harness. The
> dispatch loop below *is* the contract; it is ~70 lines and you own it.
>
> *(If a `wylde-ext-kit` ever lands, this section should switch to "implement
> the `Tool` trait and call `kit::serve()". It does not exist today — flagged as
> a real gap, not aspiration.)*

### 5a. `Extensions/Hello/mcp-server.json`

```json
{
  "name": "Hello",
  "description": "Minimal Rust MCP extension — one echo tool.",
  "version": "0.1",
  "enabled": false,
  "transport": "stdio",
  "command": ["${WYLDE_BIN}/wylde-ext-hello"],
  "cwd": "${WYLDE_ROOT}",
  "capabilities": [],
  "health": { "method": "ping", "interval_s": 30, "timeout_s": 5 }
}
```

On Windows the binary is `wylde-ext-hello.exe`; the bridge spawns the argv
directly (no shell), so append `.exe` if your launcher needs it. `${WYLDE_BIN}`
and `${WYLDE_ROOT}` are explained in §6.

### 5b. `rust/crates/wylde-ext-hello/Cargo.toml`

```toml
[package]
name = "wylde-ext-hello"
version.workspace = true
edition.workspace = true

[dependencies]
tokio = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

Add `"crates/wylde-ext-hello"` to `members` in `rust/Cargo.toml` (next to
`"crates/wylde-ext-webcrawler"`, line 19).

### 5c. `src/main.rs` — stderr-only logging + entrypoint

Logging **must** go to stderr or it corrupts the stdout JSON-RPC frame stream
(`wylde-ext-webcrawler/src/main.rs:31`):

```rust
use anyhow::Result;
use tracing_subscriber::EnvFilter;

const SERVICE_NAME: &str = "wylde-ext-hello";

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)   // stdout is the protocol — never log there
        .with_env_filter(filter)
        .try_init();
    tracing::info!("{SERVICE_NAME}: starting MCP stdio server");
    wylde_ext_hello::serve().await
}
```

### 5d. `src/mcp.rs` — the dispatch loop (the whole server)

This is `wylde-ext-webcrawler/src/mcp.rs` with the tool arms swapped. Keep the
envelope shapes byte-for-byte — they are pinned to the host + shim.

```rust
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const MCP_SPEC_VERSION: &str = "2025-11-25"; // must match host N/N-1 policy
pub const SERVER_NAME: &str = "wylde-ext-hello";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run until stdin closes (EOF = shutdown).
pub async fn serve() -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() { continue; }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => { tracing::warn!("ignoring non-JSON line: {e}"); continue; }
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
        let params = match msg.get("params") {
            Some(Value::Null) | None => json!({}),
            Some(p) => p.clone(),
        };
        // No id ⇒ notification (e.g. notifications/initialized) ⇒ ack by silence.
        let id = match msg.get("id").cloned() {
            None | Some(Value::Null) => continue,
            Some(id) => id,
        };
        let response = dispatch(method, id, params).await;
        let mut frame = serde_json::to_string(&response)?;
        frame.push('\n');
        stdout.write_all(frame.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

async fn dispatch(method: &str, id: Value, params: Value) -> Value {
    match method {
        "initialize" => ok(id, json!({
            "protocolVersion": MCP_SPEC_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })),
        "tools/list" => ok(id, json!({ "tools": tool_catalog() })),
        "tools/call" => handle_tools_call(id, params).await,
        "ping" => ok(id, json!({})),
        other => err(id, -32601, &format!("method `{other}` not implemented")),
    }
}

async fn handle_tools_call(id: Value, params: Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return err(id, -32602, "missing string `name`");
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(Value::Object(_)) => params.get("arguments").cloned().unwrap(),
        Some(_) => return err(id, -32602, "`arguments` must be an object"),
    };
    let result = match name {
        "echo" => json!({ "text": args.get("text").and_then(Value::as_str).unwrap_or("") }),
        other => return err(id, -32601, &format!("unknown tool `{other}`")),
    };
    // The non-negotiable tools/call success envelope:
    let structured = if result.is_object() { result.clone() } else { json!({ "value": result }) };
    ok(id, json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }],
        "structuredContent": structured,
        "isError": false,
    }))
}

fn tool_catalog() -> Value {
    json!([{
        "name": "echo",
        "description": "Echo the `text` argument back.",
        "inputSchema": {
            "type": "object",
            "properties": { "text": { "type": "string", "description": "Text to echo" } },
            "required": ["text"]
        }
    }])
}

fn ok(id: Value, result: Value) -> Value { json!({ "jsonrpc": "2.0", "id": id, "result": result }) }
fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn initialize_advertises_protocol() {
        let r = dispatch("initialize", json!(1), json!({})).await;
        assert_eq!(r["result"]["protocolVersion"], MCP_SPEC_VERSION);
    }
    #[tokio::test]
    async fn echo_round_trips_envelope() {
        let r = dispatch("tools/call", json!(2),
            json!({ "name": "echo", "arguments": { "text": "hi" } })).await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["structuredContent"]["text"], "hi");
    }
    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let r = dispatch("nope", json!(3), json!({})).await;
        assert_eq!(r["error"]["code"], -32601);
    }
}
```

### 5e. `src/lib.rs`

```rust
pub mod mcp;
pub use mcp::serve;
```

Those dispatch tests next to `mcp.rs` are **the mergeable bar** — model them on
`wylde-ext-webcrawler/src/mcp.rs:181`.

---

## 6. Placeholder tokens & binary resolution

Resolved at spawn time in `wylde-extension-bridge/src/mcp/client.rs:257`
(`resolve_placeholders`), applied to every `command` argv slot:

| Token | Resolves to | Override env | First-party use |
| --- | --- | --- | --- |
| `${WYLDE_ROOT}` | repo root (`.` if unset) | `WYLDE_ROOT` | `cwd` |
| `${WYLDE_BIN}` | `<root>/rust/target/release` | `WYLDE_BIN` | Rust binary dir → `${WYLDE_BIN}/wylde-ext-<name>` |
| `${WYLDE_PYTHON}` | `<root>/.venv/Scripts/python.exe` (Win) / `.venv/bin/python3` (Unix) | `WYLDE_PYTHON` | **DEPRECATED — test-shim only** (`client.rs:241`) |

> **`${WYLDE_EXT_DIR}` is NOT a real token.** It appears in no manifest and the
> resolver does not handle it (verified: zero hits across `rust/` and `docs/`).
> The bridge's *scan root* is configured by the `WYLDE_EXTENSIONS_DIR` env var
> (`config.rs:59`, default `<root>/Extensions`, falling back to
> `<root>/Wylde/Extensions`) — but that is host config, not a manifest
> placeholder. Do not put `${WYLDE_EXT_DIR}` in a `command`.

**First-party vs third-party command resolution:**

- **First-party Rust** (lives in this repo): `["${WYLDE_BIN}/wylde-ext-<name>"]`.
  The binary is built into the workspace release dir; the token resolves to it.
  This is the Webcrawler shape (`Extensions/Webcrawler/mcp-server.json:7`).
- **Legacy Python** (pre-port, shim path): `["${WYLDE_PYTHON}", "-m",
  "Extensions._shim.server", "--extension", "<Name>"]`. `${WYLDE_PYTHON}` is
  deprecated and survives only for the bridge integration test's `_shim` server
  — no production manifest may use it (`client.rs:241`).
- **Third-party** (out-of-tree binary): use an **absolute path** or a program on
  `PATH` as `command[0]`. There is no token for a third-party install dir
  because there is no out-of-tree install path yet (see §7 packaging).

---

## 7. Trust, capabilities & egress

### What the bridge does and does not sandbox

The "sandbox" is **lifecycle + advisory capabilities**, *not* an OS sandbox
(`harness-api-reference.md` §"What the bridge does NOT give you"):

- The bridge owns your **spawn, supervision, and shutdown**.
- It does **not** give you a filesystem sandbox — a Rust extension has the
  process's full FS access. Capability declarations are **advisory until a tier
  gate consumes them**.
- Nothing stops a Rust extension from opening a pipe to another Wylde service
  (and the reference extension does exactly that for egress).

### The capabilities vocabulary

Declared in the manifest's `capabilities[]`; consumed by the gateway egress
allowlist and the tier/consent gates, **not** transport-enforced
(`manifest-reference.md` §"Capabilities vocabulary",
`Extensions/extension_bridge/contract.py` `LeavesSystem`):

```
egress.web   ·   egress.browser   ·   egress.native
ingress.http ·   ingress.browser
```

Webcrawler declares `["egress.web"]` (`Extensions/Webcrawler/mcp-server.json:11`);
Wylde_Study declares `ingress.browser`, `egress.browser`. **Declare honestly** —
they document intent and feed the gateway/consent gates.

### Internet egress routes through the Gateway

An extension must **not** open raw outbound sockets. Forward through the
Gateway's `egress.forward` action so the allowlist, kill switch, and audit log
apply (`wylde-ext-webcrawler/src/egress.rs:79`):

```rust
use wylde_shared::ipc;

let payload = json!({
    "caller": "Hello",            // MUST match the name that declared the egress key
    "dest":   "web",              // egress destination key
    "method": "GET",
    "path":   url,                // full URL for wildcard destinations
    "headers": { "User-Agent": "Wylde-Hello/0.1" },
    "timeout": 10.0
});
let resp = ipc::call_action("wylde-gateway", "egress.forward", payload).await?;
// resp: { status, body, headers }
```

A **reachable** Gateway returning a policy denial (`egress_blocked`,
`egress_denied`) or an upstream failure must **surface as an error** — do not
retry with a direct request. Only a *transport* failure (Gateway pipe down)
justifies a loud direct-`reqwest` fallback (`egress.rs:46`,
`fetch_via_gateway_or_fallback`). The full callback-verb list (memory, chat,
consent) is in [harness-api-reference.md](./harness-api-reference.md).

### Consent & destructive tier

Tool dispatch passes three gates in `wylde-harness/src/tooling/`: **registry**
(resolvable?), **tier** (does the turn's `device_tier` permit a `destructive`
tool?), **consent** (has the user approved it?). A resource op marked
`destructive: true` raises a `destructive_tool_access` tier requirement and a
consent prompt; `false` is read with no prompt (`manifest-reference.md`
§OperationDeclaration). An extension wrapping a destructive capability should
expect `Pending` on first use and surface the prompt rather than treating it as
failure.

---

## 8. The real reference manifest — Webcrawler, annotated

`Extensions/Webcrawler/mcp-server.json`, the canonical first-party extension
(native Rust `wylde-ext-webcrawler`, Python shim/handler retired):

```jsonc
{
  "name": "Webcrawler",                       // discovery key; must be non-empty (manifest.rs:399)
  "description": "Fetch raw URL contents, scrape HTML…",
  "version": "1.0",                           // free-form; default "1.0"
  "enabled": false,                           // ships disabled; ext.enable flips + persists
  "transport": "stdio",                       // only stdio runs; http parses then rejects (manifest.rs:414)
  "command": ["${WYLDE_BIN}/wylde-ext-webcrawler"],  // first-party Rust binary (§6)
  "cwd": "${WYLDE_ROOT}",                     // child working dir; relative → manifest parent
  "capabilities": ["egress.web"],             // advisory; feeds gateway allowlist + consent
  "resources": [                              // declares the verb/resource surface (§4)
    {
      "resource_type": "url",                 // → ext:Webcrawler:url after namespacing
      "display_name": "Web URL",
      "description": "Fetch/scrape/extract; read-only; egress gated by the Gateway.",
      "scope": "global",                      // global | workspace | conversation
      "schema_version": 1,                    // must be 1 (manifest.rs:502)
      "operations": {
        "execute": {                          // one of list/get/create/update/delete/search/execute
          "description": "Run a web action against a URL. 'action' selects fetch|scrape|extract.",
          "destructive": false,               // false ⇒ read, no consent prompt
          "tier": "read",                     // advisory; only `destructive` is enforced today
          "actions": [                        // an execute op binds each action to an mcp_tool
            { "name": "fetch",   "mcp_tool": "fetch",   "description": "Fetch raw URL contents." },
            { "name": "scrape",  "mcp_tool": "scrape",  "description": "Scrape HTML with CSS selectors." },
            { "name": "extract", "mcp_tool": "extract", "description": "Extract structured data via a rule set." }
          ]
        }
      }
    }
  ],
  "health": { "method": "ping", "interval_s": 30, "timeout_s": 5 }  // defaults if omitted (manifest.rs:82)
}
```

Note the `execute` op has **no op-level `mcp_tool`** — that is legal *only*
because every action supplies its own override, and the validator checks exactly
that (`manifest.rs:526`). The three tools (`fetch`/`scrape`/`extract`) are
*claimed*: in verb mode they vanish from the named catalog and the model reaches
them via `wylde_execute("ext:Webcrawler:url", "fetch", {…})`.

For the **panel-only** shape (`transport:"none"`, no command, `ui_panels` only —
the N8N editor iframe) and the full field tables, see
[manifest-reference.md](./manifest-reference.md).

---

## 9. Build, test, ship

```powershell
# 1. Build the binary into ${WYLDE_BIN} (rust/target/release)
cargo build --release -p wylde-ext-hello

# 2. Your dispatch tests (the mergeable bar)
cargo test -p wylde-ext-hello

# 3. The bridge still parses every manifest, including yours
cargo test -p wylde-extension-bridge manifest

# 4. Drive it live over the bridge pipe (\\.\pipe\wylde-extension-bridge):
#    ext.enable {name:"Hello"} → ext.tools.list → ext.tools.call {extension:"Hello", tool:"echo", arguments:{text:"hi"}}
```

"Install" = drop the directory under `Extensions/` and let discovery pick it up
(the bridge re-scans on cache invalidation / catalog refresh). Then `ext.enable`.

### Packaging: first-party vs third-party

- **First-party** (this repo): the manifest under `Extensions/<Name>/` **plus**
  the crate in `rust/Cargo.toml` `members`. Built with the workspace; spawned via
  `${WYLDE_BIN}`.
- **Third-party / out-of-tree:** **there is no package format, signing, versioned
  bundle, or out-of-tree install path today** (flagged, not built —
  `writing-an-extension.md` §6). The only install mechanism is "drop a directory
  with an `mcp-server.json` into the scan root and point `command` at an absolute
  binary path." Treat a third-party extension as fully trusted code: it runs
  unsandboxed in its own process with your FS access.

---

## Appendix — Bringing back WyldeStudy

WyldeStudy is the worked example of an extension that was **excluded but
source-retained**. The crate `wylde-ext-study` was moved from `members` to
`exclude` in `rust/Cargo.toml:65` by memory plan **M7** (rag retirement), so it
is out of the default `cargo build`/`test` but the source survives as the seed of
the future Extension. Both `Extensions/Wylde_Study/{manifest,mcp-server}.json`
are already `enabled:false`, so the bridge advertises no `study_*` tools.

The **rewire spec** is `outputs/wylde-memory-fixes-DONE.md` §"WyldeStudy rewire
spec". In short, when it returns as a proper Extension (leaves-the-ecosystem
tier):

- **Own its storage.** The old `study_index_page` wrote into core memory via the
  now-deleted `rag.add_episodic`. The Extension must own a **per-corpus /
  per-session episodic store** — browsing history stays out of the user's durable
  long-term tier (the leave-the-ecosystem boundary).
- **Embed-on-write + top-k search.** `study_query` needs embed-on-ingest + top-k
  cosine, either via its own embedder or a **deliberately designed retrieval
  verb** across the bridge/Gateway boundary — *not* the deleted internal `rag.*`
  surface.
- **LLM passthroughs unaffected.** `study_summarize`/`study_explain`/
  `study_flashcards` already route to `chat.complete` and work as-is on re-enable.
- **Re-enable path:** add `crates/wylde-ext-study` back to `members`, flip both
  `Extensions/Wylde_Study` manifests to `enabled:true`, and point
  `study_index_page`/`study_query` at the new storage/retrieval contract. The MV3
  `browser_extension/` and the Gateway routing are unchanged.

---

## Cross-links

- [writing-an-extension.md](./writing-an-extension.md) — terse quickstart (the 5-method contract).
- [manifest-reference.md](./manifest-reference.md) — every `mcp-server.json` field + validation rule.
- [harness-api-reference.md](./harness-api-reference.md) — callback verbs, gateway egress, consent.
- `rust/crates/wylde-extension-bridge/` — the host: `discovery.rs`, `manifest.rs`, `host.rs`, `mcp/client.rs`, `service.rs`.
- `rust/crates/wylde-ext-webcrawler/` — the canonical Rust extension to copy.
- `outputs/wylde-memory-fixes-DONE.md` — the WyldeStudy rewire spec (Appendix).
- [../extending-wylde.md](../extending-wylde.md) — the (pre-reorg) extensibility hub; trust this directory over it for the extension contract.
- [MCP spec](https://modelcontextprotocol.io) — upstream protocol.
