"""VRAM broker shim — backward-compat re-export.

The broker implementation lives in the ``broker/`` subpackage alongside
this file. This shim re-exports the names that external callers
(test_vram_broker.py, vram_broker_client) historically imported from
``vram_broker_service``, so test monkey-patches and the client shim
don't have to change at the call sites.

When adding new attributes to the broker, prefer importing from the
relevant submodule directly (e.g. ``from Core.resource_monitor.broker.workers
import ...``) rather than extending this shim. New code should not lean on
this re-export surface.
"""

from __future__ import annotations

import urllib.request  # noqa: F401 — historical patch target; live call is in broker/workers.py

from Core.resource_monitor.broker.model_cache import (  # noqa: F401
    _ModelCache,
    _ModelCacheEntry,
    _model_cache,
)
from Core.resource_monitor.broker.policy import (  # noqa: F401
    _all_blockers_view,
    _grant,
    _insufficient,
    _signal_evict,
    _signal_soft_evict,
    _try_grant,
)
from Core.resource_monitor.broker.registry import (  # noqa: F401
    Lease,
    _init_nvml,
    _refresh_nvml,
    _registry,
)
from Core.resource_monitor.broker.service import (  # noqa: F401
    _reset_for_tests,
    install,
    stop,
)
from Core.resource_monitor.broker.workers import (  # noqa: F401
    _poll_ollama,
    _state_snapshot,
    _threads,
    _write_state,
)

__all__ = ["install", "stop"]
