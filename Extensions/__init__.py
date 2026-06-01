"""Wylde.Extensions — namespace for extensions that talk to the outside world.

By the Wylde user's Phase 7 definition: an *extension* is anything that leaves the
system (web, browser, native services). Tool groups under
``Core/harness/tooling/tools/`` stay internal. Extensions live as
sibling folders here, each with an ``manifest.json`` declaring the tools
it provides plus a ``handler.py`` that the extension_bridge's dispatcher
calls into.

The extension_bridge itself is a folder with a space in its name —
``Wylde/Extensions/extension_bridge/`` — so it can't be reached with a
plain ``import`` statement. This ``__init__.py`` exists so that callers
elsewhere in Wylde can do::

    from Wylde.Extensions import extension_bridge

and get a fully working package back. Under the hood we register the
folder as a Python package via ``importlib`` machinery, which lets
relative imports inside the bridge (``from .contract import Extension``)
resolve correctly. The module is registered as
``Wylde.Extensions.extension_bridge`` in :data:`sys.modules`; that name
is what relative imports inside the bridge see as their package qualname.

This is the only price of honouring the literal spec for the folder
name. Everything else inside the bridge is plain Python.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

_EXTENSIONS_DIR = Path(__file__).resolve().parent
_BRIDGE_DIR = _EXTENSIONS_DIR / "extension_bridge"
_BRIDGE_PKG = "Wylde.Extensions.extension_bridge"


def _load_bridge_package():
    """Load ``Wylde/Extensions/extension_bridge/`` as a Python package.

    Idempotent: re-imports do not reload. Sets up the package so that
    siblings like ``contract.py``, ``loader.py``, ``registry.py``, and
    ``dispatcher.py`` can use relative imports.
    """
    if _BRIDGE_PKG in sys.modules:
        return sys.modules[_BRIDGE_PKG]
    init_path = _BRIDGE_DIR / "__init__.py"
    if not init_path.is_file():
        # Bridge not yet present; return a stub so callers can detect
        # absence without raising at import time.
        return None
    spec = importlib.util.spec_from_file_location(
        _BRIDGE_PKG,
        init_path,
        submodule_search_locations=[str(_BRIDGE_DIR)],
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[_BRIDGE_PKG] = module
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


extension_bridge = _load_bridge_package()

__all__ = ["extension_bridge"]
