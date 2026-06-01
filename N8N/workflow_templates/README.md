# N8N Workflow Templates

Importable n8n workflow definitions, hoisted to
`Wylde/N8N/workflow_templates/` during the Phase 8 service merge
(punch-list item #3). The pre-Phase-8 shape lived under
`_n8n_service_merge/templates/` alongside a Flask shell that has now
been removed. A short-lived `N8N/templates/` folder existed during the
folding work and was consolidated into this directory — the canonical
location locked in at Phase 7.

## `agent-orchestra.json` — Multi-Agent Coding Workflow

A thin webhook → poll → respond wrapper around the native
`agent_orchestra` workflow defined in the orchestrator. The post-Phase-8
home for the native workflow is TBD.

### What n8n does here
- Accept a task via webhook
- Pull prior lessons from wylde-rag memory for observability
- Kick off the heavy native orchestration (spec → architect → TDD →
  coder → debug-reflect loop → critic → log lesson → summary)
- Poll the orchestrator for completion
- Package the summary + test/critic results and return them via the
  webhook

### What n8n does NOT do
The reflection loops (debugger ↔ coder, up to 5 rounds), human gates,
and experiential lesson-logging all run inside the orchestrator — n8n is
only the edge trigger. That keeps n8n's node graph small and the
complex parts auditable in one place.

### Importing
Three ways:

1. **Programmatically via the harness** — call the gated
   `n8n_create_workflow` tool with the JSON payload as `payload`. The
   tool runner will demand confirmation unless auto-mode is enabled.

2. **Via n8n's UI**: open `http://localhost:5678`, click
   *Workflows → Import from File*, select `agent-orchestra.json`.

3. **Via the legacy `n8n-service` HTTP route**: removed in Phase 8 (the
   Flask shell is gone). Use option 1 or 2 instead.

### Invoking
```bash
curl -X POST http://localhost:5678/webhook/agent-orchestra \
  -H "Content-Type: application/json" \
  -d '{"task": "Add rate limiting to the /api/tools endpoint", "poll_max": 120, "poll_every_s": 5}'
```

### Body fields
| Field          | Default  | Meaning                                              |
| -------------- | -------- | ---------------------------------------------------- |
| `task`         | required | The coding task to hand to the Spec Agent.           |
| `poll_max`     | 180      | Max poll attempts before returning (safety ceiling). |
| `poll_every_s` | 5        | Seconds between status polls.                        |
| `requested_by` | `'n8n'`  | Attribution tag; shows up in traces.                 |
| `trace_hint`   | `''`     | Optional free-form tag for grouping runs.            |
