# R3 Gateway — wave 2+ queue

R3 wave 1 shipped a minimum-viable Rust Gateway port: `/health`, request
trace + audit-log middleware, the device-gate IPC wrapper, a minimal
`proxy_core`, the named-pipe action shell, and the lifecycle wiring
(`configure_logging`, manifest + 60s heartbeat, `mark_stopped` on
shutdown).

Wave 2a added the chat-adjacent surface: `POST /api/chat/run_turn`,
conversations CRUD (`/api/conversations`, `/api/conversations/:id`), and
prompts CRUD (`/api/prompts`, `/api/prompts/presets`,
`/api/prompts/active`).

Wave 2a.1 finished the chat surface deferred out of 2a: `POST /api/chat`
and `POST /api/chat/generate` — the two Ollama-proxy SSE routes. Both
stream NDJSON from the local Ollama daemon at `127.0.0.1:11434` and
re-emit it as SSE through wave-2c's `streaming::ndjson_to_sse`
(`event: token` frames, `event: done` terminator keyed off Ollama's
`done` field). Auth is the same Bearer-token substitution waves 2b–2e
use in place of `require_local`, pending `auth/`.

Wave 2b added the memory-adjacent surface:
`/api/memory/long_term/*`, `/api/memory/workspace/:workspace_id/*`,
`/api/memory/short_term/:conversation_id`, `POST /api/memory/reflect`;
`/api/workspaces`, `/api/workspaces/recent`, `/api/workspaces/mru_limit`,
`/api/workspaces/activate`, `/api/workspaces/:workspace_id` (DELETE),
`/api/workspaces/:workspace_id/{status,reindex,persona}`; and the legacy
MCP-facing rag proxy `/api/rag/{query,ingest,collections}`.

Wave 2c added the model-adjacent surface: `GET /api/models`,
`GET /api/models/running`, `POST /api/models/pull` (NDJSON→SSE),
`POST /api/models/generate`, `DELETE /api/models/:name` — proxying the
local Ollama daemon at `127.0.0.1:11434`. Brought online
[`proxy_core::http_call`] (a `reqwest`-based localhost-HTTP transport)
and `streaming.rs` (NDJSON→SSE bridge — `Gateway/streaming.py::ndjson_to_sse`).

Wave 2d added the peripheral route surface: `routes/devices.rs`,
`routes/link.rs`, `routes/images.rs`, `routes/settings.rs`. (The
wave-2d `routes/voice.rs` and `routes/push.rs` were removed in the
Bucket-A IPC cleanup — both mirrored Python's Flask-style pipe surface
via [`wylde_shared::ipc::send`] onto upstream handlers that were never
wired, so they only ever 404'd in production.) Devices
dispatches `device_gate.*` actions through `services::device_gate`. Link
proxies the VPN management HTTP API at `127.0.0.1:8020`. Images proxies
the image-gen service at `127.0.0.1:8014` and reads the on-disk library
at `$WYLDE_ROOT/data/images/`. Settings reads / writes
`$WYLDE_ROOT/data/settings/ollama.json`.

Wave 2e (this wave) closes the subsystem queue:

* **Egress subsystem** — `egress/kill_switch.rs` (process-wide AtomicBool
  + `WYLDE_GATEWAY_EGRESS_KILL_SWITCH_INIT` bootstrap), `egress/destinations.rs`
  (walks `Wylde/<component>/manifest.json` + `Wylde/Extensions/<ext>/manifest.json`
  at startup, scopes destinations to the declaring component, validates
  paths against wildcard / pinned modes), and `egress/client.rs` (reqwest
  wrapper composing all three with the per-call audit line). The audit
  hook reuses [`middleware::audit_log::emit_egress`] (already shipped in
  wave 1).
* **Secrets subsystem** — `secrets/mod.rs` (`SecretsProvider` trait +
  `get_secrets()` global) and `secrets/file_backend.rs` (`.env` reader
  with OS-environ pass-through). Backend selection reads
  `WYLDE_GATEWAY_SECRETS_PROVIDER`; at this wave an unknown / `"vault"`
  value fell through to the file backend with a one-line warning — wave
  2k (below) promotes `"vault"` to a real backend.
* **Egress HTTP routes** — `routes/egress.rs` mounts
  `GET /api/egress/destinations`, `POST /api/egress/kill`,
  `POST /api/egress/forward`, `POST /api/egress/stream`. Same Bearer-token
  substitution waves 2b/2c/2d used in place of `require_local`, pending
  `auth/`.
* **Egress pipe actions** — `egress.forward`, `egress.kill_switch`,
  `egress.destinations` are live in `src/pipe.rs` (wave-1 stubs
  replaced).
* **Tool-registry route** — `routes/tool_registry.rs` dispatches
  `tools.list` to `wylde-harness` and **reshapes** the canonical list
  into the alias-keyed dict shape Python returns. Alias keys mirror
  Python's `_alias_keys_for(entry)`: `id`, `id.replace("_", ".")`,
  `name`, `name.replace(".", "_")`. The pipe actions `tools.list` and
  `tools.get` go through the same reshape so HTTP and pipe surfaces
  produce byte-equivalent shapes.
* **Extensions route** — `routes/extensions.rs` is a forward-compatible
  stub: returns `503 extension_bridge_unavailable` for every call. The
  Python implementation loads `extension_bridge` in-process; no
  `extension-bridge` pipe service exposes the `extensions.dispatch`
  action externally, so the Rust port has no upstream to dispatch to. The
  pipe action `extensions.dispatch` returns the same envelope. When the
  extension bridge ships a pipe service, both the route and the pipe
  handler gain a one-line dispatch — the surface is in place.
* **MCP routes** — `Gateway/app.py` flags MCP as a deferred concern in
  Python too ("Drop it in as a router under `Gateway.routes.mcp` when
  the protocol shape settles"). There is **no Python source to mirror**
  yet, so the Rust port has no MCP route to write. This row stays
  deferred until Python lands a concrete MCP surface.

Wave 2f closes the local-CIDR auth tier. `auth/mod.rs` ports
`Gateway/auth/` — `require_public`, `require_local`, and `require_device`
as `axum::middleware::from_fn` layers; the CIDR allowlist follows
principle #16 (loopback + legacy mesh + the WyldeLink CGNAT block).
`auth/token_cache.rs` is the standalone 60s token cache — an
async-`Mutex` `HashMap<token, (Device, expires_at)>` that evicts stale
entries on read. Every route that was substituting Bearer-token
`common::authorize` for `require_local` now layers the real tier: the
loopback/CGNAT routes (`chat`'s Ollama proxies, `models`, `images`,
`link`, `rag`, `settings`, `tool_registry`, `egress`,
`extensions`, and the `devices` GUI surface) gate on `require_local`;
`chat.run_turn` and `devices/me` gate on `require_device`; `/health` is
explicit `require_public`. The harness-mirror routes (`conversations`,
`prompts`, `memory`, `workspaces`) have no Python Gateway counterpart
and stay on `common::authorize` (Bearer-only).

Wave 2g closes the events middleware. `middleware/events.rs` ports
`Gateway/events.py` — the `forward_device_events`
`axum::middleware::from_fn` layer drains device_gate's pending-event
queue for the verified device and forwards the events on the
`X-Wylde-Events` response header as a compact JSON array (header omitted
when the queue is empty). It mounts inner to `require_device` on the two
device-tier routes (`/api/chat/run_turn`, `/api/devices/me`) so the
verified `Device` extension is already populated; a route with no device
is a graceful no-op. Being a per-route layer it sits inside the global
`audit_log` layer — the header is set before audit observes the
response. Python's `stash_pending_events` / `attach_pending_events` split
(a Starlette workaround) collapses into the single layer; the no-op
`install_events_middleware` stub has no Rust equivalent.

Wave 2h closes the rate-limit middleware. `middleware/rate_limit.rs`
ports `Gateway/middleware/rate_limit.py` — a `RateLimiter` plus a
`rate_limit` `axum::middleware::from_fn_with_state` layer. Despite the
"sliding window" framing this doc used, the Python window math is a
*fixed* window: one 60-second bucket per key, keyed on the calendar
minute (`unix_secs / 60`) and reset lazily on the first write of a new
minute. The Rust port mirrors that exactly (including the `>4096`-bucket
compaction). The bucket key is `dev:<device_id>` when a verified
`Device` is in the request extensions, else `ip:<addr>` (`ip:unknown`
with no `ConnectInfo`); `/health` is exempt. It mounts as the innermost
global layer in `app::build_router` — `CORS → Trace → AuditLog →
RateLimit → routes` — the same position outside the per-route auth tier
that Python's global `RateLimitMiddleware` holds, so in practice it keys
by IP; the `dev:` branch activates only when a `Device` is already on
the request. An over-limit request gets `429 rate_limited` with the
canonical `{ok:false, error:{...}}` envelope.

Wave 2i closes the extensions row. The `extension-bridge` pipe service
— `Extensions/extension_bridge/{pipe,run}.py` + `manifest.json` — now
hosts `\\.\pipe\wylde-extension-bridge`, registering the
`extensions.dispatch` action as a thin wrapper over the unchanged
in-process `extension_bridge.dispatch_external`. With that upstream in
place, `routes/extensions.rs` and `pipe::handle_extensions_dispatch`
were both promoted from the `503 extension_bridge_unavailable` stub to
a real dispatch through
[`crate::routes::extensions::dispatch_through_bridge`]. The Python
Gateway's `extension_routes.handle_extension_request` was flipped onto
the same pipe, so HTTP and pipe surfaces — on both implementations —
fold the bridge's typed errors (`extension_not_found` → 404,
`extension_disabled` → 409, `extension_error` → 500) and any
pipe-transport fault (→ `extension_bridge_unavailable` 503)
identically. The bridge is Python-only — there is no Rust port — and is
spawned as a daemon-managed tier=core service (`_start_extension_bridge`
in both the Python and Rust lifecycle daemons) before the Gateway.

Wave 2j lands the MCP server surface — the row this doc had blocked on
a concrete Python implementation. `Gateway/routes/mcp/` (Python) and
`rust/crates/wylde-gateway/src/routes/mcp/` (Rust) expose a v1 Model
Context Protocol server at `POST /mcp` — the Streamable HTTP transport
from the 2025-06-18 spec revision. Each side splits the same three ways:
`transport` (JSON-RPC framing + `Mcp-Session-Id` session tracking),
`handlers` (JSON-RPC method dispatch), and `adapters` (bridges to the
harness pipe). The surface is intentionally minimal — `initialize`,
`tools/list` + `tools/call`, `resources/list` + `resources/read`,
`prompts/list` + `prompts/get`. `tools/*` bridges the `tools.list` /
`tools.run` harness actions (`tools.run` runs `tool_runner.run_tool`,
the same path an in-process turn uses); `resources/*` exposes
`wylde://conversation/{id}` (via `conversations.list` /
`conversations.get`) and `wylde://workspace/{workspace_id}/{path}` (via
`rag.workspaces.list` plus a workspace-confined file read); `prompts/*`
bridges `prompts.list`, resolving the saved override or the catalog
default. The harness pipe actions are unchanged — MCP is a read/run
surface layered on top of them. `GET /mcp` returns `405` (v1 has no
server-initiated SSE stream); sampling, `*/subscribe`,
`.../list_changed` notifications, and completion are explicitly
deferred past v1. Both verbs gate on `require_device` — MCP clients
authenticate with a device-gate Bearer token, the same tier as
`chat.run_turn`. The parity suite gains four gated `mcp_*` cases in
`rust/tests/parity/tests/gateway.rs`; `docs/mcp_surface.md` documents
the exposed surface.

Wave 2k closes the Vault secrets backend — the last subsystem row this
doc had queued. `Gateway/secrets/vault_backend.py` (Python) and
`rust/crates/wylde-gateway/src/secrets/vault_backend.rs` (Rust) add a
`VaultBackend` alongside the file backend: a HashiCorp Vault KV-v2
client that reads every Gateway secret as a field of one KV-v2 secret
at `{mount}/data/wylde/gateway`. Both sides hit Vault's standard KV-v2
read endpoint `/v1/{mount}/data/{path}` directly — Python via `httpx`,
Rust via `reqwest` (its blocking client, since `SecretsProvider::get`
is a sync trait method) — with no `vaultrs`-style dependency. Auth is
token-based for v1 (`VAULT_TOKEN`); AppRole / JWT / Kubernetes auth
methods are deferred. The env-var contract is `VAULT_ADDR`,
`VAULT_TOKEN`, `VAULT_NAMESPACE` (optional — sent as the
`X-Vault-Namespace` header), and `WYLDE_VAULT_KV_MOUNT` (KV-v2 mount,
default `secret`). Reads are cached in-memory for 60 seconds; a
connection error, 5xx, or 401/403 logs a warning and falls through to
the composed file backend, so a Vault outage degrades to the
dev-default `.env` / OS-environ path rather than failing hard. The
selector — `get_secrets()` (Python) / `build_provider` (Rust) — now
routes `WYLDE_GATEWAY_SECRETS_PROVIDER=vault` to the real backend on
both implementations; a misconfigured `VAULT_*` contract still falls
through to the file backend with a warning. Secrets are not a Gateway
HTTP surface, so the parity suite is unaffected.

The Python Gateway remains the production default until the rest of the
queue lands. Each entry below lists the target Rust path, what Python
source it mirrors, a one-line scope description, and which crates it
depends on that aren't already in `wylde-gateway/Cargo.toml`.

## Remaining queue

### Tests — parity / byte-equivalence

| Path | Notes |
|------|-------|
| `tests/parity/` | Side-by-side Python+Rust process tests that hit the same HTTP route on both implementations and diff the bytes. Lifecycle-side gating (the strangler-fig env flag flip) needs this before production cutover. |

## Other deferred bits

* **Lifespan config import** — `Gateway/app.py::_apply_config_files`.
  Rust can do the equivalent (read `Core/Config/*.yaml` into
  `std::env`) but the launcher already handles this in production so
  the gain is dev-only. Defer.
* **Async loop** — `Gateway/async_loop.py`. Wave-1 tokio runtime
  already provides what Python's loop gave Starlette (one shared
  reactor); the Rust port is "no module needed". Documented here so
  it's explicitly closed, not forgotten.

## Cutover note

`WYLDE_WYLDE_GATEWAY_IMPL=rust` already routes to the Rust binary via
W3 strangler-fig. Wave 2e fills out every subsystem on the Rust side
(egress + secrets, plus the extensions + tool-registry routes); wave 2i
wired the extensions route to the live `extension-bridge` pipe service;
wave 2j landed the MCP server surface on both implementations; wave 2k
landed the Vault secrets backend. The remaining queue is the parity
tests. The Python Gateway stays the production default until they land
and share the audit log byte-for-byte with the Rust implementation.
