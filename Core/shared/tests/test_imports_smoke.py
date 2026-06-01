"""Smoke test: every shared module must be importable as ``Core.shared.X``.

If a service reaches for ``from Core.shared import ipc`` and the import
blows up, nothing else matters. This test fails fast on the most common
regression class — renaming something or breaking a dependency graph in
``Core/shared/``.

The qualified ``Core.shared.X`` form is the canonical access path in
the post-Phase-9 tree; the older bare-name pattern (``import ipc``)
was a Docker-era artifact that survived in tests until the namespace
contamination it caused started leaking into adjacent suites.
"""

from __future__ import annotations

import importlib

import pytest

SHARED_MODULES = [
    "Core.shared.ipc",
    "Core.shared.manifest",
    "Core.shared.vram_broker",
    "Core.shared.discovery",
    "Core.shared.consul_client",
    "Core.shared.tool_interface",
]


@pytest.mark.parametrize("modname", SHARED_MODULES)
def test_module_imports(modname: str) -> None:
    mod = importlib.import_module(modname)
    assert mod is not None
    # Every shared module should define __all__ or at least a callable/class.
    # Just probe that the module object has *something* attached.
    assert dir(mod), f"{modname} imported but is empty"


def test_ipc_exports_stable_surface() -> None:
    from Core.shared import ipc

    # Lock in the public surface services call. Breakage here = breakage
    # everywhere the mesh talks to itself.
    for name in ("send", "call", "serve", "Reply", "IpcError", "PipeServer"):
        assert hasattr(ipc, name), f"ipc.{name} missing"


def test_manifest_exports_stable_surface() -> None:
    from Core.shared import manifest

    for name in ("write_manifest", "start_heartbeat", "stop_heartbeat"):
        assert hasattr(manifest, name), f"manifest.{name} missing"


def test_vram_broker_exports_stable_surface() -> None:
    from Core.shared import vram_broker

    for name in (
        "reserve",
        "release",
        "reserved",
        "Priority",
        "Lease",
        "VramError",
        "VramUnavailable",
        "get_state",
    ):
        assert hasattr(vram_broker, name), f"vram_broker.{name} missing"
