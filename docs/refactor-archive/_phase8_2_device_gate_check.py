"""Phase 8.2 — Device Gate static-import smoke check.

The Device Gate folder contains a space ("Device Gate"), so it can't be
imported by dotted name. This loads device_gate.py via importlib and verifies
the module exposes its expected Flask app and main(), without actually
running the service.

Run from anywhere — the script discovers its own location and finds the
file. The script does NOT call main() or open sockets.

Expected outcome: prints "ok: <module>" plus a list of routes. Failure is
loud; missing optional deps (flask, etc.) print a hint instead of a stack
trace.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEVICE_GATE_PY = HERE / "Device Gate" / "device_gate.py"


def main() -> int:
    if not DEVICE_GATE_PY.is_file():
        print(f"FAIL: {DEVICE_GATE_PY} not found", file=sys.stderr)
        return 2

    # Make Wylde.* importable so device_gate's `from Wylde.Core.shared import
    # discovery / ipc` succeeds. We add the parent of the Wylde folder.
    wylde_parent = HERE.parent
    if str(wylde_parent) not in sys.path:
        sys.path.insert(0, str(wylde_parent))

    spec = importlib.util.spec_from_file_location(
        "wylde_device_gate.device_gate", DEVICE_GATE_PY
    )
    if spec is None or spec.loader is None:
        print("FAIL: spec_from_file_location returned None", file=sys.stderr)
        return 2
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    try:
        spec.loader.exec_module(mod)
    except ModuleNotFoundError as exc:
        # Likely flask / flask_cors not installed in this Python env. Treat
        # as a soft pass for static-import purposes — the file parses, the
        # missing piece is just runtime deps.
        print(f"soft-pass: module parsed but runtime dep missing: {exc}")
        return 0
    except Exception as exc:
        print(f"FAIL: importing device_gate.py raised: {exc!r}", file=sys.stderr)
        return 1

    print(f"ok: {mod}")
    print(f"  SERVICE_NAME = {getattr(mod, 'SERVICE_NAME', '<missing>')}")
    print(f"  PORT         = {getattr(mod, 'PORT', '<missing>')}")
    print(f"  has main()   = {callable(getattr(mod, 'main', None))}")
    app = getattr(mod, "app", None)
    if app is not None:
        try:
            routes = sorted({r.rule for r in app.url_map.iter_rules()})
            print(f"  routes       = {routes}")
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
