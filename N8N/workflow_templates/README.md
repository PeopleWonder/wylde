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

## `rag-ingest.json` — RAG Ingest (AST-aware chunking)

The memory-layer indexing pipeline. Triggered by
`Core/harness/memory/ingest.py::trigger_ingest` (webhook
`/webhook/wylde-ingest`).

### What changed (tree-sitter Slice 2)
The **chunking node** is now an n8n **HTTP Request** node pointed at the
`wylde-treesitter` sidecar's localhost HTTP front door:

```
POST {{ $env.WYLDE_TREESITTER_HTTP_URL || 'http://127.0.0.1:8030' }}/chunk
{ "path": "<abs file path>", "max_chunk_bytes": <optional> }
→ { path, language, ast_aware, chunk_count,
    chunks: [ { start_line, end_line, byte_start, byte_end, kind, symbol_name? } ] }
```

This replaces the previous heuristic chunker (fixed line/character windows)
so chunks fall on **function/class boundaries** instead of arbitrary cuts.
Unknown languages fall back to byte windows. Per
`docs/plans/treesitter-sidecar.md`: the workflow calls the Rust sidecar
**directly via the HTTP Request node** — there is no Python adapter and no
Execute-Command CLI shim. Only Python is parsed today (one statically-linked
grammar); more languages land in Slice 5.

The sidecar binds **loopback only** (`127.0.0.1`). Override the base URL with
`WYLDE_TREESITTER_HTTP_URL` if n8n runs in a container that reaches the host
by a different name (e.g. `http://host.docker.internal:8030`).

### Node graph
`Webhook → Normalise → Discover Files (pre-chunk) → **Chunk (Tree-sitter
Sidecar)** → Expand Chunks → Embed + Index (post-chunk) → Summarise →
Respond`. The pre-chunk discovery and post-chunk embed/index wiring are
unchanged — only the chunk node was swapped. Graph entity/edge upsert
(memgraph) lands in Slice 3.

### Body fields
| Field          | Default     | Meaning                                                  |
| -------------- | ----------- | -------------------------------------------------------- |
| `target_path`  | required    | Root path to index.                                      |
| `workspace_id` | `'default'` | Logical workspace bucket (chunk + graph filters).        |
| `paths`        | `[]`        | Explicit file subset; skips discovery when non-empty.    |
| `options`      | `{}`        | Pass-through knobs, e.g. `{ "max_chunk_bytes": 24576 }`. |
