"""caption_video — thin tool wrapper around video.caption_video.

Validates parameters, calls into the in-process Caption module, returns
the standard tool envelope.
"""

from __future__ import annotations

import os
from typing import Any, Dict


def run_caption_video(params: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(params, dict):
        return {"error": "params must be an object"}

    video_path = params.get("video_path")
    if not video_path or not isinstance(video_path, str):
        return {"error": "video_path is required (string)"}
    if not os.path.isfile(video_path):
        return {"error": f"video_path not found: {video_path}"}

    detail = (params.get("detail") or "detailed").lower()
    if detail not in ("brief", "normal", "detailed"):
        return {"error": f"detail must be brief|normal|detailed (got {detail!r})"}

    backend = params.get("backend") or None
    mode = params.get("mode")
    if mode is not None and mode not in ("count", "fps", "interval_s"):
        return {"error": f"mode must be count|fps|interval_s (got {mode!r})"}

    aggregate = params.get("aggregate")
    if aggregate is not None and aggregate not in ("all", "first", "middle", "summary"):
        return {
            "error": f"aggregate must be all|first|middle|summary (got {aggregate!r})"
        }

    write_txt = bool(params.get("write_txt", False))
    # The underlying caption_video always writes a sidecar — if the caller
    # doesn't want one we run with overwrite=False and a temp output_ext
    # via a tweak below. Simpler: respect write_txt by suppressing the
    # write when False using a no-op output_ext that the user is unlikely
    # to ever encounter on disk.
    overwrite = bool(params.get("overwrite", False))

    try:
        from Wylde.Trainer.Caption.run import get_captioner
        from Wylde.Trainer.Caption.video import caption_video as _caption_video
    except ImportError as exc:
        return {"error": f"Wylde.Trainer.Caption not importable: {exc}"}

    try:
        captioner = get_captioner(backend=backend)
    except Exception as exc:  # noqa: BLE001
        return {"error": f"captioner build failed: {exc}"}

    kwargs: Dict[str, Any] = {
        "detail": detail,
        "trigger": params.get("trigger"),
        "overwrite": overwrite,
        "mode": mode,
        "frame_count": params.get("frame_count"),
        "target_fps": params.get("target_fps"),
        "interval_s": params.get("interval_s"),
        "aggregate": aggregate,
        "write_frames": params.get("write_frames"),
    }
    if not write_txt:
        # Use an extension the user is unlikely to collide with so we don't
        # overwrite the user's .txt unintentionally. The caller already
        # opted out of sidecars; we just need somewhere to put the file
        # the underlying API insists on writing.
        kwargs["output_ext"] = ".caption.tmp"

    try:
        result = _caption_video(captioner, video_path, **kwargs)
    except Exception as exc:  # noqa: BLE001
        return {"error": f"video captioning failed: {exc}"}

    out: Dict[str, Any] = {
        "video_path": result.path,
        "frames_sampled": result.frames_sampled,
        "caption": result.caption,
        "per_frame_captions": list(result.per_frame_captions),
    }
    if result.frames_written:
        out["frames_written"] = list(result.frames_written)
    if not write_txt:
        # Tidy up the sentinel file we caused the underlying writer to drop.
        try:
            from pathlib import Path

            tmp = Path(video_path).with_suffix(".caption.tmp")
            if tmp.exists():
                tmp.unlink()
        except OSError:
            pass
    return out
