"""caption_batch — thin tool wrapper around batch.caption_folder.

Validates parameters, calls into the in-process Caption module, returns
the standard tool envelope. Manifest declares ``requires_confirmation:
true`` because this writes a .txt next to every image in the target
folder; the runner enforces the gate.
"""

from __future__ import annotations

import os
from typing import Any, Dict


def run_caption_batch(params: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(params, dict):
        return {"error": "params must be an object"}

    folder = params.get("folder")
    if not folder or not isinstance(folder, str):
        return {"error": "folder is required (string)"}
    if not os.path.isdir(folder):
        return {"error": f"folder not found or not a directory: {folder}"}

    detail = (params.get("detail") or "detailed").lower()
    if detail not in ("brief", "normal", "detailed"):
        return {"error": f"detail must be brief|normal|detailed (got {detail!r})"}

    backend = params.get("backend") or None

    raw_exts = params.get("extensions")
    extensions = None
    if raw_exts is not None:
        if not isinstance(raw_exts, (list, tuple)):
            return {"error": "extensions must be an array of strings"}
        extensions = tuple(str(x).lower() for x in raw_exts)

    try:
        from Wylde.Trainer.Caption.run import get_captioner
        from Wylde.Trainer.Caption.batch import caption_folder
    except ImportError as exc:
        return {"error": f"Wylde.Trainer.Caption not importable: {exc}"}

    try:
        captioner = get_captioner(backend=backend)
    except Exception as exc:  # noqa: BLE001
        return {"error": f"captioner build failed: {exc}"}

    try:
        result = caption_folder(
            captioner,
            folder,
            detail=detail,
            trigger=params.get("trigger"),
            output_ext=params.get("output_ext"),
            overwrite=bool(params["overwrite"]) if "overwrite" in params else None,
            recursive=params.get("recursive"),
            extensions=extensions,
            batch_size=params.get("batch_size"),
            progress=False,
        )
    except Exception as exc:  # noqa: BLE001
        return {"error": f"batch captioning failed: {exc}"}

    return {
        "folder": folder,
        "total": result.total,
        "captioned": result.captioned,
        "skipped": result.skipped,
        "errors": result.errors,
        "errors_list": list(result.errors_list),
        "files_sample": list(result.files[:10]),
    }
