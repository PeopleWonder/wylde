# Wylde MCP surface (v1)

Wylde's Gateway exposes a [Model Context Protocol](https://spec.modelcontextprotocol.io/)
server so external clients (Claude Desktop, the Anthropic API, Cursor, …)
can reach the harness's tool / resource / prompt catalogs through one
standard protocol instead of speaking the Wylde named-pipe IPC.

This is the **v1** surface — intentionally minimal. It lives in
`rust/crates/wylde-gateway/src/routes/mcp/` (the Python gateway was removed
in the full-Rust cutover).

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
| `tools/list` | Lists the tools available to the caller's tier (see [Tool authorization](#tool-authorization)). Each tool: `name`, `description`, `inputSchema`, and `annotations` (`readOnlyHint`/`destructiveHint`). Paginated. |
| `tools/call` | Runs one tool by name (harness `tools.run` → `tool_runner.run_tool`), gated by the caller's tier + a `confirm` arg for destructive tools (see [Tool authorization](#tool-authorization)). Returns the runner envelope as a text content block; `isError` mirrors the envelope's `ok`. Not exposed → `-32001`; destructive without confirm → `-32002`. |
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

Tool exposure depends on the caller's **device tier** (from `require_device`)
and, for destructive tools, an explicit **`confirm`** argument. The harness
`destructive` flag is the source of truth for what counts as destructive.

**Non-destructive tools** are a curated server-side allow-list
(`MCP_TOOL_ALLOWLIST` in `routes/mcp/authz.rs`) of read/query tools. They are listed
and runnable for any authenticated device, with no confirmation. Tools that
are non-destructive but *not* on the allow-list (e.g. `execute_bash`) stay
hidden from every tier.

**Destructive tools** (mutate/delete, etc.) are gated:

| Caller tier | `tools/list` shows them? | `tools/call` |
|---|---|---|
| `tool_use` (default) | no | `-32001 TOOL_NOT_PERMITTED` (not even acknowledged to exist) |
| `destructive_tool_access` | yes, with `annotations.destructiveHint: true` | needs `arguments.confirm: true` |

For a `destructive_tool_access` caller invoking a destructive tool:

* `confirm: true` → the call proceeds to `tools.run` (carrying the real
  device tier).
* `confirm` missing/false → `-32002 CONFIRMATION_REQUIRED` ("resend with
  `confirm: true`") — never run silently, never a silent no-op.

`confirm` is an MCP protocol flag: it is read for the gate and then
**stripped** from `arguments` before the tool sees it.

**Consent backstop.** The gateway gate is not the last word — the harness
`tools.run` runs its own tier gate *and* a per-tool **consent gate**. The
MCP `confirm` is forwarded to `tools.run` as a **per-call, non-persisted**
confirmation, so a confirmed call executes end-to-end:

* Consent **undecided** (never prompted / no stored decision) + `confirm`
  → the harness runs it for this one call. Nothing is persisted, so the
  next call without `confirm` prompts again.
* Consent **explicitly denied** → still blocked (`consent_denied`),
  `confirm` and all. A remote/MCP caller can never override the user's
  local deny — the local consent gate is the ultimate authority.
* A stored **approval** / `no_auth` / bypass → runs as before.

Allow-listed non-destructive tools are sent with `confirm: true` too (they
are pre-vetted for MCP), so a first call clears an undecided gate instead of
returning `consent_required`; a stored deny still blocks them.

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

- Rust unit tests: `#[cfg(test)]` modules under
  `rust/crates/wylde-gateway/src/routes/mcp/` (gate/authorization,
  pagination, negotiation, resource sandboxing).
- Harness-side per-call confirm + consent guardrail:
  `dispatch_confirm_*` tests in
  `rust/crates/wylde-harness/src/tooling/runner.rs`.
- The surface is Rust-only; there is no Python twin or cross-language
  parity gate for it.
