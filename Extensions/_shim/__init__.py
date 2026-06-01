"""wylde_mcp_py_shim — adapter that exposes a legacy importlib-style
extension (the `handler.py` + `manifest.json` shape used by Phase 3's
``Extensions.extension_bridge``) as a Model Context Protocol server
on stdio.

The Rust ``wylde-extension-bridge`` host loads any-language MCP
servers. This package lets a Phase 3 Python extension migrate to the
new contract without being rewritten — its ``mcp-server.json``
declares this shim as the server, the shim translates MCP
``tools/call`` requests into ``handler.<endpoint>(params)`` calls.

Run as::

    python -m Extensions._shim.server --extension <Name>

The ``--extension`` arg names a sibling folder under ``Extensions/``;
the shim reads that folder's legacy ``manifest.json`` for the tool
catalog and imports ``handler.py`` (or the module named in the
manifest's ``handler`` field).
"""
