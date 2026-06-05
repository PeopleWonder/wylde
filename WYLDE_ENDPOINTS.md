# Wylde — Endpoints & Pathways

Audience: future-you, or a contributor coming in cold. This is every place a request can enter Wylde and every channel components use to talk to each other. Every entry has a `file:line` citation.

> **State markers:** **[live]** = wired and reachable, **[planned]** = future, **[uncertain]** = code present but not verified end-to-end. The earlier **[delete]** items (dead orchestrator routes, fletch-web shim, retired services router) have been executed — they're gone now. Last refresh: 2026-05-10 post-execution.
>
> **Implementation note (current):** the route *surface* and pipe *contracts* below are still accurate, but the canonical Gateway is now the Rust crate `wylde-gateway` (Axum, HTTP on `127.0.0.1:8005` + the `\\.\pipe\wylde-gateway` pipe), a superset of the routes documented here; the legacy Python `Gateway/` package has been removed. Lifecycle, voice, and the vram-broker likewise default to their Rust implementations. The `Gateway/app.py:140`-style `file:line` citations below point at the historical Python implementation and are retained for contract reference — read them as "this contract", not "this file still exists".

## Architecture in one sentence

Gateway exists for trust-boundary HTTP only — outbound internet, inbound mobile (over VPN), and inbound MCP. Everything else is pipes. The native gpui GUI (`wylde-gui`) talks pipes directly from Rust, with an in-process `wylde_harness::HarnessApi` short-circuit for unary harness verbs. The chat-turn driver lives in the harness behind a new pipe. The legacy fletch-web HTTP shim is being removed.

### The three legitimate Gateway scopes

| Scope | Direction | Examples |
|---|---|---|
| **Outbound** | Wylde components → public internet | Webcrawler scrape, Ollama model pull, future weather/search |
| **Mobile-future** | Mobile app → VPN tunnel → Gateway → pipes | Mobile mirrors desktop GUI; not built yet but Gateway design anticipates it |
| **MCP** | External MCP client → Gateway → harness/tool | Claude / external clients calling Wylde's tool catalog |

Out of scope for Gateway: GUI ↔ backend, chat-turn orchestration, service-to-service, anything purely local. Those are pipes or in-process calls.

### The pipes

| Pipe | Status | Owner |
|---|---|---|
| `\\.\pipe\wylde-lifecycle` | **[live]** | `Core/Lifecycle/daemon.py:117` — service.list/.start/.stop/.wake/.health/.shutdown_all |
| `\\.\pipe\wylde-gateway` | **[live]** | `Gateway/pipe.py:154` — same actions Gateway HTTP serves, in-process callers use this |
| `\\.\pipe\wylde-memgraph` | **[live]** | `Core/Memgraph/run.py` + `graph_service.py` — Neo4j-backed graph CRUD; spawned as a subprocess of the Lifecycle daemon (Phase 2c) |
| `\\.\pipe\wylde-harness` | **[live]** | `Core/harness/pipe.py:start()`, started by the Lifecycle daemon at boot — five chat.* actions plus `tools.list`, `tools.run`, `models.list`, `models.get_profile`, `models.show`, `models.{delete,unload,set_active,set_default,get_default}`, RAG/workspace/memory CRUD, reflection |
| `\\.\pipe\wylde-voice` | **[live]** | `Voice/pipe.py:start()`, spawned as a subprocess by the Lifecycle daemon (Phase 2e) — ten `voice.*` actions: `toggle`, `start_session`, `end_session`, `set_mode`, `get_mode`, `set_active_conversation`, `get_status`, `check_wake_word_model`, `pull_wake_word_model`, `subscribe_status` |
| `\\.\pipe\wylde-device-gate` | **[live]** | `device_gate/pipe.py:start()`, spawned as a subprocess by the Lifecycle daemon (Phase 2f) — ten `device_gate.*` actions: `list_devices`, `start_pairing`, `cancel_pairing`, `get_pairing_status`, `complete_pairing`, `verify`, `set_tier`, `rotate_token`, `revoke`, `consume_pending_events` |
| `\\.\pipe\wylde-vpn` | **[opt-in]** | `VPN/api.py` — WyldeLink management surface (status, peers, STUN, pairing, push). Default-off per principle #15; users opt in when remoting from outside the LAN. Hoisted to top-level in Phase 12 (was `device_gate/VPN/` before). |

Everything else (`wylde-orchestrator`, `wylde-rag`, `wylde-trainer`, `wylde-launcher`, `tool-registry`, `n8n-service`, `webcrawler-service`, `wylde-caption`, `fletch-web`) is **dead** in the new tree — no server stands them up. Routes/clients still referencing them need to be repointed (to in-process calls, the new `wylde-harness` pipe, or `wylde-lifecycle`/`wylde-gateway`) or deleted.

---

## 1. External ingress — Gateway HTTP

The Gateway is FastAPI, factory in `Gateway/app.py:140`. Lifespan applies `Core/Config/*.yaml` → env, warms secrets, starts the async loop, reloads egress destinations from manifests, starts the named-pipe transport (`Gateway/app.py:115`). Process entry: `Gateway/run.py:26`. Aggregator: `Gateway/routes/__init__.py:49`.

**Auth:** exactly two tiers per principle #16 (`Gateway/auth/__init__.py`). `require_public` (`:95`) for health probes; `require_local` (`:100`) for everything else, with the local CIDR allowlist covering loopback, the legacy bridge range, and `100.64.0.0/10` for WyldeLink CGNAT-tunneled mobile peers.

### KEEP-OUTBOUND — `/api/egress/*` [live]

The single channel internal components use to reach the public internet (principle #11). All routes in `Gateway/routes/egress.py`.

| Method | Path | File:line | Notes |
|---|---|---|---|
| GET | `/api/egress/destinations` | `routes/egress.py:76` | lists allowlist keys + env-var names |
| POST | `/api/egress/kill` | `routes/egress.py:82` | toggles / reads kill switch |
| POST | `/api/egress/forward` | `routes/egress.py:88` | unary outbound, body = `ForwardRequest` (`:55`) |
| POST | `/api/egress/stream` | `routes/egress.py:102` | NDJSON / chunked passthrough (HTTP-only — pipe transport is unary by design) |

Outbound is also implicit in `/api/models/pull` (Ollama pulls a model from the internet on Wylde's behalf) — see Mobile-future / Models.

### KEEP-MOBILE-FUTURE — most of the Phase-9 surface

The mobile app will mirror the desktop GUI; the desktop GUI uses pipes directly, but the mobile app crosses the trust boundary at the WyldeLink VPN tunnel and presents to Gateway as a CGNAT caller. These routes pre-exist exactly to translate that ingress to pipe calls. Several still target dead pipes — flagged inline so they get repointed when the matching server stands up.

#### Health [live] — public, MCP-relevant too

| Method | Path | Handler | Notes |
|---|---|---|---|
| GET | `/health` | `routes/health.py:30` | `{"ok": true}` — keep |
| GET | `/live` | `routes/health.py:37` | liveness — keep |
| GET | `/ready` | `routes/health.py:42` | 503 if egress kill on or secrets unhealthy — keep |

#### Tool catalog [live] — also KEEP-MCP

`/api/tools` is read-only catalog inspection. **GUI does NOT call this** (uses pipe / in-process); mobile and MCP do.

| Method | Path | Handler | Downstream |
|---|---|---|---|
| GET | `/api/tools` | `routes/tool_registry.py:32` | in-process `Core/harness/tooling/tool_registry` |
| GET | `/api/tools/{tool_id}` | `routes/tool_registry.py:38` | same |

#### Extensions [live] — Phase 7 browser-extension contract

Doesn't fit the three categories cleanly (not over VPN, not MCP, not outbound), but the trust shape is identical to mobile (external client, `require_local`). Recommend keep.

| Method | Path | Handler | Downstream |
|---|---|---|---|
| GET / POST | `/extensions/{name}/{endpoint}` | `routes/extensions.py:47,56` | `services.extensions.dispatch()` → `Extensions/extension_bridge/dispatcher` |

#### Chat

`POST /api/chat/run_turn` is the mobile-bound chat-turn endpoint. It calls `require_device` to verify the Bearer token and forwards `auth.tier` into the harness `chat.run_turn` action as `device_tier`. The chat-turn loop tier-gates tools mid-loop (read_only blocks all, tool_use blocks `requires_confirmation:true` tools, destructive_tool_access runs everything). Response carries an `X-Wylde-Events` header when device_gate has events queued for the device (see below).

`POST /api/chat` and `POST /api/chat/generate` remain naked Ollama proxies for non-tool-loop use. They use `require_local` only.

| Method | Path | Auth | Notes |
|---|---|---|---|
| POST | `/api/chat/run_turn` | `require_device` | Full harness chat turn; tier-gated tool dispatch; `X-Wylde-Events` on response |
| POST | `/api/chat` | `require_local` | Naked Ollama NDJSON→SSE proxy (no tools) |
| POST | `/api/chat/generate` | `require_local` | Naked Ollama generate proxy |

**`X-Wylde-Events` response header (device_gate event delivery):**

After every successful `require_device` verify, Gateway drains the device's pending-event queue from device_gate (see `device_gate.consume_pending_events`). Events ride out as a JSON array on the `X-Wylde-Events` response header — same channel for every route the mobile hits. Mobile reads the header and dispatches by `type`:

* `token_rotated` → mobile replaces its stored token with `new_token` from the event payload, continues the session.
* `revoked` → mobile clears its token, prompts the user to re-pair.
* `tier_changed` → mobile refreshes the cached tier; some UI may re-render.

Header is omitted when no events are pending. When present, the JSON is compact (no whitespace) so the header stays a single line. Example:

```
X-Wylde-Events: [{"type":"token_rotated","device_id":"dev_abc","new_token":"…","at":1700000000.0}]
```

Implementation: `Gateway/auth/device.py:require_device` → `Gateway/events.stash_pending_events` (request-time drain) → route handler calls `Gateway/events.attach_pending_events(request, response)` on its way out.

#### Devices [live route, dead pipe]

LAN device-gate. Targets `device-gate` pipe; current `device_gate/device_gate.py` is in-process, no pipe stood up. **Repoint to in-process.**

`/api/devices/pending` `:35`, `/approve/{ip}` `:41`, `/deny/{ip}` `:49`, `/approved` `:55`, `/{ip}` PATCH `:61`, DELETE `:69` — all pipe `device-gate`.

#### Images [live] — backed by external image-gen at `:8014`

| Method | Path | Backend | File:line |
|---|---|---|---|
| POST | `/api/images/generate` | `http://127.0.0.1:8014/generate` | `routes/images.py:32` |
| GET | `/api/images/library` | filesystem `data/images/*.png` | `:44` |
| GET | `/api/images/library/{img_id}` | filesystem | `:67` |
| DELETE | `/api/images/library/{img_id}` | filesystem | `:98` |
| GET | `/api/images/models` | image-gen `/list_models` | `:115` |
| GET | `/api/images/loras` | image-gen `/list_loras` | `:121` |

#### Link / VPN [live] — proxies VPN HTTP at `:8020`

The mobile-bridge surface. Once a peer has a tunnel they hit Gateway as `100.64/10`; this router lets them self-manage.

| Method | Path | File:line |
|---|---|---|
| GET | `/api/link/status`, `/peers`, `/stun` | `routes/link.py:37,45,60` |
| POST | `/api/link/peers/remove`, `/pair` | `:51,68` |
| GET | `/api/link/qr/{token}` (SVG passthrough) | `:77` |

#### Models [live]

Live (Ollama at `127.0.0.1:11434`):

| Method | Path | File:line |
|---|---|---|
| GET | `/api/models` | `routes/models.py:33` |
| GET | `/api/models/running` | `:39` |
| POST | `/api/models/pull` (NDJSON→SSE; **also outbound — Ollama fetches from internet**) | `:45` |
| POST | `/api/models/generate` | `:59` |
| DELETE | `/api/models/{name:path}` | `:69` |

The four `/api/models/registry*` sub-routes that proxied to the dead
`wylde-orchestrator` pipe have been removed. The unified registry lives
in-process at `Core/harness/model_registry/` and will be reachable via
the future `\\.\pipe\wylde-harness` actions (`models.list`,
`models.get_profile`, `models.discovery.*`).

#### Push [removed]

The `/api/push/{subscribe,unsubscribe,pending}` routes were removed in the
Bucket-A IPC cleanup. They proxied a Flask-style pipe surface on
`wylde-vpn` whose `peers.push` handlers were never wired (the VPN Python
store was deleted), so they only ever 404'd in production. No Gateway HTTP
push surface today; re-introduce one only when a live peer-push store
exists.

#### RAG [live route, dead pipe] — also KEEP-MCP

`/api/rag/query` `:27`, `/ingest` `:40`, `/collections` `:53` all target dead `wylde-rag`. **Repoint to in-process `Core/harness/memory/rag.py`.**

#### Settings [live]

`/api/settings/ollama` GET `:69`, PUT `:74` are local-file-backed (live). The `/api/settings/hardware` proxies were retired with the system-metrics surface; a future hardware-detection sub-router can land back here once a service owns the responsibility.

#### Training [live route, dead pipe]

All endpoints target `wylde-trainer` pipe (no server in new tree). `Trainer/Caption/` exists; `Trainer/` itself is a folder shell. Build the pipe server or in-process equivalent.

The `/api/training/register` endpoint (which targeted the dead
`wylde-orchestrator`) has been removed. Registration of trained
checkpoints now happens in-process via `Core/harness/model_registry/`.

#### Voice [live] — `wylde-voice`

`Voice/pipe.py` serves ten `voice.*` actions on `\\.\pipe\wylde-voice`. The Lifecycle daemon spawns Voice as a subprocess at Phase 2e (parallel to Memgraph). The gpui GUI calls the `voice.*` actions over the pipe directly from Rust via `wylde-gui-pipe` (`Core/GUI/Frontend/Pipe/`); `voice.subscribe_status` rides the streaming `stream_call` path, the rest are unary `call`s.

Voice owns audio I/O and the orchestration loop (capture → STT → harness chat → TTS → play). Since the Phase-11.E voice cutover the STT (Whisper) and TTS (Kokoro) engines run **in-process inside `wylde-voice`** itself, reached via the `voice.transcribe` / `voice.synthesize` actions. The old harness `models.transcribe` / `models.synthesize` verbs that used to drive the deleted Python `Voice/` engines were retired in the Bucket-A IPC cleanup.

Active conversation: Voice mirrors whatever the GUI pushes via `voice.set_active_conversation`; cold start falls back to the most-recent conversation in `Core/harness/memory/conversation.list_conversations()`. Voice never creates conversations.

Legacy `/api/voice/{command,speak,transcribe,health}` routes were dead in the new tree and have been removed; no Gateway HTTP surface for Voice today.

#### Devices [live] — `wylde-device-gate`

`device_gate/pipe.py` serves ten `device_gate.*` actions on `\\.\pipe\wylde-device-gate`. The Lifecycle daemon spawns device_gate as a subprocess at Phase 2f. Three permission tiers — `read_only` (default at pairing), `tool_use`, `destructive_tool_access` — gate every external request.

**Pairing flow** (code-based, replaces the legacy nginx auth_request / IP-allowlist model):

1. Desktop GUI calls `device_gate.start_pairing` → returns 6-digit code, 5-min TTL.
2. User enters `{code, username, password, device_metadata}` in the mobile app.
3. Mobile sends to Gateway `POST /api/link/pair`, which calls `device_gate.complete_pairing` over the pipe.
4. On success, device_gate mints a UUID4 token at the `read_only` tier and returns `{device_id, token, tier}`. Pairing mode auto-OFF.
5. Mobile stores the token; subsequent requests carry `Authorization: Bearer <token>`.

**Per-request auth** (`Gateway/auth/device.py`): every protected route uses `Depends(require_device)` which extracts the Bearer token, calls `device_gate.verify(token)`, attaches `{device_id, tier}` to `request.state.device_auth`, and returns 401 on miss / invalid. `Depends(require_tier(min_tier))` and `Depends(require_tool_access(tool_id))` chain on top — the latter reads the tool manifest's `requires_confirmation` flag as the "destructive" signal.

**Token rotation**: GUI clicks "Rotate token" → `device_gate.rotate_token(device_id)` → new UUID4 minted, old token invalidated. If the device is currently active (verify() touched within the last minute), device_gate queues a `token_rotated` event in its per-device pending-events queue. Gateway drains the queue via `device_gate.consume_pending_events(device_id)` after each successful verify and forwards the new token over the active connection so the mobile updates without a re-pair.

**Revocation**: `device_gate.revoke(device_id)` removes the record + invalidates the token + queues a `revoked` event. Mobile sees the next request return 401, clears its token, and prompts the user to re-pair.

GUI surface: the gpui Devices panel (`Core/GUI/Frontend/Panels/Devices/`, crate `wylde-panel-devices`) — pairing modal with countdown + scannable QR code, device cards with tier segmented control (all three tiers visible per spec), rotate-token + revoke buttons. Its `src/ipc.rs` calls the `device_gate.*` actions over the pipe via `wylde-gui-pipe`: `device_gate.list_devices`, `device_gate.start_pairing`, `device_gate.cancel_pairing`, `device_gate.get_pairing_status`, `device_gate.set_tier`, `device_gate.rotate_token`, `device_gate.revoke`.

#### Workflows [live n8n bridge only]

The 23 dead-orchestrator endpoints (catalog/run/compose/stream/resume/
gate/budget/lint/versioning/diff/ab-test/traces/optimizer/autotuner)
were removed. What remains is the n8n bridge:

| Method | Path | Backend | File:line |
|---|---|---|---|
| GET | `/api/workflows/n8n` | n8n `/api/v1/workflows` | `routes/workflows.py:49` |
| POST | `/api/workflows/n8n` | n8n `/api/v1/workflows` | `:57` |
| POST | `/api/workflows/n8n/{wf}/execute` | n8n `/api/v1/workflows/{wf}/run` | `:68` |
| DELETE | `/api/workflows/n8n/{wf}` | n8n `/api/v1/workflows/{wf}` | `:79` |

#### Services — gone

`Gateway/routes/services.py` was deleted. The `/api/services/*` family
(list/health/start/stop/wake) was already retired in Phase 9 — the
active surface now lives at `\\.\pipe\wylde-lifecycle`. The file was
sitting around as an inert stub; it's gone.

### KEEP-MCP — `/mcp/*` [planned, not implemented]

**Not built.** Only artefact is a comment slot at `Gateway/app.py:26-27`. `Gateway/routes/mcp.py` does not exist. No `FastMCP`/`mcp.server` imports anywhere outside `_legacy/.../venv/`.

When implemented, the MCP surface is the third trust-boundary scope. External MCP clients (Claude, etc.) hit Gateway → harness via the future `wylde-harness` pipe. Likely transports:
- **stdio** — for native MCP clients spawning Wylde as a subprocess; not HTTP, but the dispatcher logic still belongs here
- **streamable HTTP** — `POST /mcp/v1/messages` (or whatever the protocol settles on)

Tool exposure: walks `Core/harness/tooling/tools/` filesystem registry, exports each as MCP `tools/list` + `tools/call`. Same catalog the harness uses internally.

---

## 2. Named pipes — `\\.\pipe\wylde-*`

The shared transport lives in `Core/shared/ipc.py`: msgpack framing, `register_action()` (`:235`) for the `/__action__` envelope, `serve_forever_background()` (`:380`) to start a server in a daemon thread. The action-style envelope is `{ method: "/__action__", body: { action, payload } }`; non-action envelopes fall through to a Flask test client (legacy compatibility).

### Live pipe servers

| Pipe | Server file:line | Surface |
|---|---|---|
| `\\.\pipe\wylde-lifecycle` | `Core/Lifecycle/daemon.py:117` | Actions in `Core/Lifecycle/control.py:443`: `service.list`, `service.health`, `service.start`, `service.stop`, `service.wake`, **`service.shutdown_all`** (absorbs the role of fletch-web's old `/shutdown`; calls `Core/Lifecycle/shutdown.py:30`). Stub Flask `/health` at `daemon.py:56`. |
| `\\.\pipe\wylde-gateway` | `Gateway/pipe.py:154`, started in `Gateway/app.py:118` | Actions in `Gateway/pipe.py:134-150`: `egress.forward`, `egress.kill_switch`, `egress.destinations`, `extensions.dispatch`, `tools.list`, `tools.get`. Plus full HTTP routes via Flask test-client fallback (Gateway exposes both fronts identically). |
| `\\.\pipe\wylde-memgraph` | `Core/Memgraph/run.py` + `graph_service.py` | Documented surface: `GET /health`, `POST /ensure_schema`, `POST /upsert`, `POST /delete_path`, `POST /traverse`, `GET /stats`. Backed by Neo4j on `bolt://127.0.0.1:7687`. **Spawned as a subprocess** by the Lifecycle daemon at boot (Phase 2c — `Core/Lifecycle/daemon.py:_start_memgraph`); the daemon's signal handler calls `_stop_memgraph` to take Neo4j down cleanly on shutdown. |
| `\\.\pipe\wylde-harness` | `Core/harness/pipe.py:start()` (started in-process by `Core/Lifecycle/daemon.py` at boot — Phase 2b) | Nine actions wrapping the chat-turn driver in `Core/harness/turn.py` — see §4 for the full surface. |

### `\\.\pipe\wylde-harness` — chat-turn driver [live]

Started in-process by `Core/Lifecycle/daemon.py` at boot (the
"Phase 2b" block right after the lifecycle pipe comes up). Server lives
in `Core/harness/pipe.py`; driver in `Core/harness/turn.py`; standalone
foreground entry at `Core/harness/server.py` (run via
`py -3 -m Core.harness.server`).

Nine actions: five `chat.*` for the per-turn loop, plus `tools.*` /
`models.*` for the catalog and registry surfaces the GUI used to hit
through dead orchestrator/tool-registry pipes.

**Chat:**

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `chat.start_turn` | `{ user_message, conversation_id, model?, turn_id?, workspace_id?, modality?, device_tier? }` | `{ turn_id, conversation_id }` | Kicks off a turn server-side; returns immediately. The turn runs on a daemon thread until completion or cancel. ``device_tier`` is one of `read_only` / `tool_use` / `destructive_tool_access` (default `tool_use`); the chat-turn loop blocks tool calls the tier isn't allowed to make and emits `tool_error` events with reason codes (`tier_read_only`, `tier_tool_use_blocked_destructive`). |
| `chat.run_turn` | `{ user_message, conversation_id, model?, turn_id?, workspace_id?, modality?, device_tier?, timeout? }` | `{ turn_id, conversation_id, final_message, tool_calls_summary, aborted, abort_reason }` | Blocks until the turn is done. Used by tests, future MCP, mobile single-shot — anyone who doesn't need live UI. Same tier-gating as `chat.start_turn`. |
| `chat.cancel` | `{ turn_id }` | `{ ok, turn_id }` | Sets the cancel flag; the driver checks between iterations and emits `turn_aborted` on the user-facing stream. |
| `chat.stream_turn` | `{ turn_id, cursor?, max_wait_ms? }` | `{ events, next_cursor, done }` | Long-poll cursor for the **user-facing** event stream. Block up to `max_wait_ms` (default 5000, capped at 25000) until ≥ 1 event past `cursor` or `done`. Consumers loop until `done`. |
| `chat.stream_tools` | `{ turn_id, cursor?, max_wait_ms? }` | `{ events, next_cursor, done }` | Same shape, **tool-activity** stream. |

**Tools:**

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `tools.list` | `null` | `{ tools, count }` | Live tool catalog from `Core/harness/tooling/tool_registry/`. List of entry dicts (not the keyed dict the registry stores internally). |
| `tools.run` | `{ name, args, confirm? }` | runner envelope (`{ok, data}` / `{ok: False, error}` / `{ok: False, confirmation_required}`) | Run one tool by id, bypassing the chat loop. The Phase 8 confirmation gate (Wylde Design Principle #12) still applies — pass `confirm: true` to bypass for already-approved calls. |

**Models:**

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `models.list` | `{ kind? }` | `{ models, count, kind }` | Models known to `Core/harness/model_registry/`. Optional `kind` filter (`llm` / `stt` / `tts` / `embed` / `vision`). |
| `models.get_profile` | `{ name }` | `{ name, profile }` | Routing profile for a model name (backend, backend_model). Mirrors what `backend_routing._lookup_profile` does internally. |

**RAG workspaces** (folder-indexed RAG corpora, MRU 5):

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `rag.workspaces.list` | `null` | `{ workspaces }` | All workspaces in MRU order, full metadata. |
| `rag.workspaces.recent` | `{ limit? }` | `{ workspaces }` | Top-N for the Chat panel's workspace dropdown. Capped at 5 server-side. |
| `rag.workspaces.activate` | `{ path, conversation_id?, full_reindex? }` | workspace record | Activate a folder. Delta-refreshes existing workspaces, full-indexes new ones; if 6th workspace, evicts the oldest (deletes its index folder + workspace memory). Optional `conversation_id` binds the workspace to that conversation in `harness/memory/conversation.py`. |
| `rag.workspaces.reindex` | `{ workspace_id }` | workspace record | Force full rebuild — the GUI's "Reindex" button. |
| `rag.workspaces.status` | `{ workspace_id }` | `{ id, file_count, last_indexed_at, last_activated_at, indexing }` | Indexing snapshot for the dropdown / progress UI. |
| `rag.workspaces.delete` | `{ workspace_id }` | `{ ok, workspace_id }` | Remove from registry + delete index dir + drop workspace memory. |
| `rag.workspaces.set_persona` | `{ workspace_id, text }` | `{ ok, workspace_id }` | Per-workspace prompt fragment. |
| `rag.workspaces.get_persona` | `{ workspace_id }` | `{ workspace_id, persona }` | – |

**Memory: long-term** (Layer 1, global, `Core/harness/memory/long_term.py`):

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `memory.long_term.list` | `{ include_superseded? }` | `{ memories, count }` | Sorted importance desc then recency. |
| `memory.long_term.search` | `{ query, k? }` | `{ hits }` | Hybrid retrieval: vector similarity scaled by importance × `exp(-age_days / decay)`. Skips superseded records by default. |
| `memory.long_term.save` | `{ body, source?, importance?, tags? }` | record | Importance defaults to a length+entity heuristic (capped at 8) when not supplied. |
| `memory.long_term.update` | `{ id, body?, importance?, source? }` | new record | Revision-not-deletion: writes a new record, marks old as `superseded_by` the new id. |
| `memory.long_term.delete` | `{ id }` | `{ ok, id }` | Removes the record AND any predecessors in its supersession chain. |
| `memory.long_term.history` | `{ id }` | `{ id, chain }` | Full forward + backward supersession walk for the Settings UI history view. |

**Memory: workspace** (Layer 2, **durable across MRU eviction**):

Workspace memory lives at `Core/harness/memory/workspace_memories/<slug>/`,
**outside** the file-index folder. MRU eviction deletes only the
index folder; the durable memory survives so a re-activated workspace
starts with its LLM-curated insights ready. Explicit `delete_workspace`
removes both.

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `memory.workspace.list` | `{ workspace_id, include_superseded? }` | `{ memories, count, workspace_id }` | – |
| `memory.workspace.search` | `{ workspace_id, query, k? }` | `{ hits }` | Same scoring as long-term. |
| `memory.workspace.save` | `{ workspace_id, body, importance?, entities?, source? }` | record | Optional `entities[]` writes Memgraph edges (best-effort — graph layer down → save still succeeds). |
| `memory.workspace.update` | `{ workspace_id, id, body?, importance?, entities? }` | new record | Same revision-not-deletion shape as long-term. |
| `memory.workspace.delete` | `{ workspace_id, id }` | `{ ok, workspace_id, id }` | – |
| `memory.workspace.curate` | `{ workspace_id }` | `CurationResult` | LLM-driven sweep of the workspace's memories in batches. The model returns `keep` / `supersede(reason)` / `merge(into, new_body)` verdicts; the curator applies them. Stale entries get a tombstone supersession (audit trail intact, hidden from default retrieval). **Returns skipped** when called via the pipe — the scheduler runs the real path with an injected chat_fn. |

**Memory: short-term** (Layer 3, per-conversation, persisted on the conversation record):

Lives on the conversation's JSON record on disk — same place chat
history lives. Survives normal app close + reopen. **Dies** when
`delete_conversation` is called (intentional — the design's
"per-conversation" scope).

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `memory.short_term.get` | `{ conversation_id }` | `{ working_memory, conversation_id }` | Tool calls / files / decisions accumulated this conversation. |
| `memory.short_term.append` | `{ conversation_id, entry }` | `{ conversation_id, working_memory }` | Driver auto-appends `{kind: "tool", data: {...}}` on every tool dispatch; callers can also append manually. |
| `memory.short_term.clear` | `{ conversation_id }` | `{ cleared, conversation_id }` | – |

**Memory: reflection / consolidation:**

| Action | Payload | Returns | Notes |
|---|---|---|---|
| `memory.reflect` | `{ scope }` | `ReflectionResult` | `scope` is `"long_term"` or `"workspace:<id>"` (or `"conversation:<id>"`, currently a no-op — working memory has no promotion target). **Returns skipped** when called via the pipe — the scheduler runs the real path with an injected chat_fn. |

**Reflection + curation scheduler** (`Core/harness/memory/scheduler.py`):

The Lifecycle daemon spawns a `MemoryScheduler` thread in Phase 2d that
polls every 60 s and fires `reflect()` and `curate()` at separate
cadences:

| Scope | Default cadence | Trigger |
|---|---|---|
| Conversation reflection | conversation idle ≥ 10 min, dispatched once per idle period | `_tick_conversations` |
| Workspace reflection | every 6 h per workspace in MRU 5 | `_tick_workspaces_reflect` |
| Workspace curation | every 24 h per workspace | `_tick_workspaces_curate` |
| Long-term reflection | every 24 h | `_tick_long_term` |

State persists at `$DATA_DIR/scheduler_state.json` so a daemon restart
doesn't replay the entire backlog. Cadences are env-overridable
(`WYLDE_SCHED_*_S`). The scheduler uses `Core.harness.memory.scheduler.default_chat_fn()`
which wraps `backend.default_router()` — when no router is reachable
(no Ollama configured) the scheduler logs and stays parked in
"skipped-only" mode; direct Python callers can still drive
reflection / curation explicitly.

#### Event types (wire-level disjoint)

`chat.stream_turn` ever returns only:
- `token` — `{ turn_id, text }` — assistant text chunk
- `thinking` — `{ turn_id, text }` — thinking-tokens (only if backend emits them)
- `turn_complete` — `{ turn_id, final_message }`
- `turn_aborted` — `{ turn_id, reason, error? }` (`reason` is `"cancelled"` | `"error"` | `"tool_loop_limit"` | `"pipe_error"`)

`chat.stream_tools` ever returns only:
- `tool_dispatched` — `{ turn_id, call_id, name, args }`
- `tool_result` — `{ turn_id, call_id, name, output, duration_ms }`
- `tool_error` — `{ turn_id, call_id, name, error, duration_ms }`

Tool events MUST NOT leak into `chat.stream_turn`; token events MUST NOT leak into `chat.stream_tools`. The split is enforced by `_emit_turn` vs `_emit_tool` in `turn.py:131-145` writing to two distinct event lists, each drained by its own action handler. There is no consumer-side filtering.

#### Driver loop (`Core/harness/turn.py:_drive_turn_inner`)

1. Build messages: system prompt (with current tool catalog) + user message.
2. Loop up to `_MAX_TOOL_LOOPS` (default 8):
   - Call `chat_fn(messages, tools, model)` → `ChatStep(text, thinking, tool_calls)`.
   - Emit `thinking` if any.
   - If no tool calls: emit `token` (full text) + `turn_complete`, return.
   - Emit `token` for any bridge text from the assistant.
   - For each tool call: emit `tool_dispatched`; run via `tool_runner.run_tool()`; emit `tool_result` or `tool_error`; append a `tool` message to the LLM history.
3. If the loop hits the cap without a final response, emit `turn_aborted` with `reason="tool_loop_limit"`.

Token streaming today is degraded — one `token` event per LLM round trip, carrying the full text. Real per-token streaming is a future upgrade that wires `harness/backend/streaming.stream_chat` into the loop.

Conversation history is stubbed (a single user message). When `harness/memory/conversation.py` is wired (Open Items F1, decision locked), the driver will prepend prior turns before the new message.

#### Callers

- **gpui GUI** — the Chat panel (`Core/GUI/Frontend/Panels/Chat/`, crate `wylde-panel-chat`). Its `src/ipc.rs` drives the turn via `wylde-gui-pipe`: `start_turn` / `cancel_turn` (unary `chat.start_turn` / `chat.cancel`) and `stream_turn` / `stream_tools` (the `wylde_gui_pipe::stream_call` `PipeStream` loop, abort-on-drop). Streaming verbs go over the wire; unary harness verbs can take the in-process `HarnessApi` short-circuit. The panel spawns each subscription on a gpui task and `cx.notify()`s the View per chunk.
- **Chat token feed** — the Chat panel's transcript bubbles consume the `chat.stream_turn` channel (`token` / `thinking` / `turn_complete` / `turn_aborted`). Token-only consumer; never sees tool events.
- **Tool activity strip** — the Chat panel's tool-activity surface (`chat_panel.rs`) subscribes to the disjoint `chat.stream_tools` channel for the active turn id and renders dispatched / completed / failed tool calls with durations.

#### Migration paths still open

- `Gateway/routes/chat.py` is still a naked Ollama proxy. Repoint to `chat.run_turn` (mobile) or `chat.start_turn` + Gateway-side SSE re-emission of the user-facing stream (mobile streaming).
- The dead-orchestrator surface (agent-turn / linter / model-registry / autotuner / optimizer) the Svelte alpha's `api.js` once targeted has no gpui-side equivalent. The chat-turn surface is migrated; the rest of the orchestrator's surface (workflows, autotuner consolidations, optimizer proposals) doesn't have a harness equivalent and may not get one — most of it lives in N8N now (Workflows surfaces as the n8n iframe panel — see §6).

### Pipes referenced as clients but no server in the new tree [delete or repoint]

These are routes that target pipes whose servers were dissolved during the refactor. Anything calling them today gets `pipe_timeout` or similar. (The Svelte alpha's `api.js` `SVC_*` callers that used to appear in this list went away with the gpui cutover — the gpui client in `Core/GUI/Frontend/Pipe/` only wires the live surfaces, so the remaining stragglers are all Gateway-side Python routes.)

| Pipe | Where used | Verdict |
|---|---|---|
| `\\.\pipe\wylde-orchestrator` | `Gateway/routes/{workflows,models,training}.py` | **delete most callers** (orchestrator is gone); a small subset (registry, agent-turn) repoints to `wylde-harness` |
| `\\.\pipe\wylde-rag` | `routes/rag.py:24` | **repoint** to in-process `harness/memory/rag.py` (or `wylde-harness memory.query`) |
| `\\.\pipe\wylde-trainer` | `routes/training.py:28` | **build the server in `Trainer/`** or repoint to in-process |
| `\\.\pipe\wylde-vpn` | `routes/push.py:33` | **build pipe in VPN** or repoint to in-process `VPN/peers/push.py` |
| ~~`\\.\pipe\wylde-launcher`~~ | n/a | **resolved** — replaced by `wylde-lifecycle`; no GUI caller survives the cutover |
| ~~`\\.\pipe\wylde-voice`~~ | n/a | **resolved** — pipe server stood up, see `Voice/pipe.py:start()` |
| ~~`\\.\pipe\wylde-voice-assistant`~~ | n/a | **resolved** — surface dissolved, replaced by `wylde-voice` actions |
| ~~`\\.\pipe\device-gate`~~ | n/a | **resolved** — pipe server stood up at `\\.\pipe\wylde-device-gate`, see `device_gate/pipe.py:start()` |
| ~~`\\.\pipe\wylde-caption`~~ | n/a | **resolved** — no GUI caller survives the cutover; captioning is in-process `Trainer/Caption/` |
| ~~`\\.\pipe\webcrawler-service`~~ | n/a | **resolved** — Webcrawler is an extension, not a service; goes via extension_bridge |
| ~~`\\.\pipe\tool-registry`~~ | n/a | **resolved** — Gateway `/api/tools` HTTP or in-process import; the gpui GUI reads the catalog via `tools.list` on `wylde-harness` |
| ~~`\\.\pipe\n8n-service`~~ | n/a | **resolved** — replaced by `N8N/client.py` in-process; the gpui GUI opens the n8n editor as an iframe panel (§6) |
| ~~`\\.\pipe\fletch-web`~~ | n/a | **resolved** — HTTP-only shim, now deleted; the gpui GUI talks pipes directly — see §6 / §7 |

---

## 3. Internal IPC / in-process

Direct Python imports between components. No serialization, no transport.

### Lifecycle daemon → launcher → manifests [live]

- `Core/Lifecycle/daemon.py:101` calls `launcher.launch_all()` once at boot; pipe drives start/stop/wake afterward.
- `Core/Lifecycle/launcher.py:39` reads `Core/Network/services.yaml`, builds env overlay (`:122` adds `WYLDE_<NAME>_PORT` / `_ENDPOINT` for every registered service), spawns `entry_point` per manifest.
- `Core/Lifecycle/discovery.py:39` walks top-level `Wylde/` subfolders (excluding `Core/` per `_common.py:35`), auto-generates `manifest.json` for new ones, writes them to `services.yaml`.
- `Core/Lifecycle/shutdown.py:30` `shutdown_all()` — exists, ready to absorb the fletch-web `/shutdown` role.

### Gateway → service-function layer [live]

Dual-front-door: both HTTP routes and pipe actions call into `Gateway/services/*.py` for one implementation per operation.

| Service module | Functions | Used by |
|---|---|---|
| `Gateway/services/egress.py` | `forward`, `forward_sync`, `kill_switch`, `destinations` | `routes/egress.py`, `pipe.py:_h_egress_*` |
| `Gateway/services/extensions.py` | `dispatch` | `routes/extensions.py`, `pipe.py:_h_extensions_dispatch` |
| `Gateway/services/tool_registry.py` | `list_all`, `get_one` | `routes/tool_registry.py`, `pipe.py:_h_tools_*` |

### Harness — composable subpackages, no top-level orchestrator [planned chat-turn driver]

Namespace package — no `Core/harness/__init__.py`. Pieces a future driver will compose:

| Subpackage | Public surface | File:line |
|---|---|---|
| `harness/backend/` | `default_router().chat(messages, model, …) → ChatResult` (single LLM call, no tools) | `backend/backend_routing.py:264` |
| `harness/backend/ollama_client.py` | `stream_chat(...)` for streaming events | – |
| `harness/memory/rag.py` | `build_memory_block(query, …)` | – |
| `harness/memory/memgraph.py` | client over `\\.\pipe\wylde-memgraph` | `memory/memgraph.py:1-35` |
| `harness/memory/conversation.py` | conversation persistence (Open Items F1 — locked design) | – |
| `harness/memory/miss_log.py` | RAG miss tracking | – |
| `harness/tooling/tool_registry/` | walks `tools/<group>/<id>/manifest.json`, returns catalog | – |
| `harness/tooling/tool_runner/` | `run(name, args)` — single-tool dispatch with confirmation gating | – |
| `harness/model_registry/` | `list_models(kind=...)`, `get_profile`, `select_model`, `bench_model` | `model_registry/__init__.py` |
| `harness/prompts/` | system prompt templates | – |
| `harness/_legacy/orchestrator_api/` | **reference only** — has the GUI-routing bug per `NOTE.md`; do not reuse | – |

### Voice — pipe-served (`\\.\pipe\wylde-voice`)

| Component | Surface | File:line |
|---|---|---|
| `Voice/pipe.py` | Ten `voice.*` action handlers; `start()` brings up the pipe via `Core.shared.ipc.serve_forever_background` | `Voice/pipe.py:start` |
| `Voice/run.py` | Process entry; `start_voice()` / `stop_voice()` for embedding hosts; `__main__` is the long-lived service loop the Lifecycle daemon spawns | `Voice/run.py` |
| `Voice/orchestrator.py` | `run_session(state, *, capture, playback, harness, ...)` — capture → STT → chat → TTS → play; `HarnessPipeClient` wraps `\\.\pipe\wylde-harness` | `Voice/orchestrator.py:run_session` |
| `Voice/state.py` | `VoiceState` (thread-safe), `VoiceConfig` (persistent mode/wake-word) | `Voice/state.py` |
| `Voice/audio_io.py` | `AudioCaptureProtocol` / `AudioPlaybackProtocol` + sounddevice-backed defaults + fakes for tests | `Voice/audio_io.py` |
| `Voice/wake_word.py` | `WakeWordDetector` stub, `is_model_installed`, `initiate_pull` | `Voice/wake_word.py` |
| `Voice/transcribe.py`, `Voice/synthesize.py` | Whisper / Kokoro engines (legacy Python `Voice/` tree, deleted at the Phase-11.E voice cutover). They were once called by the harness `models.transcribe` / `models.synthesize` verbs, which were themselves retired in the Bucket-A IPC cleanup; STT/TTS now run in-process in `wylde-voice` via `voice.*`. | – |

Voice is a subprocess of the Lifecycle daemon (Phase 2e in `Core/Lifecycle/daemon.py:_start_voice`). The harness owns STT/TTS engines; Voice talks to them only through the harness pipe.

`VoiceAssistant/` was dissolved — its responsibilities folded into `Voice/`.

### device_gate — pipe-served (`\\.\pipe\wylde-device-gate`)

| Component | Surface | File:line |
|---|---|---|
| `device_gate/pipe.py` | Ten `device_gate.*` action handlers; `start()` brings up the pipe via `Core.shared.ipc.serve_forever_background` | `device_gate/pipe.py:start` |
| `device_gate/run.py` | Process entry; the Lifecycle daemon spawns this with cwd=vault root via `py -3 "device_gate/run.py"` | `device_gate/run.py` |
| `device_gate/core.py` | `DeviceGateService` — pairing, tokens, tier, rotate, revoke, pending-events queue. Pure-Python, no transport | `device_gate/core.py` |
| `device_gate/store.py` | JSON-backed device store; tier constants + rank table; `Device` dataclass | `device_gate/store.py` |
| `device_gate/auth.py` | htpasswd credential check (passlib + crypt + APR1 inline fallback for Windows-without-passlib) | `device_gate/auth.py` |

device_gate is a subprocess of the Lifecycle daemon (Phase 2f in `Core/Lifecycle/daemon.py:_start_device_gate`). Token issuance / verification / tier-gating / rotation / revocation all live behind the `device_gate.*` pipe surface.

### VPN — top-level service (was `device_gate/VPN/`)

VPN was hoisted out of device_gate during Phase 12 — it's now a peer top-level service at `Wylde/VPN/`. See its own `manifest.json` for the egress allowlist; the management API still serves on `\\.\pipe\wylde-vpn` (control plane) when the user opts the service in (default-off per principle #15). Pairing of mobile devices to the WyldeLink mesh is a separate concern from device_gate's per-device permission tiers — they happen in sequence: VPN tunnel up first, then device_gate token check on every request that flows through the tunnel.

---

## 4. Tool catalog — the harness tool-call surface

Filesystem-as-registry under `Core/harness/tooling/tools/<group>/<tool_id>/`. Each tool dir has `manifest.json` + a Python module of the same name. The registry walks the tree and returns a flat list. Smoke-test count: **48 baseline tools**, growing to 51 when Webcrawler is enabled.

| Group | Tool IDs |
|---|---|
| `code` | `execute_python`, `execute_bash` |
| `diff` | `show_diff`, `apply_patch` |
| `fs` | `read_file`, `write_file`, `edit_file`, `list_files` |
| `git` | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_branch`, `git_stash` |
| `meta` | `tool_search`, `graph_query` |
| `n8n` (registry-side empty) | – (handlers under `N8N/tools/`) |
| `ollama` | `preload_model`, `evict_model`, `list_loaded_models`, `auto_evict_lru` |
| `rag` | `rag_ask`, `rag_index`, `rag_reindex`, `rag_feedback`, `rag_misses`, `rag_chunk_usage`, `rag_graph_stats`, `rag_prune` |
| `search` | `code_search`, `code_search_files` |
| `test` | `run_tests`, `run_test_file` |
| `visual` | 15 tools: `screenshot`, `click`, `type_text`, `hotkey`, `mouse_move`, `scroll`, `get_screen_size`, `get_mouse_position`, `navigate`, `browser_screenshot`, `browser_click`, `browser_fill`, `wait_for`, `browser_eval`, `browser_text` |

Open per `WYLDE_PUNCH_LIST.md` TODO table:
- `tools/rag/{rag_feedback,rag_misses,rag_chunk_usage}` are `not_implemented` stubs (mostly unblocked now that `harness/memory/miss_log.py` exists)
- `tools/rag/rag_ask` returns raw search hits — full HyDE → hybrid retrieval → cross-encoder rerank not ported
- `tools/meta/graph_query/graph_query.py:46` — `TODO(graph-aware-rag)`

### N8N service tools — separate registry slice

`N8N/tools/n8n_*/` — service-side tools (Phase 8.5 principle: services host the tools they expose). Each wraps an `N8N/client.py` function.

| Tool dir | Wraps |
|---|---|
| `n8n_list_workflows` | `client.list_workflows()` |
| `n8n_get_workflow` | `client.get_workflow()` |
| `n8n_create_workflow` | `client.create_workflow()` |
| `n8n_edit_workflow` | `client.edit_workflow()` |
| `n8n_execute_workflow` | `client.execute_workflow()` |
| `n8n_delete_workflow` | `client.delete_workflow()` |
| `n8n_get_execution` | `client.get_execution()` |

`N8N/clients/` exists but is empty — leftover folder, delete.

---

## 5. Outbound external calls

| Target | Where | URL / source | Env |
|---|---|---|---|
| Ollama (local daemon) | `Gateway/routes/chat.py:29`, `routes/models.py:29`, `Core/harness/backend/ollama_client.py` | `http://127.0.0.1:11434` (hardcoded; no env var) | – |
| n8n (local) | `Gateway/routes/workflows.py:33`, `N8N/client.py:65` | `http://127.0.0.1:5678` | `WYLDE_N8N_URL`; auth via `WYLDE_N8N_API_KEY` *or* `WYLDE_N8N_EMAIL`+`_PASSWORD`; optional `WYLDE_N8N_BASIC_AUTH_USER`/`_PASSWORD` (`N8N/client.py:65-70`) |
| Image gen (local) | `routes/images.py:26` | `http://127.0.0.1:8014` | – |
| WyldeLink VPN management | `routes/link.py:34` | `http://127.0.0.1:8020` | – |
| Webcrawler-targeted public web | `Extensions/Webcrawler/handler.py:140` (Gateway-first via `Wylde.Gateway.client.forward(dest="web", …)`); `_direct_get` fallback (`:189`) | per call | – (Webcrawler manifest declares `egress: [{key: "web"}]`) |
| Remote LLM backends (vLLM, openai-compat) | `Core/harness/backend/backend_routing.py:35` (uses `Wylde.Gateway.client.forward`) | per Gateway destination key | configured in Gateway destinations registry |

### Egress destinations registry [live]

Loaded from each component's `manifest.json` `egress[]` block at boot (`Gateway/app.py:92-97` → `Gateway/egress/destinations.reload_destinations()`). Webcrawler declares `key: "web"` → `https://` (`Extensions/Webcrawler/manifest.json:9-15`). Other components add destinations the same way.

---

## 6. GUI surface — native gpui desktop app [live]

`Core/GUI/` is a native [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui)
(GPU-rendered, Rust) desktop app. It is its own Cargo workspace
(`Core/GUI/Cargo.toml`), deliberately *not* nested in the backend
`rust/` workspace, so gpui's heavy graphics deps can't ripple the
backend lockfile. The shipped binary is `wylde-gui` (the `Shell` crate
at `Core/GUI/Shell/`, `bin src/main.rs`); the release artefact is
`Core/GUI/target/release/wylde-gui.exe` per `Core/GUI/manifest.json`'s
`entry_point`. This replaced the Tauri 2 + Svelte 5 alpha at the
slice-11 cutover (2026-05-29); the old `src/` (Svelte) and `src-tauri/`
(Tauri) trees were deleted then, along with the npm/Vite toolchain.

There is **no HTTP for local traffic** and no webview-hosted SPA: the
GUI speaks the Wylde named pipes directly from Rust via the
`wylde-gui-pipe` crate (`Core/GUI/Frontend/Pipe/`). For unary harness
verbs it uses the in-process `wylde_harness::HarnessApi` short-circuit
(Phase 12.1, `try_dispatch_harness` in `Frontend/Pipe/src/lib.rs`),
bypassing the IPC hop entirely; the harness binary still serves its
named pipe for MCP/CLI clients.

### IPC surface (`wylde-gui-pipe`, `Core/GUI/Frontend/Pipe/`)

| Function | Backing | File |
|---|---|---|
| `call(service, http_verb, path, body)` | generic unary pipe client (msgpack `/__action__` envelope) | `Pipe/src/lib.rs` |
| `try_dispatch_harness(api, verb, payload)` | in-process `HarnessApi` short-circuit for `chat.*` (non-streaming) / `tools.*` / `memory.long_term.*` / `memory.workspaces.*`; falls through to the wire for unknown verbs | `Pipe/src/lib.rs` |
| `stream_call(service, action, payload) -> PipeStream` | streaming verbs (`chat.stream_turn`, `chat.stream_tools`, `consent.stream_pending`); `ChunkFrame` loop, **abort-on-drop** is the cancel signal | `Pipe/src/lib.rs` |
| `lifecycle_action(action, payload)` / `service_health(name)` | `service.*` actions on `\\.\pipe\wylde-lifecycle` | `Pipe/src/lib.rs` |
| `list_wylde_pipes()` | enumerates `\\.\pipe\wylde-*` | `Pipe/src/lib.rs` |
| `install_runtime(handle)` / `install_nav_sender` / `request_nav` | tokio-handle bridge (gpui dispatcher threads have no tokio runtime) + cross-panel nav bus | `Pipe/src/lib.rs`, `Pipe/src/nav_bus.rs` |

The envelope `caller` field is still `"fletch-gui"` (`CALLER_NAME` in
`Pipe/src/lib.rs`) so log greps stay stable across the cutover overlap;
it flips to `wylde-gui` in a later cleanup commit.

### Panels — gpui Views, not Svelte pages [live]

Panels are gpui `View` crates, one per panel, under
`Core/GUI/Frontend/Panels/` — eleven first-party panels
(`Settings`, `Workspaces`, `Tools`, `Memory`, `Chat`, `Models`,
`Dashboard`, `Devices`, `RemoteAccess`, `Images`, `Training`), each a
workspace member. Each ships its own `src/ipc.rs` (per-panel pipe-call
helpers) and a `manifest.json` whose `source.kind` is `gpui_view` with a
`factory` that resolves the View (enforced by `wylde_check` rule
`first_party_manifest_must_be_gpui_view`).

The panel registry (`wylde-panel-registry`,
`Core/GUI/Manifest/Extension_handlers/`) aggregates first-party manifests
at build time and overlays extension-contributed panels at runtime via
`extensions.list_panels`; the unified list is exposed through
`gui.list_tabs`. `main.rs` builds the registry
(`install_panel_registry`) before the gpui event loop starts; the Shell's
sidebar (`Shell/src/sidebar.rs` + `nav.rs`) renders one row per registry
entry and the slot (`Shell/src/slot.rs`) mounts the selected panel's View.

### Extension iframe panels [live]

Extensions contribute panels via the `ui_panels` manifest field (slice
12.7). Those are `iframe`-kind panels hosted in a `wry`-backed WebView
child window — there is no Svelte iframe and no embedded SPA. The host
lives in `wylde-webview` (`Core/GUI/Frontend/Extension_handlers/WebView/`,
`IframeHost`): the slot HEAD/GET-probes the panel URL with
`probe_url` first, mounts the WebView as a native child of the gpui
Window's HWND on a healthy probe, and otherwise renders the
`ServiceUnavailable` stub. Iframe panels are loopback-validated. The n8n
editor is the canonical first-class iframe panel — see §9.

### Shell lifecycle / shutdown [live]

The tray (via the `tray-icon` crate, replacing `tauri::tray`) and the
window-close (X) path both run the same manifest-driven graceful
shutdown (`shutdown_order` field in each service's `manifest.json`).
`Shell/src/shutdown.rs::run_graceful_shutdown` dispatches
`lifecycle.shutdown_all` on `\\.\pipe\wylde-lifecycle`, waits up to 10 s
for the daemon-managed services to exit, then hard-kills by image name
if the pipe was unreachable or the drain timed out. `main.rs` drains the
tray's `Quit` → graceful shutdown → `cx.quit()`; `ShowWindow` raises the
window. `wylde_check` enforces the GUI architecture via rules including
`no_cross_panel_imports`, `no_legacy_gui_imports_in_panels`,
`webview_only_in_extension_handlers`, `panel_crate_must_be_workspace_member`,
and `stream_call_must_handle_cancel`.

---

## 7. fletch-web — removed

`Core/GUI/web/` is gone. The localhost HTTP shim is no longer in the
path; the GUI talks pipes directly. (This removal predates the gpui
cutover — fletch-web was already gone on the Tauri alpha; the gpui
client in `Core/GUI/Frontend/Pipe/` carries the same pipe-direct model
forward.)

What was removed: `fletch_web.py` (Flask app + 16 prefix proxies),
`serve.py`, `startup.py`, `start_web.bat`, `start_fletch_web.bat`,
`shutdown.py`, vendored copies of `Core/shared/` modules
(`consul_client.py`, `protocol_kv.py`, `manifest.py`, `errors.py`,
`ipc.py`), `requirements.txt`, `config.yaml`, `logs/`.

What replaced its three real responsibilities:

1. **Ollama pull stream.** Now goes through Gateway `/api/models/pull`
   (NDJSON→SSE; live), reached from the gpui Models panel
   (`Core/GUI/Frontend/Panels/Models/`) over the Gateway HTTP egress
   surface; the gpui-era Gateway default is `http://127.0.0.1:8005`.
2. **System-wide shutdown.** Now via the `lifecycle.shutdown_all`
   action on `\\.\pipe\wylde-lifecycle` (`Core/Lifecycle/control.py`).
   The gpui Shell's `Shell/src/shutdown.rs::run_graceful_shutdown`
   dispatches it via `wylde-gui-pipe`, waits up to 10 s for the
   daemon-managed services to exit, then `taskkill`s any survivors
   (including Ollama, which isn't launcher-tracked). Both the tray
   `Quit` item and the window-close (X) path run this — see §6.
3. **Orchestrator SSE streams** (workflow exec, autotuner, agent turn).
   The orchestrator they targeted is gone too; those stream helpers were
   all deleted. The chat-turn UI is now the gpui Chat panel, which
   consumes the harness streaming surface (`chat.stream_turn` /
   `chat.stream_tools`) via `wylde_gui_pipe::stream_call` — see §2 and
   §4 callers.

The gpui binary's bundler/updater config is no longer a `tauri.conf.json`:
per the rewrite plan the installer moves to standalone WiX scripts under
`Core/GUI/installer/` and the updater to `self_update` + `self-replace`
(declared in `Core/GUI/Cargo.toml`, wired in a post-alpha slice). The
old Tauri bundle-resource and `core/security-api` cleanup items are moot
now that `src-tauri/` is deleted.

---

## 8. Extensions — Phase 7 contract [live]

Layout under `Wylde/Extensions/<name>/`:
- `manifest.json` — name, transport, handler module, declared `tools[]` and `egress[]`
- `handler.py` — Wylde-side dispatch module; bridge imports by file path, calls `run_*` functions per manifest
- Optional `tools/` (legacy adapter) and `browser_extension/` (browser-side code)

Bridge (`Extensions/extension_bridge/`):
- `loader.py` — discovers extensions on disk; mtime-cached
- `registry.py` — runtime enable/disable; flips invalidate the harness tool catalog
- `dispatcher.py` — routes calls; imports `handler.py` via `importlib.util.spec_from_file_location` under a synthetic qualified name (`Extensions/Webcrawler/handler.py:55-80`)
- Public surface in `Extensions/extension_bridge/__init__.py:37-83`

Two ingress paths:
1. **From the LLM tool loop** — enabled extension tools merge into the harness catalog; LLM tool call routes via `extension_bridge.dispatch()` → `handler.run_<endpoint>(params)`.
2. **From an external browser** — `Gateway/routes/extensions.py:47,56` → `services.extensions.dispatch()` → same handler.

### Currently shipped extensions

| Extension | Tools | Egress |
|---|---|---|
| Webcrawler (`Extensions/Webcrawler/manifest.json`) | `fetch → run_fetch`, `scrape → run_scrape`, `extract → run_extract` (group `web`) | `web` → `https://` |
| Wylde_Study (`Extensions/Wylde_Study/manifest.json`) | `study_index_page`, `study_query`, `study_summarize`, `study_explain`, `study_flashcards` (group `study`) | `ingress.browser`, `egress.browser` capabilities |

Webcrawler egress flows through `Wylde.Gateway.client.forward(dest="web", ...)` (`handler.py:140`) with a `_direct_get` fallback for dev (`handler.py:189`); fallback logs at WARNING (`:226`).

Wylde_Study has a `browser_extension/` directory; canonical example of an external-browser caller using `POST /extensions/Wylde_Study/<endpoint>`.

---

## 9. N8N coupling [live]

n8n is an external process Wylde does not own. Coupling lives in three places:

### Python client (in-process)

`N8N/client.py` is the single REST client. Imported by:
- the seven N8N service-side tools at `N8N/tools/n8n_*/`
- the harness when dispatching an N8N tool call
- `Gateway/routes/workflows.py:270-300` for `/api/workflows/n8n*` (uses `proxy_core.http_call(N8N_HTTP, ...)` directly, not via `N8N/client.py` — small inconsistency, both end up at the same n8n daemon)

Public surface (`N8N/client.py:163-396`):

| Function | n8n endpoint |
|---|---|
| `list_workflows()` | `GET /rest/workflows` |
| `get_workflow(id)` | `GET /rest/workflows/{id}` |
| `get_execution(id)` | `GET /rest/executions/{id}` |
| `execute_workflow(id, inputs)` | `POST /rest/workflows/{id}/run` |
| `create_workflow(payload)` | `POST /rest/workflows` |
| `edit_workflow(id, payload)` | `PATCH /rest/workflows/{id}` |
| `delete_workflow(id)` | `POST /rest/workflows/{id}/archive` then `DELETE /rest/workflows/{id}` |

Auth modes (`N8N/client.py:75`): `X-N8N-API-KEY` (preferred), or cookie session via `POST /rest/login` (fallback). Optional outer basic auth.

If neither is configured the module imports cleanly; calls fail-fast with `{"error": "auth_not_configured"}` (`client.py:152`).

### Webhook ingest [partially-implemented]

n8n triggers Wylde back via webhooks. No webhook receivers wired in `Gateway/routes/` yet. The pre-Phase-9 path was through fletch-web (going away). Future receiver lives on Gateway under one of the three legitimate scopes (likely a new router under `routes/n8n_callbacks.py` — KEEP-MOBILE-FUTURE / KEEP-INTEGRATION).

`Core/harness/memory/ingest.py` has an `n8n` reference per `Core/harness/requirements.txt:15` ("HTTP fallbacks (memgraph, ingest → N8N webhook)") — outbound webhook from harness to n8n.

### Workflow templates

`N8N/workflow_templates/agent-orchestra.json` — seed orchestra workflow shipped with Wylde. No code wires this into n8n at boot; it's a template for manual import.

### GUI

The gpui GUI surfaces n8n's editor as an `iframe`-kind panel (the
`ext:n8n/editor` registry key). The Shell's slot mounts a `wry`-backed
WebView child window of the gpui Window pointed at the n8n URL (default
`http://127.0.0.1:5678`), after `wylde-webview`'s `probe_url`
(`Core/GUI/Frontend/Extension_handlers/WebView/`) confirms the editor is
reachable; an unreachable probe falls back to the `ServiceUnavailable`
stub. This is the user-facing surface for editing workflows — see §6 for
the iframe-panel mechanism.

---

## What's reachable today on a clean boot

If `Core/Lifecycle/daemon.py` runs against the live tree on a Python env with `fastapi`, `uvicorn`, `pywin32`, `msgpack` installed:

- `\\.\pipe\wylde-lifecycle` — **up** (`service.list/.health/.start/.stop/.wake/.shutdown_all`)
- `\\.\pipe\wylde-harness` — **up** (chat.* + tools.* + models.* — 9 actions); started in-process by the daemon's Phase 2b
- `\\.\pipe\wylde-memgraph` — **up** (Phase 2c — daemon spawns `Core/Memgraph/run.py` as a subprocess; Neo4j Bolt on `127.0.0.1:7687` reachable after ~10–60s warmup)
- Gateway HTTP — **up** if entry_point spawns (the launcher's `PYTHONPATH` overlay now prepends parent-of-Wylde for `Wylde.Gateway.run`-style entry points); when up, `\\.\pipe\wylde-gateway` is also up via lifespan
- All other pipes — **dead or uncertain**
- Chat turns — **work end-to-end via the harness pipe** with a real Ollama backend that supports tool calls. The gpui Chat panel's transcript consumes `chat.stream_turn`; its tool-activity strip consumes `chat.stream_tools`.
- `POST /api/chat` (Gateway) — still a naked Ollama proxy. Mobile path will repoint to the harness once mobile-side streaming lands.
- `GET /api/tools` — lists 48 harness tools + extension tools per enable state
- `POST /api/models/pull` — Ollama model pull (NDJSON→SSE); browser-side via the GUI's `pullModel`
- `POST /extensions/Webcrawler/run_fetch` etc. — work today (Phase 7 smoke is 7/7 green)
- MCP — **not implemented**
- fletch-web — **gone**

## Smoke results (post-execution)

| Suite | Result |
|---|---|
| `Core/harness/tests/test_turn.py` (chat-turn driver: tool loop / cancel / unknown id / streaming tokens / conversation history / tools+models actions) | **6/6 pass** |
| `Core/harness/tests/test_memory.py` (long-term supersession / workspace-memory durable persistence + explicit-delete / short-term + cross-restart / reflection / curation / scoring) | **15/15 pass + 1 lancedb-skipped** |
| `Core/harness/tests/test_workspaces.py` (registry + MRU + indexing + per-conversation binding + persona) | **3/3 pass + 3 lancedb-skipped** |
| `Core/harness/tests/test_retrieval.py` (HyDE + hybrid + rerank + citations) | 6/6 lancedb-skipped (production env has lancedb installed; this dev env doesn't) |
| `Core/harness/tests/test_graph_retrieval.py` (Memgraph-stubbed multihop + traverse + envelope shapes) | **6/6 pass** |
| `Core/harness/tests/test_scheduler.py` (clock-injection cadence, conversation idle window, state persistence across restart) | **5/5 pass** |
| `Core/harness/tooling/tests/test_smoke.py` | 9/9 pass |
| `Core/harness/model_registry/tests/test_model_registry.py` | 20/20 pass |
| `Gateway/tests/test_pipe_smoke.py` + `test_smoke.py` | 21/21 pass |
| `device_gate/VPN/tests/test_smoke.py` | **33/33 pass** (the previously-pre-existing `test_deleted_modules_are_gone[tools]` was reframed — `tools` is intentionally not in the deleted-list because PEP 420 lets the bare name resolve against any sibling `tools/` on sys.path; the real signal is the `tools.<submodule>` cases, which still test correctly) |
| `Extensions/extension_bridge/tests/smoke_test.py` | 7/7 pass |
| `Extensions/Webcrawler/tests/smoke_test.py` | 6/6 pass |

**Total: 129 pass, 0 fail, 10 lancedb-skipped.** Tool catalog: 63 (the 5 memory tools registered correctly). Harness pipe action surface is now 31 actions (chat.* + tools.* + models.* + rag.workspaces.* + memory.long_term.* + memory.workspace.* + memory.short_term.* + memory.reflect).

## Daemon boot test (2026-05-10)

`py -3 -m Core.Lifecycle.daemon` from `Wylde/` with `PYTHONPATH=parent-of-Wylde`:

```
INFO wylde.lifecycle: daemon: booting Lifecycle controller
INFO wylde.config: config: loaded 8 env vars from 2 files
INFO wylde.lifecycle: control: registered 6 actions on wylde-lifecycle
INFO wylde.harness.pipe: harness pipe: registered 9 chat.* actions
INFO wylde.harness.pipe: harness pipe: serving \\.\pipe\wylde-harness
INFO wylde.lifecycle: memgraph: spawned (pid=25344) — Neo4j boot may take up to 120s
INFO wylde.lifecycle: daemon: ready (\\.\pipe\wylde-lifecycle)
```

Pipe + Bolt status after Neo4j had time to start:

```
OPEN  \\.\pipe\wylde-lifecycle
OPEN  \\.\pipe\wylde-harness
OPEN  \\.\pipe\wylde-memgraph
bolt:7687 OPEN
```

`\\.\pipe\wylde-gateway` correctly absent — Gateway is a separate
service-mode process and isn't part of the daemon's in-process pipe set.
