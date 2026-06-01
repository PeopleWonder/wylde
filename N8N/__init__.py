"""Wylde.N8N — n8n integration surface.

Public modules:

* :mod:`Wylde.N8N.client` — authenticated REST client for n8n. Exposes
  ``list_workflows``, ``get_workflow``, ``execute_workflow``,
  ``create_workflow``, ``edit_workflow``, ``delete_workflow``.

The seven harness-callable tools live at ``Wylde/N8N/tools/<tool_id>/``
(service-owned, alongside the client they wrap). The tool_registry
walker discovers them via its ``Wylde/<Service>/tools/`` pass and
unions them into the catalog; ``Core/harness/tooling/tools/`` no longer
holds an n8n group. See Phase 8 punch-list item #3 for the design
rationale and Phase 8.5 for the service-folder hoist.

The ``_legacy/`` folder under this package preserves the pre-Phase-8
orchestrator/improve services for reference. Do not import from it.
"""
