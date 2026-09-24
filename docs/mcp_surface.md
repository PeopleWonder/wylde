# Wylde MCP surface (v1)

Wylde's Gateway exposes a [Model Context Protocol](https://spec.modelcontextprotocol.io/)
server so external clients (Claude Desktop, the Anthropic API, Cursor, …)
can reach the harness's tool / resource / prompt catalogs through one
standard protocol instead of speaking the Wylde named-pipe IPC.

This is the **v1** surface — intentionally minimal. It is implemented
twice, byte-for-byte equivalent: `Gateway/routes/mcp/` (Python) and
`rust/crates/wylde-gateway/src/routes/mcp/` (Rust).

## Endpoint

| | |
|---|---|
| **URL** | `POST /mcp` on the Gateway |
| **Transport** | MCP Streamable HTTP — one endpoint, JSON-RPC 2.0 bodies |
| **Protocol revision** | `2025-06-18` (pinned) |
| **Sessions** | A session id is minted on `initialize` and returned in the `Mcp-Session-Id` response header. Clients may echo it on later requests; a request without it is still served (stateless fallback). |
| **`GET /mcp`** | `405` — v1 emits no server-initiated SSE stream |

## Auth

`require_device` — the same device-gate Bearer-token tier as
`POST /api/chat/run_turn`. An MCP client authenticates with a device
token:

```
Authorization: Bearer <device-token>
```

A request with no/invalid token is rejected with `401` before any
JSON-RPC dispatch.

## JSON-RPC methods

| Method | What it does |
|--------|--------------|
| `initialize` | Handshake — negotiates `protocolVersion` (echoes the client's if supported, else answers with the server's latest), and returns `capabilities` (`tools`/`resources`/`prompts`) and `serverInfo` (`wylde-gateway-mcp` 1.0.0). |
| `tools/list` | Lists the **allow-listed** harness tools (see [Tool authorization](#tool-authorization)). Each tool: `name`, `description`, `inputSchema`. Paginated. |
| `tools/call` | Runs one allow-listed tool by name (harness `tools.run` → `tool_runner.run_tool`), gated by the caller's device tier. Returns the runner envelope as a text content block; `isError` mirrors the envelope's `ok`. A tool that is not exposed → JSON-RPC `-32001`. |
| `resources/list` | Lists recent conversations + workspaces (see below). Paginated. |
| `resources/read` | Reads one resource by `uri` (see below). |
| `prompts/list` | Lists the system-prompt catalog (harness `prompts.list`). Each prompt: `name`, `description`. Paginated. |
| `prompts/get` | Returns one prompt's resolved text — the saved override if set, else the catalog default — as a single `user` message. |
| `notifications/*` | Accepted as no-ops. |

Any other method → JSON-RPC `-32601` (method not found).

### Pagination

`tools/list`, `resources/list`, and `prompts/list` are paginated. The page
size is 100. A response carries a `nextCursor` (an opaque token) only when
more entries remain; pass it back verbatim as the `cursor` param to fetch
the next page. A malformed cursor → JSON-RPC `-32602`.

### Tool authorization

`tools/list` and `tools/call` are restricted to a curated server-side
allow-list (`MCP_TOOL_ALLOWLIST` in `adapters.rs`) of non-destructive
read/query tools. Tools that mutate, delete, execute, or reach the network
are **not** exposed over MCP regardless of the caller's tier — MCP is an
unattended surface with no way to confirm a destructive action. Defence in
depth:

1. The allow-list (what `tools/list` shows and `tools/call` will run).
2. A `destructive`-flag filter on the live catalog, so a destructive tool
   can never be advertised even if mis-added to the allow-list.
3. The caller's real device tier is passed to `tools.run`, so the harness
   tier gate is the final backstop.

A `tools/call` for a tool that is not exposed returns JSON-RPC `-32001`
(`TOOL_NOT_PERMITTED`) before the harness pipe is touched.

## Resources

`resources/list` enumerates two resource types:

| Type | URI | List source | Read source |
|------|-----|-------------|-------------|
| Conversation | `wylde://conversation/{id}` | harness `conversations.list` | harness `conversations.get` — full conversation document as JSON |
| Workspace file | `wylde://workspace/{workspace_id}/{path}` | harness `workspaces.list_mru` (one entry per workspace) | the file at `{path}` under the workspace's indexed folder, read as UTF-8 text |

Workspace file reads are:

* **Confined** to the workspace root — a `{path}` that resolves outside it
  (via `../`) is rejected.
* **Bounded** — a file larger than 1 MiB is refused before any bytes are
  read, so a large file cannot spike gateway memory.
* **UTF-8 only** — a non-UTF-8 (binary) file is refused rather than
  returned as garbled text.

## Not in v1

Deliberately deferred past v1: server-initiated **sampling**,
`notifications/.../list_changed`, resource **subscriptions**,
**completion**, **logging**, and **roots**. The harness pipe actions
are never modified — the MCP surface is a read/run layer on top of them.

## Verification

- Python unit + integration tests: `Gateway/tests/test_mcp.py`.
- Rust unit + integration tests: `#[cfg(test)]` modules under
  `rust/crates/wylde-gateway/src/routes/mcp/`.
- Cross-language parity: the four gated `mcp_*` cases in
  `rust/tests/parity/tests/gateway.rs`.
