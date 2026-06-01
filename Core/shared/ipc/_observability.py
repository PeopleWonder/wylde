"""Per-call observability — appends one JSON record per IPC round-trip."""

from __future__ import annotations

import json
import sys
import threading
import time
from typing import Any

from . import _wire as _w
from ._wire import Reply

_log_lock = threading.Lock()
_log_file = None


def _log_call(
    service: str, method: str, reply: Reply, bytes_in: int, bytes_out: int
) -> None:
    global _log_file
    try:
        with _log_lock:
            if _log_file is None:
                _w.LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
                _log_file = _w.LOG_PATH.open("a", buffering=1, encoding="utf-8")
            line = {
                "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "caller": _w._SELF_NAME,
                "callee": service,
                "method": method,
                "transport": reply.transport,
                "bytes_in": bytes_in,
                "bytes_out": bytes_out,
                "dur_ms": round(reply.duration_ms, 3),
                "ok": reply.ok,
            }
            if not reply.ok and reply.error:
                line["err_code"] = reply.error.get("code")
            _log_file.write(json.dumps(line) + "\n")
    except (OSError, ValueError, TypeError) as e:
        try:
            sys.stderr.write(f"[ipc] log_call fallback: {type(e).__name__}: {e}\n")
        except Exception:
            pass


def _size(x: Any) -> int:
    if x is None:
        return 0
    if isinstance(x, (bytes, bytearray)):
        return len(x)
    if isinstance(x, str):
        return len(x.encode("utf-8", errors="replace"))
    try:
        return len(json.dumps(x, default=str))
    except Exception:
        return 0
