"""Unit tests for ``Wylde.Trainer.Caption.rust_worker``.

The worker is the inference engine the new ``wylde-trainer`` Rust crate
fronts. These tests verify the action handlers directly (no pipe, no
daemon) so they're fast and don't load the ~1.5 GB Florence-2 model:

* ``caption.health`` returns a structured ``model_loaded=false``
  envelope before any inference call.
* ``caption.list_backends`` returns the canonical three backends and
  the default from config.
* The wrapped handler converts captioner crashes into a structured
  ``{"error": ...}`` envelope instead of propagating exceptions.
* ``--check`` smoke verifies the module imports without booting the
  worker loop (so a dev box without torch can still gate on this).
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict


WYLDE_ROOT = Path(__file__).resolve().parents[2]
WORKER = WYLDE_ROOT / "Trainer" / "Caption" / "rust_worker.py"


def test_worker_check_smoke() -> None:
    """``--check`` imports the module and exits 0 without booting the loop."""
    env = os.environ.copy()
    env["PYTHONUNBUFFERED"] = "1"
    res = subprocess.run(
        [sys.executable, str(WORKER), "--check"],
        cwd=str(WYLDE_ROOT),
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert res.returncode == 0, f"--check failed: {res.stderr}"
    assert "import OK" in res.stdout


def test_health_action_does_not_load_model() -> None:
    """The health handler is a fast probe that must not touch the captioner."""
    from Trainer.Caption.rust_worker import _action_health

    result = _action_health({})
    assert isinstance(result, dict)
    assert result["model_loaded"] is False
    assert "backend" in result
    assert "device" in result
    assert "dtype" in result


def test_list_backends_returns_canonical_three() -> None:
    from Trainer.Caption.rust_worker import _action_list_backends

    result = _action_list_backends({})
    assert set(result["backends"]) == {"florence", "qwen", "joycaption"}
    # ``default`` mirrors CAPTION_BACKEND; empty string is allowed when
    # config import failed (e.g. yaml missing on a stripped checkout).
    assert "default" in result


def test_wrap_handler_catches_exceptions_into_envelope() -> None:
    """A handler that crashes should surface as ``{"error": "..."}``."""
    from Trainer.Caption.rust_worker import _wrap_handler

    def boom(_params: Dict[str, Any]) -> Dict[str, Any]:
        raise RuntimeError("simulated CUDA OOM")

    wrapped = _wrap_handler(boom)
    out = wrapped({})
    assert isinstance(out, dict)
    assert "error" in out
    assert "simulated CUDA OOM" in out["error"]
    assert "traceback" in out  # Wrap includes traceback for ops triage.


def test_wrap_handler_passes_clean_result_through() -> None:
    from Trainer.Caption.rust_worker import _wrap_handler

    def fine(params: Dict[str, Any]) -> Dict[str, Any]:
        return {"caption": "a smiling cat", "echo": params.get("x")}

    wrapped = _wrap_handler(fine)
    out = wrapped({"x": 42})
    assert out["caption"] == "a smiling cat"
    assert out["echo"] == 42
    assert "error" not in out


def test_wrap_handler_substitutes_empty_params_when_missing() -> None:
    """``params=None`` is normalised to ``{}`` so handlers don't have to."""
    from Trainer.Caption.rust_worker import _wrap_handler

    captured: Dict[str, Any] = {}

    def record(params: Dict[str, Any]) -> Dict[str, Any]:
        captured["params"] = params
        return {"ok": True}

    wrapped = _wrap_handler(record)
    wrapped(None)
    assert captured["params"] == {}


def test_actions_table_lists_the_five_caption_handlers() -> None:
    from Trainer.Caption.rust_worker import _ACTIONS

    expected = {
        "caption.health",
        "caption.list_backends",
        "caption.generate",
        "caption.generate_batch",
        "caption.generate_video",
    }
    assert set(_ACTIONS.keys()) == expected
    for name, handler in _ACTIONS.items():
        assert callable(handler), f"{name} handler is not callable"


def test_generate_without_image_path_surfaces_validation_error() -> None:
    """The generate handler delegates to the in-process tool, which
    returns an envelope with an ``error`` key when ``image_path`` is
    missing — confirms the worker doesn't accidentally bypass the
    existing validation layer."""
    from Trainer.Caption.rust_worker import _action_generate

    out = _action_generate({})
    assert isinstance(out, dict)
    assert "error" in out
    assert "image_path" in out["error"].lower()
