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
| `initialize` | Handshake — returns `protocolVersion`, `capabilities` (`tools`/`resources`/`prompts`), and `serverInfo` (`wylde-gateway-mcp` 1.0.0). |
| `tools/list` | Lists the harness tool catalog (harness `tools.list`). Each tool: `name`, `description`, `inputSchema`. |
| `tools/call` | Runs one tool by name (harness `tools.run` → `tool_runner.run_tool`). Returns the runner envelope as a text content block; `isError` mirrors the envelope's `ok`. |
| `resources/list` | Lists recent conversations + workspaces (see below). |
| `resources/read` | Reads one resource by `uri` (see below). |
| `prompts/list` | Lists the system-prompt catalog (harness `prompts.list`). Each prompt: `name`, `description`. |
| `prompts/get` | Returns one prompt's resolved text — the saved override if set, else the catalog default — as a single `user` message. |
| `notifications/*` | Accepted as no-ops. |

Any other method → JSON-RPC `-32601` (method not found).

## Resources

`resources/list` enumerates two resource types:

| Type | URI | List source | Read source |
|------|-----|-------------|-------------|
| Conversation | `wylde://conversation/{id}` | harness `conversations.list` | harness `conversations.get` — full conversation document as JSON |
| Workspace file | `wylde://workspace/{workspace_id}/{path}` | harness `rag.workspaces.list` (one entry per workspace) | the file at `{path}` under the workspace's indexed folder, read as text |

Workspace file reads are confined to the workspace root — a `{path}`
that resolves outside it (via `../`) is rejected.

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
