"""video.py — sample frames from a video and caption them.

Uses OpenCV for frame extraction to keep dependencies simple. Output modes:
    - "all"     : one caption per frame, joined with newlines
    - "first"   : caption first sampled frame
    - "middle"  : caption the centre frame
    - "summary" : caption every frame, dedupe near-duplicates, join with ", "
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, List, Optional, Sequence

import cv2
import numpy as np
from PIL import Image

from . import config as C
from .captioner import apply_trigger

logger = logging.getLogger(__name__)


@dataclass
class VideoResult:
    path: str = ""
    frames_sampled: int = 0
    caption: str = ""
    per_frame_captions: List[str] = field(default_factory=list)
    frames_written: List[str] = field(default_factory=list)


def _cv2_to_pil(frame_bgr: Any) -> Image.Image:
    rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
    return Image.fromarray(rgb)


def sample_frame_indices(
    total_frames: int,
    fps: float,
    mode: str,
    frame_count: int,
    target_fps: float,
    interval_s: float,
) -> List[int]:
    if total_frames <= 0:
        return []

    if mode == "count":
        n = max(1, min(frame_count, total_frames))
        if n == 1:
            return [total_frames // 2]
        return list(np.linspace(0, total_frames - 1, n, dtype=int))

    if mode == "fps":
        if target_fps <= 0 or fps <= 0:
            step = max(1, total_frames // max(1, frame_count))
        else:
            step = max(1, int(round(fps / target_fps)))
        return list(range(0, total_frames, step))

    if mode == "interval_s":
        if fps <= 0 or interval_s <= 0:
            step = max(1, total_frames // max(1, frame_count))
        else:
            step = max(1, int(round(fps * interval_s)))
        return list(range(0, total_frames, step))

    raise ValueError(f"Unknown sample_mode '{mode}'")


def extract_frames(
    video_path: str | Path,
    mode: Optional[str] = None,
    frame_count: Optional[int] = None,
    target_fps: Optional[float] = None,
    interval_s: Optional[float] = None,
) -> tuple[List[Image.Image], List[int], dict]:
    video_path = Path(video_path)
    if not video_path.exists():
        raise FileNotFoundError(video_path)

    cap = cv2.VideoCapture(str(video_path))
    if not cap.isOpened():
        raise RuntimeError(f"Could not open video: {video_path}")

    total = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))
    fps = float(cap.get(cv2.CAP_PROP_FPS))
    meta = {"total_frames": total, "fps": fps}

    indices = sample_frame_indices(
        total_frames=total,
        fps=fps,
        mode=mode or C.VIDEO_SAMPLE_MODE,
        frame_count=frame_count if frame_count is not None else C.VIDEO_FRAME_COUNT,
        target_fps=target_fps if target_fps is not None else C.VIDEO_TARGET_FPS,
        interval_s=interval_s if interval_s is not None else C.VIDEO_INTERVAL_S,
    )

    frames: List[Image.Image] = []
    taken_indices: List[int] = []
    for idx in indices:
        cap.set(cv2.CAP_PROP_POS_FRAMES, idx)
        ok, frame = cap.read()
        if not ok or frame is None:
            continue
        frames.append(_cv2_to_pil(frame))
        taken_indices.append(idx)
    cap.release()
    return frames, taken_indices, meta


def _dedupe_captions(captions: Sequence[str]) -> List[str]:
    """Merge near-identical consecutive captions (common on static scenes)."""
    out: List[str] = []
    for c in captions:
        c = (c or "").strip()
        if not c:
            continue
        if out and c.lower() == out[-1].lower():
            continue
        out.append(c)
    return out


def _aggregate(captions: Sequence[str], mode: str) -> str:
    captions = [c for c in captions if c]
    if not captions:
        return ""
    if mode == "first":
        return captions[0]
    if mode == "middle":
        return captions[len(captions) // 2]
    if mode == "all":
        return "\n".join(captions)
    if mode == "summary":
        return ", ".join(_dedupe_captions(captions))
    return " ".join(captions)


def caption_video(
    captioner: Any,
    video_path: str | Path,
    *,
    detail: Optional[str] = None,
    trigger: Optional[str] = None,
    output_ext: Optional[str] = None,
    overwrite: Optional[bool] = None,
    mode: Optional[str] = None,
    frame_count: Optional[int] = None,
    target_fps: Optional[float] = None,
    interval_s: Optional[float] = None,
    aggregate: Optional[str] = None,
    write_frames: Optional[bool] = None,
    frames_subdir: Optional[str] = None,
) -> VideoResult:
    detail = detail if detail is not None else C.DETAIL
    trigger = trigger if trigger is not None else C.TRIGGER
    output_ext = output_ext if output_ext is not None else C.OUTPUT_EXT
    overwrite = overwrite if overwrite is not None else C.OVERWRITE
    aggregate = aggregate if aggregate is not None else C.VIDEO_AGGREGATE
    write_frames = write_frames if write_frames is not None else C.VIDEO_WRITE_FRAMES
    frames_subdir = (
        frames_subdir if frames_subdir is not None else C.VIDEO_FRAMES_SUBDIR
    )

    video_path = Path(video_path)
    out_txt = video_path.with_suffix(output_ext)
    if out_txt.exists() and not overwrite:
        logger.info("Skip (exists): %s", out_txt)
        return VideoResult(
            path=str(video_path), caption=out_txt.read_text(encoding="utf-8")
        )

    frames, indices, meta = extract_frames(
        video_path,
        mode=mode,
        frame_count=frame_count,
        target_fps=target_fps,
        interval_s=interval_s,
    )
    if not frames:
        raise RuntimeError(f"No frames extracted from {video_path}")

    logger.info(
        "Extracted %d/%d frames from %s (fps=%.2f)",
        len(frames),
        meta.get("total_frames", 0),
        video_path.name,
        meta.get("fps", 0),
    )

    per_frame = captioner.caption_batch(frames, detail=detail)
    merged = _aggregate(per_frame, aggregate)
    final = apply_trigger(merged, trigger)

    out_txt.write_text(final, encoding="utf-8")

    frames_written: List[str] = []
    if write_frames:
        frames_dir = video_path.parent / frames_subdir / video_path.stem
        frames_dir.mkdir(parents=True, exist_ok=True)
        for frame, idx, cap_text in zip(frames, indices, per_frame):
            stem = f"{video_path.stem}_f{idx:06d}"
            img_path = frames_dir / f"{stem}.jpg"
            frame.save(img_path, quality=92)
            (frames_dir / f"{stem}{output_ext}").write_text(
                apply_trigger(cap_text, trigger),
                encoding="utf-8",
            )
            frames_written.append(str(img_path))

    return VideoResult(
        path=str(video_path),
        frames_sampled=len(frames),
        caption=final,
        per_frame_captions=list(per_frame),
        frames_written=frames_written,
    )


def caption_video_folder(
    captioner: Any,
    folder: str | Path,
    *,
    extensions: Optional[tuple] = None,
    recursive: Optional[bool] = None,
    **kwargs: Any,
) -> List[VideoResult]:
    folder = Path(folder)
    exts = tuple(e.lower() for e in (extensions or C.VIDEO_EXTENSIONS))
    recursive = recursive if recursive is not None else C.RECURSIVE
    it = folder.rglob("*") if recursive else folder.glob("*")
    videos = sorted(p for p in it if p.is_file() and p.suffix.lower() in exts)
    logger.info("Found %d videos in %s", len(videos), folder)

    results: List[VideoResult] = []
    for v in videos:
        try:
            results.append(caption_video(captioner, v, **kwargs))
        except Exception as e:  # noqa: BLE001
            logger.warning("Failed to caption %s: %s", v, e)
    return results
