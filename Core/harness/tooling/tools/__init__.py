"""tools/ — filesystem-as-registry tree of Wylde tools.

Each subdirectory groups related tools (``meta/`` for system-introspection
tools, ``git/`` for repo ops, ``web/`` for HTTP, etc.). Each tool is a folder
containing at least:

* ``manifest.json`` — discoverable metadata (id, description, tags, params).
* one Python module exposing the tool's public ``run_*`` entrypoint.

Discovery is performed by :func:`Wylde.Core.harness.tooling.tool_registry.list_tools`,
which walks ``manifest.json`` files under this tree.
"""
