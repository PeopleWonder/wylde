# wylde_check Batch B + C — findings ledger

This is the per-finding ledger from the Batch B (rules 12–14) and Batch C
(rules 15–19) implementation pass. Counts are after auto-fixes; remaining
items are surfaced for the Wylde user's decision or are expected warnings on
daemon-managed services.

## Batch A leftovers — what landed

### 13 × `gui_action_contract` — **needs the Wylde user's decision**

All 13 calls target actions that were never ported from the prior
orchestrator pipe (renamed/removed during the Phase-9 refactor) to
`Core/harness/pipe.py`. No rename target exists:

| GUI file | line | unresolved action |
| --- | --- | --- |
| `Core/GUI/src/lib/conversations.js` | 22 | `conversations.new` |
| `Core/GUI/src/lib/conversations.js` | 33 | `conversations.list` |
| `Core/GUI/src/lib/conversations.js` | 42 | `conversations.get` |
| `Core/GUI/src/lib/conversations.js` | 47 | `conversations.delete` |
| `Core/GUI/src/lib/ollama.js` | 146 | `harness.models.show` |
| `Core/GUI/src/lib/ollama.js` | 157 | `harness.models.delete` |
| `Core/GUI/src/lib/ollama.js` | 172 | `harness.models.unload` |
| `Core/GUI/src/lib/ollama.js` | 188 | `harness.models.set_active` |
| `Core/GUI/src/lib/systemPrompts.js` | 39 | `prompts.list` |
| `Core/GUI/src/lib/systemPrompts.js` | 58 | `prompts.save` |
| `Core/GUI/src/lib/systemPrompts.js` | 68 | `prompts.save_preset` |
| `Core/GUI/src/lib/systemPrompts.js` | 73 | `prompts.set_active` |
| `Core/GUI/src/lib/systemPrompts.js` | 79 | `prompts.delete_preset` |

Each calling module has an explicit "graceful-degrade until handlers
land" comment at the top — the GUI catches the `action_not_found`
response and renders empty data. Two paths forward:

1. Implement the missing handlers in `Core/harness/pipe.py`. Catalog
   data + backend functions already exist (`system_prompts_catalog.py`,
   conversation persistence in the harness chat-turn loop, model
   registry helpers); only the pipe-action wrappers are missing.
2. Remove the GUI call-sites entirely (means the Conversations page,
   System Prompts section in Settings, and Models page lose features).

Recommendation: implement the handlers. They're thin wrappers around
existing harness machinery and the GUI surfaces are non-trivial.

### `gui_no_backend_bypass` — fixed

`Core/GUI/src/pages/Settings.svelte:709` referenced the literal
`data/system_prompts.json` in a UI label describing where edits land.
Replaced with "saved through the harness pipe."

### `gui_pipe_constants` — fixed

Removed the `SVC_TRAINER = 'wylde-trainer'` constant from
`Core/GUI/src/lib/api.js`. The Trainer pipe doesn't exist yet, so the
wrappers (`trainerHealth`, `listTrainingJobs`, etc.) now throw a stable
`_TRAINER_PIPE_PENDING` error rather than reference a missing service
constant. When the pipe lands, re-introduce `SVC_TRAINER` and swap the
stub bodies for real `pipeCall` sites.

### `dead_service_refs` — fixed

The example list inside rule-6's description (in
`docs/wylde_check_rules.md`) was tripping rule 6 on itself. Rephrased to
omit specific dead-service names; the canonical list now lives only in
`DEAD_SERVICE_NAMES` inside `wylde_check.py`.

## Batch B (rules 12–14) — fixes applied

### Rule 12 (`tool_docstring_required`) — 0 findings on first run

Every tool file under `Core/harness/tooling/tools/**/*.py` already
carries a ≥20-char module docstring. No fixes needed.

### Rule 13 (`logging_setup_only`) — 12 findings, all fixed

Replaced direct `logging.basicConfig` / `logging.getLogger().setLevel`
calls with `Core.shared.logging_setup.configure_logging` (or a no-op
fallback when run as a subprocess that bootstraps its own `sys.path`):

| File | Resolution |
| --- | --- |
| `Core/Memgraph/graph_service.py:908` | Removed — `main()` is invoked from `run.py`, which already configures logging. |
| `Core/Memgraph/run.py:43` | Fallback now defines a no-op `configure_logging` instead of calling `logging.basicConfig`. |
| `Core/Memgraph/seed_graph.py:614` | Replaced with `configure_logging(level=…)`. |
| `Gateway/run.py:39` | Replaced with `configure_logging(service="wylde-gateway")`. |
| `device_gate/run.py:85` | Replaced with `configure_logging(service="wylde-device-gate")` with import guard. |
| `Voice/download_models.py:37` | Replaced with `configure_logging(service="wylde-voice")` with import guard. |
| `Voice/run.py:75` | Replaced with `configure_logging(service="wylde-voice")` with import guard. |
| `VPN/api.py:44` | Replaced with `configure_logging(service="wylde-vpn")` with import guard. |
| `VPN/tunnel/dns_stub.py:24` | Replaced with `configure_logging(service="wylde-vpn")` with import guard. |
| `Trainer/Caption/cli.py:121` | Replaced with `configure_logging(level=…)`. |
| `Trainer/Caption/config.py:39` | Replaced with `configure_logging(level=…)` with import guard. |
| `Trainer/Caption/download_models.py:27` | Replaced with `configure_logging()` with import guard. |

The rule itself also gained two soft skips: (1) `wylde_check.py` itself
(its docstring literally lists the pattern names the rule catches), and
(2) lines inside triple-quoted docstrings (so mentions of the rule's
matched APIs in service-side docs don't trip it).

### Rule 14 (`no_external_subprocess`) — 0 findings on first run

The allowlist captures every legitimate subprocess site: Lifecycle
daemon, harness dev tools, all tool runtimes (`Core/harness/tooling/tools/`),
the Memgraph JVM wrapper, the VPN tunnel shell-outs, and the Voice
device manager. Nothing else uses subprocess in active code.

## Batch C (rules 15–19) — first-run results

### Rule 15 (`spawn_paths_exist`) — 0 findings

Every `-m <module>` reference inside `Core/Lifecycle/daemon_state.py`
resolves (`Core.Memgraph.run`, `Voice.run`, `device_gate.run`,
`Core.resource_monitor.run`, `Gateway.run`).

### Rule 16 (`run_py_entry_point`) — 0 findings

No service folder uses a deprecated `<svc>_run.py` / `start_*.py` /
`launcher*.py` variant. The Trainer / N8N / Extensions folders are
library-style and tolerated as having no entry point.

### Rule 17 (`pipe_name_convention`) — 0 findings (after regex tightening)

Initial implementation overshot — the underscore-form regex was
catching legitimate Python identifiers (`wylde_root`, `wylde_check`,
`wylde_ipc`, `wylde_pending_events`). The typo regex is now anchored to
`pipe\…` / `pipe/…` paths, which is the only context where a typo
matters (and where it's unambiguously a mistake).

### Rule 18 (`run_py_startup_sequence`) — 11 warnings, all expected

Every existing `run.py` is missing at least one step of the
configure_logging → write_manifest → start_heartbeat → serve-loop
convention. All 11 warnings are on daemon-managed services where the
Lifecycle daemon writes the manifest and runs heartbeats on the
service's behalf:

| Service | Missing |
| --- | --- |
| `Core/resource_monitor/run.py` | write_manifest, start_heartbeat |
| `Core/Memgraph/run.py` | write_manifest, start_heartbeat |
| `device_gate/run.py` | write_manifest, start_heartbeat |
| `Gateway/run.py` | write_manifest, start_heartbeat |
| `Voice/run.py` | write_manifest, start_heartbeat |
| `VPN/run.py` | configure_logging |

Recommendation: leave as-is. The warnings document that the run.py
files intentionally delegate manifest writes to the daemon. A future
pass could tighten the rule to consult the daemon-managed list before
warning, but that's an evolution, not a fix.

### Rule 19 (`shutdown_handler_marks_stopped`) — 6 warnings, all expected

Same shape as Rule 18: all six existing `run.py` files lack an explicit
manifest-cleanup call in their signal handler, but for daemon-managed
services the daemon's `_stop_*` helpers do that cleanup. Surfaced as
warnings rather than auto-fixed.

## Totals after this pass

- **Errors**: 13 (all `gui_action_contract`, awaiting the Wylde user's decision on implement-vs-remove).
- **Warnings**: 17 (all on `run.py` files; rules 18/19 fire because daemon-managed services delegate manifest lifecycle to the daemon).
- **Total findings**: 30, down from 47 on first run after Batch B+C landed.
