"""wylde-trainer-worker — Python pipe service for Florence-2 inference.

Spawned by the Lifecycle daemon when ``WYLDE_WYLDE_TRAINER_IMPL=rust``.
Hosts ``\\\\.\\pipe\\wylde-trainer-worker`` and is the place where the
Florence-2 model actually loads. The Rust ``wylde-trainer`` service
forwards every inference request here via ``wylde_shared::ipc::call``.

Why a sibling pipe service instead of a stdio subprocess of
``wylde-trainer``?  The ``no_external_process_spawn_rust`` lint rule
pins ``Command::new`` to ``wylde-lifecycle`` — supervising other
processes is the daemon's job, not a per-service responsibility. So the
worker is daemon-managed, and the trainer is a pipe-to-pipe forwarder.

Action surface (mirrors what the Rust trainer registers on
``\\\\.\\pipe\\wylde-trainer``):

* ``caption.health`` — fast probe, does NOT load Florence-2.
* ``caption.list_backends`` — static list of backends.
* ``caption.generate`` — single image; in-process call to
  :func:`Wylde.Trainer.Caption.tools.caption_image.caption_image.run_caption_image`.
* ``caption.generate_batch`` — folder walk; in-process call to
  :func:`Wylde.Trainer.Caption.tools.caption_batch.caption_batch.run_caption_batch`.
* ``caption.generate_video`` — video sampling; in-process call to
  :func:`Wylde.Trainer.Caption.tools.caption_video.caption_video.run_caption_video`.

Service owns its manifest: ``write_manifest`` at startup,
``start_heartbeat`` to keep ``status.heartbeat`` fresh, ``mark_stopped``
from the signal handler. Same shape as :mod:`Voice.run`.
"""

from __future__ import annotations

import argparse
import logging
import signal
import sys
import threading
import traceback
from types import FrameType
from typing import Any, Dict


SERVICE_NAME = "wylde-trainer-worker"
PIPE_LABEL = "\\\\.\\pipe\\wylde-trainer-worker"


# ── action handlers ──────────────────────────────────────────────────


def _action_health(_params: Dict[str, Any]) -> Dict[str, Any]:
    """Liveness + state — fast probe; does NOT load the model."""
    try:
        from Wylde.Trainer.Caption import config as C
        from Wylde.Trainer.Caption.run import _captioner as _state_captioner
    except ImportError as exc:
        return {
            "backend": "",
            "model_loaded": False,
            "model_id": "",
            "device": "unknown",
            "dtype": "",
            "import_error": str(exc),
        }
    with C.state_lock:
        return {
            "backend": C.service_state.get("backend", C.BACKEND),
            "model_loaded": bool(_state_captioner is not None),
            "model_id": C.service_state.get("model_id", ""),
            "device": C.service_state.get("device", "unknown"),
            "dtype": C.service_state.get("dtype", C.DTYPE),
            "total_captioned": C.service_state.get("total_captioned", 0),
        }


def _action_list_backends(_params: Dict[str, Any]) -> Dict[str, Any]:
    """Static list of backends compiled into the captioner factory."""
    try:
        from Wylde.Trainer.Caption import config as C

        default = C.BACKEND
    except ImportError:
        default = ""
    return {
        "backends": ["florence", "qwen", "joycaption"],
        "default": default,
    }


def _action_generate(params: Dict[str, Any]) -> Dict[str, Any]:
    """Caption a single image — mirrors tools/caption_image.run_caption_image."""
    from Wylde.Trainer.Caption.tools.caption_image.caption_image import (
        run_caption_image,
    )

    out: Dict[str, Any] = run_caption_image(params)
    return out


def _action_generate_batch(params: Dict[str, Any]) -> Dict[str, Any]:
    """Caption every image in a folder — mirrors tools/caption_batch.run_caption_batch."""
    from Wylde.Trainer.Caption.tools.caption_batch.caption_batch import (
        run_caption_batch,
    )

    out: Dict[str, Any] = run_caption_batch(params)
    return out


def _action_generate_video(params: Dict[str, Any]) -> Dict[str, Any]:
    """Caption a sampled video — mirrors tools/caption_video.run_caption_video."""
    from Wylde.Trainer.Caption.tools.caption_video.caption_video import (
        run_caption_video,
    )

    out: Dict[str, Any] = run_caption_video(params)
    return out


_ACTIONS = {
    "caption.health": _action_health,
    "caption.list_backends": _action_list_backends,
    "caption.generate": _action_generate,
    "caption.generate_batch": _action_generate_batch,
    "caption.generate_video": _action_generate_video,
}


def _wrap_handler(handler: Any) -> Any:
    """Catch handler exceptions and surface them as a structured envelope.

    The action handlers either return a result dict (success) or a dict
    with an ``"error"`` key (validation failure). Crashes from the
    captioner — CUDA OOM, missing weights, bad image — are caught here
    and converted to ``{"error": "..."}`` so the pipe call never hangs
    on an uncaught exception in the dispatcher thread.
    """

    def _wrapped(params: Any) -> Any:
        try:
            return handler(params or {})
        except Exception as exc:  # noqa: BLE001
            logger.exception("trainer_worker: handler raised")
            return {
                "error": f"{type(exc).__name__}: {exc}",
                "traceback": traceback.format_exc(limit=4),
            }

    return _wrapped


# ── service lifecycle ────────────────────────────────────────────────


logger = logging.getLogger("wylde.trainer.worker")

_shutdown_event = threading.Event()


def _install_signal_handlers() -> None:
    def _handler(signum: int, _frame: FrameType | None) -> None:
        from Core.shared.manifest import mark_stopped

        logger.info("trainer_worker: signal %s, shutting down", signum)
        try:
            mark_stopped(SERVICE_NAME)
        except Exception:  # noqa: BLE001
            logger.exception("trainer_worker: mark_stopped raised")
        _shutdown_event.set()

    for sig_name in ("SIGINT", "SIGTERM", "SIGBREAK"):
        sig = getattr(signal, sig_name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass


def _register_actions() -> Any:
    try:
        from Core.shared import ipc
    except ImportError as exc:
        logger.error("trainer_worker: Core.shared.ipc unavailable: %s", exc)
        return None
    for name, handler in _ACTIONS.items():
        ipc.register_action(name, _wrap_handler(handler))
    logger.info("trainer_worker: registered %d caption.* actions", len(_ACTIONS))
    return ipc


def _serve_forever() -> int:
    try:
        from Core.shared.logging_setup import configure_logging
    except ImportError:
        pass
    else:
        configure_logging(service=SERVICE_NAME)

    try:
        from Core.shared.manifest import (
            mark_stopped,
            start_heartbeat,
            write_manifest,
        )
    except ImportError as exc:
        logger.error("trainer_worker: Core.shared.manifest unavailable: %s", exc)
        return 1

    write_manifest(
        service_name=SERVICE_NAME,
        port=0,
        category="standard",
        description=(
            "Trainer worker — Python inference engine for wylde-trainer. "
            "Hosts " + PIPE_LABEL + " and loads Florence-2 lazily on the "
            "first caption.generate* call."
        ),
        contributes={
            "wylde_trainer_worker": {
                "actions": sorted(_ACTIONS.keys()),
            },
            "dashboard": {"label": "Trainer worker", "icon": "image", "color": "teal"},
        },
        entry_point="python:Trainer.Caption.rust_worker",
    )
    start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()

    ipc = _register_actions()
    if ipc is None:
        logger.error("trainer_worker: pipe failed to start (ipc unavailable)")
        mark_stopped(SERVICE_NAME)
        return 1

    try:
        ipc.serve_forever_background(SERVICE_NAME, None)
    except Exception as exc:  # noqa: BLE001
        logger.warning("trainer_worker: serve_forever_background failed (%s)", exc)
        mark_stopped(SERVICE_NAME)
        return 1

    logger.info("trainer_worker: serving %s", PIPE_LABEL)
    try:
        while not _shutdown_event.is_set():
            _shutdown_event.wait(timeout=1.0)
    except KeyboardInterrupt:
        pass
    mark_stopped(SERVICE_NAME)
    logger.info("trainer_worker: shutdown complete")
    return 0


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="rust_worker")
    parser.add_argument(
        "--check",
        action="store_true",
        help="Import the worker and exit (verifies wiring without serving).",
    )
    args = parser.parse_args(argv)

    if args.check:
        sys.stdout.write("Wylde.Trainer.Caption.rust_worker: import OK\n")
        return 0
    return _serve_forever()


if __name__ == "__main__":
    sys.exit(_main())
