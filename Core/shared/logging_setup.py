"""Single entry point for configuring root logging across Wylde processes.

Five entry points previously called ``logging.basicConfig`` with slightly
different formats; only the first call wins, so the others were silent
no-ops once a process imported any of them. This module replaces them with
one idempotent ``configure_logging()`` that:

* installs a default StreamHandler on the root logger if none exists yet,
* uses a consistent format (with an optional service-name prefix), and
* squelches the chatty third-party loggers (``urllib3`` / ``requests``).

Re-entrant by design — calling it twice is safe and a no-op on the second
call. Pass ``force=True`` to replace existing handlers (rare, mostly for
tests).
"""

from __future__ import annotations

import logging
from typing import Optional


def configure_logging(
    level: int = logging.INFO,
    service: Optional[str] = None,
    *,
    force: bool = False,
) -> None:
    """Configure the root logger once.

    ``service`` adds a ``[service]`` tag between the timestamp and the
    level so subprocesses can be distinguished in merged log output.
    """
    root = logging.getLogger()
    if root.handlers and not force:
        # Lazy-import keeps logging_setup importable even if manifest.py
        # is unavailable (test environments, library use).
        from Core.shared.manifest import attest_phase

        attest_phase("configure_logging")
        return
    if force:
        for h in list(root.handlers):
            root.removeHandler(h)
    if service:
        fmt = f"%(asctime)s [{service}] %(levelname)s %(name)s: %(message)s"
    else:
        fmt = "%(asctime)s %(levelname)s %(name)s: %(message)s"
    logging.basicConfig(level=level, format=fmt, force=force)
    logging.getLogger("urllib3").setLevel(logging.WARNING)
    logging.getLogger("requests").setLevel(logging.WARNING)
    from Core.shared.manifest import attest_phase

    attest_phase("configure_logging")


__all__ = ["configure_logging"]
