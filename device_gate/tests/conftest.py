"""Shared fixtures for device_gate tests.

Adds the vault root to ``sys.path`` so tests can
``from device_gate.X import ...`` regardless of pytest's cwd. The
post-rename layout has device_gate addressable as a top-level package
from the vault root.
"""

from __future__ import annotations

import sys
from pathlib import Path

_VAULT_ROOT = Path(__file__).resolve().parent.parent.parent
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))
