"""Wylde.N8N.tools — service-owned tool surface for the n8n integration.

Each subfolder is one harness-callable tool, structured the same way as
the harness-internal tools under ``Core/harness/tooling/tools/``: a
``manifest.json`` next to ``<tool_id>.py``, with the package's
``__init__.py`` re-exporting ``run_<tool_id>`` for convenience.

Discovery is performed by ``Wylde.Core.harness.tooling.tool_registry``,
which walks each top-level service folder's ``tools/`` subdirectory in
addition to the harness ``tools/`` tree and the extension_bridge surface.
The registry stamps each catalog entry's ``module`` field with the
absolute import path so the runner can dispatch without knowing where
the tool physically lives.

This convention — services own the tools they expose — replaces the
older "all tools live under the harness" layout. It mirrors how
extensions contribute tools, but for first-party services that ship in
the same checkout (N8N, future Voice/Caption/etc.).
"""
