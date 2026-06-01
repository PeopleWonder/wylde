"""Strangler-fig dispatch helpers for the daemon-managed services.

Services on the Rust port plan (vram_broker, device_gate, gateway) are
dispatched through :func:`_impl_for`: ``WYLDE_<SERVICE>_IMPL=rust``
picks a sibling Rust binary resolved by :func:`_rust_binary_path`,
default ``python`` keeps the existing in-tree implementation. Missing
or unparseable env vars fall back to Python with a warning, so a
mis-set deployment can never silently lose the service.
:func:`_spawn_rust_service` is the Rust-branch ``Popen`` wrapper.

Split out of :mod:`._services` so that file stays a single concern —
the per-service ``_start_X`` / ``_stop_X`` pairs — and under the
file-size cap. ``_services`` re-imports these three names, so callers
(and test code) that reach them as ``_services._impl_for`` etc. keep
resolving unchanged.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Literal, Optional

from .._common import WYLDE_ROOT, logger as _lc_logger


def _impl_for(
    service: str,
    default: Literal["python", "rust"] = "python",
) -> Literal["python", "rust"]:
    """Read ``WYLDE_<SERVICE>_IMPL`` for ``service``; ``default`` when unset.

    The service name ``wylde-vram-broker`` maps to env var
    ``WYLDE_WYLDE_VRAM_BROKER_IMPL`` — dashes become underscores,
    everything uppercased. Unrecognised values log a warning and fall
    back to ``default`` so a typo can't take a service offline.

    ``default`` exists for the per-service cutover defaults: VPN ships
    its default flipped to ``rust`` after Phase 2.E even though the
    Python implementation is still on disk as a rollback path.
    """
    var = f"WYLDE_{service.upper().replace('-', '_')}_IMPL"
    raw = os.environ.get(var)
    if raw is None:
        return default
    val = raw.lower()
    if val == "rust":
        return "rust"
    if val == "python":
        return "python"
    _lc_logger.warning(
        "daemon: %s=%r is not 'python' or 'rust'; falling back to %s",
        var,
        raw,
        default,
    )
    return default


def _rust_binary_path(service: str) -> Optional[Path]:
    """Resolve the Rust binary for ``service`` or return ``None``.

    Resolution order:
      1. ``WYLDE_<SERVICE>_BIN`` override (must point at an existing file).
      2. Bundled install path ``rust/bin/wylde-<stripped>.exe``.
      3. Cargo release target ``rust/target/release/wylde-<stripped>.exe``.
      4. Cargo debug target ``rust/target/debug/wylde-<stripped>.exe``.

    ``<stripped>`` is ``service`` with the ``wylde-`` prefix removed.
    On non-Windows hosts the ``.exe`` suffix is dropped.
    """
    stripped = service.removeprefix("wylde-")
    override_var = f"WYLDE_{service.upper().replace('-', '_')}_BIN"
    override = os.environ.get(override_var)
    if override:
        path = Path(override)
        return path if path.exists() else None

    suffix = ".exe" if sys.platform == "win32" else ""
    bin_name = f"wylde-{stripped}{suffix}"
    candidates = (
        WYLDE_ROOT / "rust" / "bin" / bin_name,
        WYLDE_ROOT / "rust" / "target" / "release" / bin_name,
        WYLDE_ROOT / "rust" / "target" / "debug" / bin_name,
    )
    for candidate in candidates:
        if candidate.exists():
            return candidate
    return None


def _spawn_rust_service(
    *,
    service: str,
    rust_bin: Path,
) -> Optional[subprocess.Popen[bytes]]:
    """Spawn ``rust_bin`` with the same Popen options as the Python branch.

    Returns the Popen handle or ``None`` if the spawn itself failed
    (logged via ``_lc_logger.exception``). Callers store the handle on
    the matching ``_ds._<service>_proc`` slot and call
    :func:`_ds._record_spawn` with ``impl="rust"``.
    """
    cmd = [str(rust_bin)]
    env = os.environ.copy()
    env["WYLDE_SERVICE_NAME"] = service
    env["WYLDE_ROOT"] = str(WYLDE_ROOT)
    env.setdefault("RUST_LOG", "info")

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        return subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: %s rust spawn failed (%s)", service, rust_bin)
        return None


__all__ = ["_impl_for", "_rust_binary_path", "_spawn_rust_service"]
