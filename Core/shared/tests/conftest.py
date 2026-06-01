"""Test environment for ``Core/shared/tests/``.

The previous incarnation of this conftest added ``Core/shared/`` to
``sys.path`` so the tests could write bare imports (``import ipc``).
That mutation leaked across the pytest session and contaminated
adjacent test suites (notably ``VPN/tests/test_smoke.py``, which
asserts bare names like ``consul_client`` / ``ipc`` are *not*
resolvable from VPN's perspective).

The Core/shared tests have been switched to qualified imports
(``from Core.shared import ipc``) — matching what every active
service does today — so the path mutation is gone.  What remains:
isolate the IPC audit log under a per-test tmp dir so we don't
write into the live ``logs/`` folder.
"""

from __future__ import annotations

import os
from pathlib import Path

_SHARED = Path(__file__).resolve().parent.parent

os.environ.setdefault("WYLDE_IPC_LOG", str(_SHARED / "tests" / "_tmp" / "ipc.jsonl"))
(_SHARED / "tests" / "_tmp").mkdir(parents=True, exist_ok=True)
