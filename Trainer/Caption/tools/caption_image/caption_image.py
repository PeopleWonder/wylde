"""caption_image — thin tool wrapper around the in-process Caption module.

Validates parameters, calls into ``Wylde.Trainer.Caption`` directly (no
HTTP loopback — Caption is in-process), returns the standard tool envelope.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict


def run_caption_image(params: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(params, dict):
        return {"error": "params must be an object"}

    image_path = params.get("image_path")
    if not image_path or not isinstance(image_path, str):
        return {"error": "image_path is required (string)"}
    if not os.path.isfile(image_path):
        return {"error": f"image_path not found: {image_path}"}

    detail = (params.get("detail") or "detailed").lower()
    if detail not in ("brief", "normal", "detailed"):
        return {"error": f"detail must be brief|normal|detailed (got {detail!r})"}

    trigger = params.get("trigger") or ""
    backend = params.get("backend") or None
    write_txt = bool(params.get("write_txt", False))
    overwrite = bool(params.get("overwrite", False))

    try:
        from Wylde.Trainer.Caption.run import get_captioner
        from Wylde.Trainer.Caption.captioner import apply_trigger
    except ImportError as exc:
        return {"error": f"Wylde.Trainer.Caption not importable: {exc}"}

    try:
        captioner = get_captioner(backend=backend)
        raw = captioner.caption_one(image_path, detail=detail)
        caption = apply_trigger(raw, trigger)
    except Exception as exc:  # noqa: BLE001
        return {"error": f"captioning failed: {exc}"}

    out: Dict[str, Any] = {
        "image_path": image_path,
        "caption": caption,
        "backend": backend or getattr(captioner, "model_id", ""),
    }

    if write_txt:
        sidecar = Path(image_path).with_suffix(".txt")
        if sidecar.exists() and not overwrite:
            out["txt_path"] = str(sidecar)
            out["written"] = False
        else:
            try:
                sidecar.write_text(caption, encoding="utf-8")
                out["txt_path"] = str(sidecar)
                out["written"] = True
            except OSError as exc:
                out["txt_error"] = str(exc)
                out["written"] = False

    return out
