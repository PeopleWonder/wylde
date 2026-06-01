"""Caption sub-service entry point (Trainer/Caption) — in-process, no pipe.

Caption is a Trainer-owned image/video captioner backed by Florence-2
weights.  Unlike the other ``run.py`` files in the tree it does not
spawn a service or open a named pipe: it exposes a ``start_caption`` /
``stop_caption`` pair so the harness or test harness can drive its
lifecycle in-process, plus a ``__main__`` for module-level
verification.

Default behaviour is lazy: ``start_caption(eager=False)`` returns
immediately and the ~1.5 GB model is built on first ``get_captioner``
call.  ``eager=True`` builds and warms the captioner up front for
callers who can't absorb the multi-second first-call latency.
``stop_caption`` drops the cached captioner so its weights can be
freed (CUDA ``empty_cache`` is best-effort).  Run with ``--check`` to
verify import wiring without paying the model-load cost.
"""

from __future__ import annotations

import argparse
import logging
import sys
import threading
from typing import Any, Optional, Sequence

from . import config as C

logger = logging.getLogger(__name__)

_lock = threading.Lock()
_captioner = None  # type: Optional[object]


def get_captioner(
    backend: Optional[str] = None,
    florence_variant: Optional[str] = None,
    qwen_variant: Optional[str] = None,
    joy_load_4bit: Optional[bool] = None,
) -> Any:
    """Return the module-level captioner, building it on first call.

    Subsequent calls return the same instance regardless of arguments —
    swapping backends mid-process is rare enough to require ``stop_caption``
    + ``start_caption`` rather than a silent rebuild.
    """
    global _captioner
    with _lock:
        if _captioner is None:
            from .captioner import build_captioner

            _captioner = build_captioner(
                backend=backend,
                florence_variant=florence_variant,
                qwen_variant=qwen_variant,
                joy_load_4bit=joy_load_4bit,
            )
            with C.state_lock:
                C.service_state["model_loaded"] = True
                C.service_state["backend"] = (backend or C.BACKEND).lower()
                C.service_state["model_id"] = getattr(_captioner, "model_id", "")
                C.service_state["device"] = getattr(_captioner, "device", "unknown")
        return _captioner


def start_caption(
    *,
    eager: bool = False,
    backend: Optional[str] = None,
    florence_variant: Optional[str] = None,
    qwen_variant: Optional[str] = None,
    joy_load_4bit: Optional[bool] = None,
) -> Any:
    """Start the Caption module.

    Args:
        eager: If True, build the captioner now (loading the model into VRAM).
            If False (default), defer until the first ``get_captioner`` call.
        backend / florence_variant / qwen_variant / joy_load_4bit: Forwarded
            to ``build_captioner`` when eager=True (or when the captioner is
            ultimately built). All None falls through to ``config.yaml``.

    Returns the captioner instance when eager, else None.
    """
    logger.info(
        "[caption] starting (eager=%s, backend=%s)", eager, backend or C.BACKEND
    )
    if eager:
        return get_captioner(
            backend=backend,
            florence_variant=florence_variant,
            qwen_variant=qwen_variant,
            joy_load_4bit=joy_load_4bit,
        )
    return None


def stop_caption() -> None:
    """Release the module-level captioner so its weights can be freed."""
    global _captioner
    with _lock:
        if _captioner is None:
            return
        logger.info("[caption] stopping; releasing captioner")
        _captioner = None
        with C.state_lock:
            C.service_state["model_loaded"] = False
            C.service_state["model_id"] = ""

    # Best-effort CUDA cleanup. Wrapped — torch may not be importable in
    # extremely stripped environments and we don't want stop_caption to
    # raise.
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
    except Exception:  # noqa: BLE001
        pass


def _main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="caption.run")
    parser.add_argument(
        "--check",
        action="store_true",
        help="Import and exit (verifies module wiring without loading the model).",
    )
    parser.add_argument(
        "--eager",
        action="store_true",
        help="Build the captioner now (loads ~1.5GB of Florence-2 weights into VRAM).",
    )
    args = parser.parse_args(argv)

    if args.check:
        print("Wylde.Trainer.Caption.run: import OK")
        return 0

    start_caption(eager=args.eager)
    print("Wylde.Trainer.Caption: started (eager=%s)" % args.eager)
    return 0


if __name__ == "__main__":
    sys.exit(_main())
