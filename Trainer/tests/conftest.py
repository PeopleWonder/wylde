"""pytest config — mirror the runtime PYTHONPATH overlay.

Production launches the worker with ``PYTHONPATH=<parent-of-WYLDE_ROOT>``
so ``from Wylde.Trainer.Caption ...`` resolves the namespace-package
``Wylde``. Pytest runs from ``WYLDE_ROOT`` itself, which only exposes
the top-level ``Trainer`` package, so the worker's internal
``Wylde.X`` imports fail without this overlay.

Adding ``parent-of-WYLDE_ROOT`` to ``sys.path`` here keeps the test
environment byte-equivalent to the daemon-spawned worker — same
imports, same resolution order.
"""

from __future__ import annotations

import sys
from pathlib import Path

WYLDE_ROOT = Path(__file__).resolve().parents[2]
_NS_ROOT = str(WYLDE_ROOT.parent)
if _NS_ROOT not in sys.path:
    sys.path.insert(0, _NS_ROOT)
