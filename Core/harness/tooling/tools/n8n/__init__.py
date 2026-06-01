"""tools/n8n/ — workflow operations against a running n8n instance.

Seven tools, three of which mutate state and therefore declare
``requires_confirmation: true`` in their manifest (see Wylde Design
Principle #12 — the confirmation gate, with auto-mode bypass).

Read-only:

* ``n8n_list_workflows``   — list every workflow (id, name, active flag)
* ``n8n_get_workflow``     — fetch a single workflow definition by id
* ``n8n_get_execution``    — fetch a single execution's status payload by id
* ``n8n_execute_workflow`` — kick off a workflow run (effects are bounded
  by the workflow itself, so this is treated as non-mutating at the
  catalog level)

Gated (mutating):

* ``n8n_create_workflow`` — POST a new workflow into n8n
* ``n8n_edit_workflow``   — PATCH an existing workflow
* ``n8n_delete_workflow`` — archive + delete a workflow

Each tool is a thin wrapper over :mod:`Wylde.N8N.client`. The runtime
import path is resolved via ``sys.path`` containing the project root
(``Wylde/`` is a namespace package). If that import fails — usually
because the harness is being inspected from outside a configured Wylde
checkout — the tools surface the failure as a structured error instead
of crashing the registry walk.
"""
