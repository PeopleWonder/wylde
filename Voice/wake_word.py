"""Wake-word detection — stub for this pass.

The real detector loads a small wake-word model and listens to the
mic in a low-power loop. On detection it fires the same trigger
``voice.start_session`` would. The model file is fetched via
Gateway's ``/api/models/pull`` — same path the LLM model pull uses —
gated behind a GUI-side trust dialog that confirms the user wants
to pull from external sources.

For this build we ship the supervisory framing — a class that can
``start()`` / ``stop()`` and reports whether a model is installed —
without the inference engine. The orchestrator keeps the detector
around when ``mode == always_on``; tests inject a fake that triggers
on demand.

Looking up "is the model installed" goes through the harness model
registry — Voice doesn't know where on disk the model file lives.
That's the architectural rule: Voice asks the harness, harness
answers from its own registry.
"""

from __future__ import annotations

import logging
import threading
from typing import Callable, Optional

logger = logging.getLogger("wylde.voice.wake_word")


class WakeWordDetector:
    """Background loop that calls ``on_detect`` when the wake word
    fires. Stub implementation — the real one will load the model the
    harness registry resolved.

    The orchestrator constructs one when always-on mode flips on, and
    calls ``stop()`` when it flips off.
    """

    def __init__(self, model_name: str, *, on_detect: Callable[[], None]) -> None:
        self.model_name = model_name
        self._on_detect = on_detect
        self._thread: Optional[threading.Thread] = None
        self._stop_event = threading.Event()
        self._loaded = False

    @property
    def loaded(self) -> bool:
        return self._loaded

    def start(self) -> bool:
        """Spin up the detector thread. Returns False if the model
        couldn't be loaded — caller should keep the service idle and
        prompt the user to install it."""
        if self._thread is not None and self._thread.is_alive():
            return True
        if not self._load_model():
            return False
        self._stop_event.clear()
        self._thread = threading.Thread(
            target=self._loop,
            name=f"wylde-wake-word-{self.model_name.replace('/', '_')}",
            daemon=True,
        )
        self._thread.start()
        logger.info("wake_word: detector started (model=%s)", self.model_name)
        return True

    def stop(self) -> None:
        self._stop_event.set()
        self._thread = None
        logger.info("wake_word: detector stopped")

    def _load_model(self) -> bool:
        # Stub: real implementation looks up the model via the harness
        # model registry, ensures it's downloaded, instantiates the
        # inference engine. For now we say "loaded" if the caller
        # claims the model exists; the orchestrator is responsible for
        # checking installed-status before constructing the detector.
        self._loaded = True
        return True

    def _loop(self) -> None:
        # Stub: in production this reads from the mic in chunks, runs
        # the wake-word inference, and calls self._on_detect() on hits.
        # The stub just sleeps until stopped — never fires.
        while not self._stop_event.is_set():
            self._stop_event.wait(timeout=1.0)


def is_model_installed(model_name: str) -> bool:
    """Ask the harness model registry whether the wake-word model is
    available. Returns False on any failure — Voice prefers a
    conservative "not installed" over a stuck affirmative.

    The harness exposes ``models.list`` with a ``kind`` filter; we
    look for an entry whose id matches ``model_name``. Wake-word
    models live under ``kind="stt"`` for now (separate kind would
    require a registry change the Wylde user flagged but didn't ask us to do
    in this pass).
    """
    try:
        from Core.harness.model_registry import get_model
    except ImportError:
        return False
    try:
        entry = get_model(model_name)
    except Exception:  # noqa: BLE001
        return False
    return entry is not None


def initiate_pull(model_name: str) -> str:
    """Kick off an outbound model pull via Gateway's ``/api/models/pull``
    SSE route and return a ``job_id`` the GUI can poll.

    Implementation is stubbed: the real flow makes a non-blocking
    request to ``http://127.0.0.1:<gateway>/api/models/pull`` and
    returns the upstream NDJSON-stream id. For this pass we just
    return a UUID — the GUI's "pulling…" state can hang on it until
    the harness reports the model is registered.
    """
    import uuid

    job_id = uuid.uuid4().hex[:12]
    logger.info("wake_word: pull initiated for %s (job=%s) — stub", model_name, job_id)
    return job_id


__all__ = [
    "WakeWordDetector",
    "is_model_installed",
    "initiate_pull",
]
