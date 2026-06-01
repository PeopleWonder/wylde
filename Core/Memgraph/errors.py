# AUTO-GENERATED, edit core/shared/errors.py and run python core/shared/sync.py
"""
errors.py — Helpers for the Wylde 7-code error taxonomy.

Services that catch low-level exceptions (pipe disconnects, broken pipes, OS
errors, network timeouts) need a consistent way to map them onto the seven
codes from docs/protocols/ERROR_HANDLING.md §2:

    connection_refused  timeout  parse_error  auth_error
    resource_exhausted  internal_error  not_found

This module provides:

  - ERROR_CODES, ERROR_STATUS — the canonical taxonomy + HTTP status mapping.
  - classify(exc) — best-effort mapping from a raw Exception to one of the
    seven codes, with the original message preserved.
  - to_envelope(...) — build the canonical error reply envelope.
  - retry_with_backoff(fn) — exponential backoff decorator/helper that
    matches the §5 retry schedule (1, 2, 4, 8, 30s cap; 5 attempts).
  - log_error(...) — write a structured WARNING/ERROR log line in the
    format specified by §6.

Usage in a service handler:

    from errors import IpcError, classify

    try:
        result = do_work()
    except (BrokenPipeError, ConnectionResetError) as exc:
        # Surface as connection_refused so the caller's retry policy applies.
        raise IpcError("connection_refused", "downstream pipe disconnected",
                       details={"cause": str(exc)})
    except Exception as exc:
        code = classify(exc)
        raise IpcError(code, f"{type(exc).__name__}: {exc}")

Usage when you only have a Flask error handler:

    @app.errorhandler(Exception)
    def _on_error(exc):
        env = to_envelope(classify(exc), str(exc), {"type": type(exc).__name__})
        return env, env["status"]
"""

from __future__ import annotations

import errno
import logging
import socket
import time
from typing import Any, Callable, Dict, Optional, Tuple, Type, TypeVar

logger = logging.getLogger(__name__)

# ── Canonical taxonomy ────────────────────────────────────────────────
# Keys MUST match ERROR_HANDLING.md §2 exactly. Adding a new key here
# requires updating the protocol doc and the GUI's status mapping.
ERROR_STATUS: Dict[str, int] = {
    "connection_refused": 503,
    "timeout": 503,
    "parse_error": 422,
    "auth_error": 401,
    "resource_exhausted": 507,
    "internal_error": 500,
    "not_found": 404,
}

ERROR_CODES = tuple(ERROR_STATUS.keys())


class IpcError(Exception):
    """Re-export of the IpcError shape services raise from route handlers.

    Keeping the definition here as well as in ipc.py lets services that
    don't depend on the full IPC layer (e.g. a CLI tool) still raise the
    structured exception. ipc.serve()'s catch-all recognises both.
    """

    def __init__(
        self, code: str, message: str, details: Optional[Dict[str, Any]] = None
    ):
        if code not in ERROR_STATUS:
            # Fail loud during dev — protocol violation is an upstream bug.
            raise ValueError(
                f"IpcError: unknown code {code!r}; valid codes are {ERROR_CODES}"
            )
        self.code = code
        self.message = message
        self.details = details or {}
        super().__init__(f"{code}: {message}")


# ── Exception → code classification ────────────────────────────────────
# Concrete-type checks first; OSError errno fallback after; bare-Exception
# as the last-resort `internal_error`.
_TYPE_MAP: Tuple[Tuple[Type[BaseException], str], ...] = (
    (TimeoutError, "timeout"),
    (socket.timeout, "timeout"),
    (ConnectionRefusedError, "connection_refused"),
    (ConnectionAbortedError, "connection_refused"),
    (ConnectionResetError, "connection_refused"),
    (BrokenPipeError, "connection_refused"),
    (FileNotFoundError, "not_found"),
    (PermissionError, "auth_error"),
    (MemoryError, "resource_exhausted"),
    (ValueError, "parse_error"),
    (TypeError, "parse_error"),
)

_ERRNO_MAP: Dict[int, str] = {
    errno.ENOENT: "not_found",  # No such file/path/pipe
    errno.ECONNREFUSED: "connection_refused",
    errno.ECONNRESET: "connection_refused",
    errno.ECONNABORTED: "connection_refused",
    errno.EPIPE: "connection_refused",
    errno.ETIMEDOUT: "timeout",
    errno.EAGAIN: "timeout",
    errno.EACCES: "auth_error",
    errno.EPERM: "auth_error",
    errno.ENOMEM: "resource_exhausted",
    errno.ENOSPC: "resource_exhausted",
}


def classify(exc: BaseException) -> str:
    """Map an arbitrary exception to one of the seven canonical codes.

    Best-effort; never raises. Fall-through is `internal_error`, which
    matches the protocol's default for unexpected failure paths.
    """
    if isinstance(exc, IpcError):
        return exc.code

    for typ, code in _TYPE_MAP:
        if isinstance(exc, typ):
            return code

    # OSError subclasses with an errno that maps cleanly. pywintypes.error
    # also exposes .winerror but that's Windows-only; we keep the errno
    # fallback as the portable path.
    if isinstance(exc, OSError):
        en = getattr(exc, "errno", None)
        if en is not None and en in _ERRNO_MAP:
            return _ERRNO_MAP[en]

    return "internal_error"


# ── Envelope construction ──────────────────────────────────────────────
def to_envelope(
    code: str,
    message: str,
    details: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Build the canonical error reply envelope (ERROR_HANDLING.md §3).

    Returned dict has the shape JSON serialisers expect; pass to Flask
    `jsonify(env)` or include directly in a msgpack pipe reply.
    """
    if code not in ERROR_STATUS:
        # Don't 500 on a misuse — surface internal_error and log loudly.
        logger.error(
            "errors.to_envelope: invalid code %r; coercing to internal_error", code
        )
        code = "internal_error"
    return {
        "ok": False,
        "status": ERROR_STATUS[code],
        "error": {
            "code": code,
            "message": message,
            "details": details or {},
        },
    }


# ── Retry helper ───────────────────────────────────────────────────────
T = TypeVar("T")

# Per ERROR_HANDLING.md §5: 1, 2, 4, 8, 30 (cap), max 5 attempts.
_BACKOFF_SCHEDULE = (1.0, 2.0, 4.0, 8.0, 30.0)
_MAX_ATTEMPTS = len(_BACKOFF_SCHEDULE)
# Codes that warrant the persistent-failure schedule. Auth and not_found
# do NOT retry (deterministic failure); parse_error retries only when the
# caller knows the service is alive (handled out-of-band).
_RETRYABLE_CODES = frozenset({"connection_refused", "timeout", "resource_exhausted"})


def retry_with_backoff(
    fn: Callable[..., T],
    *args: Any,
    on_codes: Optional[frozenset] = None,
    max_attempts: int = _MAX_ATTEMPTS,
    sleep: Callable[[float], None] = time.sleep,
    **kwargs: Any,
) -> T:
    """Invoke `fn(*args, **kwargs)`. If it raises IpcError with a retryable
    code, sleep per the §5 schedule and retry up to `max_attempts` times.

    Non-IpcError exceptions are not retried — they bubble up. Use this only
    around well-defined call sites where the retryable failure mode is the
    one you actually want to retry.
    """
    codes = on_codes or _RETRYABLE_CODES
    attempt = 0
    while True:
        try:
            return fn(*args, **kwargs)
        except IpcError as exc:
            attempt += 1
            if exc.code not in codes or attempt >= max_attempts:
                raise
            delay = _BACKOFF_SCHEDULE[min(attempt - 1, len(_BACKOFF_SCHEDULE) - 1)]
            logger.info(
                "retry_with_backoff: %s (attempt %d/%d) — sleep %.1fs",
                exc.code,
                attempt,
                max_attempts,
                delay,
            )
            sleep(delay)


# ── Logging helper ─────────────────────────────────────────────────────
def log_error(
    service: str,
    code: str,
    message: str,
    details: Optional[Dict[str, Any]] = None,
    *,
    log: Optional[logging.Logger] = None,
) -> None:
    """Write a structured log line in the §6 format.

    `internal_error` logs at ERROR with stack trace; everything else at
    WARNING without a trace (per protocol). Caller is responsible for
    invoking inside an exception handler if a stack is wanted.
    """
    log = log or logger
    extra = f" {details}" if details else ""
    msg = f"[{service}] {code}: {message}{extra}"
    if code == "internal_error":
        log.error(msg, exc_info=True)
    else:
        log.warning(msg)


__all__ = [
    "IpcError",
    "ERROR_CODES",
    "ERROR_STATUS",
    "classify",
    "to_envelope",
    "retry_with_backoff",
    "log_error",
]
