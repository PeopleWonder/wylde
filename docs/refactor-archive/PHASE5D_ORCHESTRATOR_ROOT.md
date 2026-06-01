# Phase 5 Pass D — Orchestrator Root Inventory

## Classification table

| File | LOC | What it does | Tool-call routing | Destination | Reason |
|------|-----|--------------|-------------------|-------------|--------|
| `harness_api.py` | ~622 | Pipe-action surface for the harness (health, models CRUD, settings, prompts, conversations, chat.complete) — registers handlers via `ipc.register_action`. | clean (pipe-only, no LLM-tool-via-GUI; chat.complete returns tool_calls but does not dispatch them). | `Core/harness/_legacy/orchestrator_api/harness_api.py` | API surface to keep as reference for the new harness pipe contract. |
| `orchestrator_api.py` | ~1526 | Flask app: workflow run/compose/stream/resume, gates, agent-turn routes, autotuner, optimizer, traces, lint, model-registry routes; calls `register_harness_actions()` at boot. | GUI-mediated (agent-turn dispatches tool calls via SSE events the frontend then executes — see red flags). | `Core/harness/_legacy/orchestrator_api/orchestrator_api.py` | Reference for new harness/orchestrator API surface; keep verbatim until new harness covers parity. |
| `run.py` | ~215 | Standalone Windows launcher: loads `config.yaml`, sets env vars, writes manifest, builds tools manifest from `tools.TOOLS`, calls `orchestrator_api.main()`. | N/A | `N8N/_legacy/wylde-orchestrator-root/run.py` | Boot script for the orchestra service; dies with the orchestra. |
| `startup.py` | 61 | Installs/uninstalls Windows Startup-folder `.bat` shortcut for `start_orchestrator.bat`. | N/A | `N8N/_legacy/wylde-orchestrator-root/startup.py` | Per-orchestra autostart helper; dies with the orchestra. |
| `ipc.py` | ~1399 | Unified pipe/HTTP IPC transport: `send`/`call`/`serve`, action dispatch (`register_action`/`/__action__`), pipe pool, handshake, framed msgpack. AUTO-GENERATED from `core/shared/ipc.py`. | clean (transport only). | `Core/shared/ipc.py` | Shared transport — already lives canonically in `core/shared/`; the orchestrator copy is auto-synced. |
| `discovery.py` | ~468 | Service discovery wrapper over Consul + mDNS/zeroconf with shared API (`register_service`, `get_healthy_instances`, `get_pipe_name`). AUTO-GENERATED. | N/A | `Core/shared/discovery.py` | Same as ipc.py — canonical source is `core/shared/`. |
| `consul_client.py` | 326 | Stdlib-only Consul HTTP client (register/deregister/health-check/heartbeat). AUTO-GENERATED. | N/A | `Core/shared/consul_client.py` | Canonical source in `core/shared/`. |
| `manifest.py` | ~209 | Service manifest writer + heartbeat (writes `data/manifests/<svc>.json`). AUTO-GENERATED. | N/A | `Core/shared/manifest.py` | Canonical source in `core/shared/`. |
| `errors.py` | ~249 | 7-code error taxonomy + `IpcError`, `classify`, `to_envelope`, `retry_with_backoff`. AUTO-GENERATED. | N/A | `Core/shared/errors.py` | Canonical source in `core/shared/`. |
| `config.yaml` | 50 | Service config: ports, upstream URLs, paths, planner model, autotuner knobs, Consul. | N/A | `N8N/_legacy/wylde-orchestrator-root/config.yaml` | Per-orchestra config; dies with the orchestra. |
| `requirements.txt` | 7 | Trivially small — flask, flask-cors, requests, pyyaml, msgpack, pywin32. | N/A | `N8N/_legacy/wylde-orchestrator-root/requirements.txt` | Orchestra service deps. |
| `start_orchestrator.bat` | 53 | Windows venv-bootstrap + `python run.py` launcher. | N/A | `N8N/_legacy/wylde-orchestrator-root/start_orchestrator.bat` | Orchestra launcher script. |
| `README.md` | 7 | Trivially small — five-line service blurb. | N/A | `N8N/_legacy/wylde-orchestrator-root/README.md` | Per-orchestra blurb. |
| `tools/__init__.py` | ~241 | `TOOLS` schema + `TOOL_HANDLERS` dispatch table for run_agent_orchestra, run_workflow, list_workflows, get_workflow_status, respond_to_gate, tool_search, graph_query_fallback. | clean (declares schema; handlers fire via Flask `/api/<tool_id>`, not via GUI). | `N8N/_legacy/wylde-orchestrator-root/tools/__init__.py` | Workflow-orchestra tool catalog; dies with the orchestra. |
| `tools/client.py` | 30 | Trivially small — loopback HTTP helpers (`orch_get`, `orch_post`) for tool handlers calling the local orchestrator. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/client.py` | Tied to orchestra loopback URL. |
| `tools/list_workflows.py` | 16 | Trivially small — wraps GET `/workflows`. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/list_workflows.py` | Orchestra-specific. |
| `tools/get_workflow_status.py` | 57 | Wraps GET `/workflow/<id>/status` and produces a node-summary. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/get_workflow_status.py` | Orchestra-specific. |
| `tools/respond_to_gate.py` | 38 | Wraps POST `/workflow/<id>/gate/<node>/respond`. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/respond_to_gate.py` | Orchestra gate plumbing. |
| `tools/run_workflow.py` | 36 | Wraps POST `/workflow/run` for arbitrary workflow_id or inline definition. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/run_workflow.py` | Orchestra-specific. |
| `tools/run_agent_orchestra.py` | 36 | Wraps POST `/workflow/run` with `workflow_id="agent_orchestra"`. | clean | `N8N/_legacy/wylde-orchestrator-root/tools/run_agent_orchestra.py` | Orchestra-specific. |
| `tools/tool_search.py` | ~160 | Catalog search against tool-registry (`/api/tools`) with token-overlap scorer; fallback to orchestrator URL. | clean | `Core/harness/tooling/tool_search.py` | Generic capability-match against tool registry; useful in new harness tooling layer. |
| `tools/graph_query.py` | ~158 | Graph traversal fallback — wylde-rag `/api/graph_query` first, wylde-graph `/traverse` second; auto-extracts entities. | clean | `Core/harness/tooling/graph_query.py` | Generic graph fallback usable by the new harness; not orchestra-specific. |
| `streaming/__init__.py` | 1 | Stub re-export. | N/A | `N8N/_legacy/wylde-orchestrator-root/streaming/__init__.py` | Tied to SSE channels. |
| `streaming/sse.py` | ~131 | In-process SSE channel manager: `open_channel`, `push_event`, `close_channel`, `stream_events`, `get_history` with bounded history + replay. | N/A | `N8N/_legacy/wylde-orchestrator-root/streaming/sse.py` | Used by orchestrator workflow runs only; new harness streaming lives elsewhere. |
| `models/__init__.py` | ~668 | Model registry: profiles, benchmarks, capability-slot routing, churn-prevention, HF discovery, Ollama-model watcher; merged from wylde-model-registry. | clean (no LLM tool-call routing — purely model selection/bench). | `Core/harness/backend/model_registry.py` | Capability-slot routing + benchmarks are useful in the new harness backend area. |
| `inference/__init__.py` | 14 | Trivially small — backwards-compat shim re-exporting `harness.backend_routing.InferenceRouter`, `default_router`, `BackendError`, `ChatResult`. | clean (just re-exports). | `DELETE` | Pure compat shim; new code already imports from `harness.backend_routing`. Despite the name, NOT related to the inference-bar bug. |
| `inference/router.py` | 15 | Trivially small — same shim, alternate import path. | clean | `DELETE` | Same compat shim; redundant with new harness imports. |

## Tool-call routing red flags

`orchestrator_api.py` — agent-turn HTTP routes pass tool dispatch through SSE events that the frontend (Fletch GUI) consumes and round-trips back, which is the very pattern the refactor is meant to kill.

- **File:** `%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\orchestrator_api.py`
- **Lines 700-711** — SSE stream comment names the offending event types:
  - Quote (under 15 words): `"assistant_token / tool_call_pending / app_tool_dispatch / ..."` (line 702)
  - Why it violates: `app_tool_dispatch` is the GUI-mediated tool-call channel; the LLM emits a tool call, the orchestrator pushes it to SSE, the GUI executes it in the frontend, then POSTs the result back.
- **Lines 741-758** — `agent_turn_app_tool_result` route accepts client-side tool results:
  - Quote: `"Deliver a client-side tool result back to a paused loop"` (line 744)
  - Why it violates: the route exists specifically to receive tool results that the GUI executed on behalf of the LLM — the wrong-direction loop.
- **Lines 121-122** — imports `deliver_app_tool_result as agent_deliver_app_tool_result` from `workflows/agent_turn`:
  - Quote: `"agent_deliver_app_tool_result"` (line 122)
  - Why it violates: explicit handoff plumbing for the GUI→tool→GUI→LLM round trip. (Note: `workflows/agent_turn` is in the already-migrated `workflows/` subdir, but the import line in this root file is the public symptom.)

`run.py` line 109 mentions InferenceBar in the docstring of `_params_list_to_schema` — `"the InferenceBar / Ollama tool-calling expects"` — this is descriptive only, no actual GUI-routed dispatch happens here, so not flagging as a red flag, but worth noting the coupling assumption.

`tools/__init__.py` line 6 docstring references `"the embedded LLM in Fletch (InferenceBar)"` as the consumer — descriptive comment, not GUI-mediated dispatch in code. The handlers themselves call back via `client.py` loopback HTTP, which is `LLM → orchestrator-tools → orchestrator-routes` — clean per the new principle.

`harness_api.py`'s `_h_chat_complete` (lines 461-551) returns `tool_calls` to the caller but never executes them — that decision belongs to the caller. Clean.

No other GUI-mediated tool-dispatch patterns found in the in-scope files.

## Recommended grouped moves

### Group 1 — `Core/harness/_legacy/orchestrator_api/`
- `_legacy/core/wylde-orchestrator/harness_api.py`
- `_legacy/core/wylde-orchestrator/orchestrator_api.py`

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\Core\harness\_legacy\orchestrator_api" harness_api.py orchestrator_api.py /MOV
```

### Group 2 — `Core/shared/` (auto-generated copies; verify canonical exists first)
- `_legacy/core/wylde-orchestrator/ipc.py`
- `_legacy/core/wylde-orchestrator/discovery.py`
- `_legacy/core/wylde-orchestrator/consul_client.py`
- `_legacy/core/wylde-orchestrator/manifest.py`
- `_legacy/core/wylde-orchestrator/errors.py`

These are auto-generated copies of `core/shared/*`. If `Core/shared/` already holds the canonical versions, **delete** these copies instead of moving them. Robocopy form (move only — dedup manually):

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\Core\shared" ipc.py discovery.py consul_client.py manifest.py errors.py /MOV
```

### Group 3 — `Core/harness/tooling/`
- `_legacy/core/wylde-orchestrator/tools/tool_search.py`
- `_legacy/core/wylde-orchestrator/tools/graph_query.py`

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\tools" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\Core\harness\tooling" tool_search.py graph_query.py /MOV
```

### Group 4 — `Core/harness/backend/`
- `_legacy/core/wylde-orchestrator/models/__init__.py`  → rename to `model_registry.py` after move

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\models" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\Core\harness\backend" __init__.py /MOV
:: then rename
ren "%USERPROFILE%\Documents\Obsidian Vault\Wylde\Core\harness\backend\__init__.py" model_registry.py
```

### Group 5 — `N8N/_legacy/wylde-orchestrator-root/` (everything that dies with the orchestra)

Root-level files:
- `run.py`, `startup.py`, `config.yaml`, `requirements.txt`, `start_orchestrator.bat`, `README.md`

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\N8N\_legacy\wylde-orchestrator-root" run.py startup.py config.yaml requirements.txt start_orchestrator.bat README.md /MOV
```

`tools/` subdirectory (orchestra-coupled handlers — keep folder structure):

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\tools" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\N8N\_legacy\wylde-orchestrator-root\tools" __init__.py client.py list_workflows.py get_workflow_status.py respond_to_gate.py run_workflow.py run_agent_orchestra.py /MOV
```

`streaming/` subdirectory (orchestra-coupled SSE):

```bat
robocopy "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\streaming" "%USERPROFILE%\Documents\Obsidian Vault\Wylde\N8N\_legacy\wylde-orchestrator-root\streaming" __init__.py sse.py /MOV
```

### Group 6 — DELETE (compat shims)
- `_legacy/core/wylde-orchestrator/inference/__init__.py`
- `_legacy/core/wylde-orchestrator/inference/router.py`

```bat
del /Q "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\inference\__init__.py"
del /Q "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\inference\router.py"
rmdir /Q "%USERPROFILE%\Documents\Obsidian Vault\Wylde\_legacy\core\wylde-orchestrator\inference"
```

## Open questions

1. **`Core/shared/` canonical files** — are `ipc.py`, `discovery.py`, `consul_client.py`, `manifest.py`, `errors.py` already present in `Core/shared/`? The orchestrator copies all carry an `# AUTO-GENERATED, edit core/shared/...` header, suggesting the canonical lives there. If so, the orchestrator copies should be deleted, not moved. Confirm before running Group 2.

2. **`models/__init__.py` (model registry, ~668 LOC)** — I suggested `Core/harness/backend/model_registry.py`. The "backend" sub-area in the brief is ambiguous: this file is more about *which* model to pick (capability slots, benchmark scoring, churn prevention) than about *how* to talk to a backend. A separate `Core/harness/model_registry/` sub-area might fit better. Surface for your call.

3. **`tools/__init__.py` (TOOLS schema + dispatch)** — sent to `N8N/_legacy/wylde-orchestrator-root/`. The schemas themselves describe orchestra-specific tools (run_agent_orchestra, respond_to_gate, etc.) so this is correct. But the **pattern** of declaring a TOOLS dict + TOOL_HANDLERS dispatch is exactly what the new harness `tooling/` layer needs as a model. Want me to flag it for "extract pattern, then archive" rather than straight archive?

4. **`tool_search.py` and `graph_query.py`** — both fetch from `tool-registry` / `wylde-rag` via HTTP loopback. They're useful in concept but currently coupled to those services. Confirm they belong in the new `Core/harness/tooling/` (they call sibling services, not LLM-via-GUI), or whether the new harness expects in-process implementations.

5. **`harness_api.py` destination** — it's labelled as `_legacy/orchestrator_api/` because its dispatch wiring (`register_harness_actions` → `ipc.register_action`) is currently bolted onto the orchestrator boot. The handler bodies themselves are clean and re-targetable. Do you want this split — handlers to a live `Core/harness/api/` location, registration shim to `_legacy/`?

6. **`orchestrator_api.py` red flags vs. preservation** — flagging `app_tool_dispatch` and `agent_turn_app_tool_result` as the GUI-mediated bug, but the file is being archived in `_legacy/` for reference rather than fixed in place. Confirm that's the intent (archive the bug verbatim, don't try to clean it before moving).

## Already-migrated subdirs (not in scope)

Confirmed seen and skipped:
- `graph/`
- `harness/`
- `planner/`
- `optimizer/`
- `autotuner/`
- `gates/`
- `guards/`
- `tracing/`
- `budget/`
- `checkpoint/`
- `versioning/`
- `delegation/`
- `workflows/`
- `linter/`

Also encountered (and skipped per the standing rule): `__pycache__/` directories under `tools/`, `streaming/`, `models/`, `inference/`.
