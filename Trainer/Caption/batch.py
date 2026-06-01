"""batch.py — walk a folder, caption each image, write caption .txt file."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, List, Optional

from tqdm import tqdm

from . import config as C
from .captioner import apply_trigger

logger = logging.getLogger(__name__)


@dataclass
class BatchResult:
    total: int = 0
    captioned: int = 0
    skipped: int = 0
    errors: int = 0
    files: List[str] = field(default_factory=list)
    errors_list: List[dict] = field(default_factory=list)


def find_images(
    folder: Path,
    extensions: Optional[tuple] = None,
    recursive: bool = True,
) -> List[Path]:
    folder = Path(folder)
    exts = tuple(e.lower() for e in (extensions or C.IMAGE_EXTENSIONS))
    if not folder.exists():
        raise FileNotFoundError(f"Folder not found: {folder}")
    if not folder.is_dir():
        raise NotADirectoryError(f"Not a directory: {folder}")

    it = folder.rglob("*") if recursive else folder.glob("*")
    return sorted(p for p in it if p.is_file() and p.suffix.lower() in exts)


def caption_file(
    captioner: Any,
    image_path: Path,
    detail: str,
    trigger: str,
    output_ext: str,
    overwrite: bool,
) -> Optional[str]:
    out_path = image_path.with_suffix(output_ext)
    if out_path.exists() and not overwrite:
        return None
    raw = captioner.caption_one(image_path, detail=detail)
    text = apply_trigger(raw, trigger)
    out_path.write_text(text, encoding="utf-8")
    return text


def caption_folder(
    captioner: Any,
    folder: str | Path,
    *,
    detail: Optional[str] = None,
    trigger: Optional[str] = None,
    output_ext: Optional[str] = None,
    overwrite: Optional[bool] = None,
    recursive: Optional[bool] = None,
    extensions: Optional[tuple] = None,
    batch_size: Optional[int] = None,
    progress: bool = True,
    on_result: Optional[Callable[[Path, str], None]] = None,
) -> BatchResult:
    detail = detail if detail is not None else C.DETAIL
    trigger = trigger if trigger is not None else C.TRIGGER
    output_ext = output_ext if output_ext is not None else C.OUTPUT_EXT
    overwrite = overwrite if overwrite is not None else C.OVERWRITE
    recursive = recursive if recursive is not None else C.RECURSIVE
    batch_size = max(1, int(batch_size if batch_size is not None else C.BATCH_SIZE))

    folder = Path(folder)
    images = find_images(folder, extensions=extensions, recursive=recursive)
    result = BatchResult(total=len(images))

    pending: List[Path] = []
    for p in images:
        out_path = p.with_suffix(output_ext)
        if out_path.exists() and not overwrite:
            result.skipped += 1
            continue
        pending.append(p)

    if not pending:
        logger.info(
            "Nothing to caption (all %d files already have captions).", len(images)
        )
        return result

    logger.info(
        "Captioning %d / %d images in %s (batch=%d, detail=%s, trigger=%r)",
        len(pending),
        len(images),
        folder,
        batch_size,
        detail,
        trigger,
    )

    iterator = range(0, len(pending), batch_size)
    bar = tqdm(iterator, total=len(iterator), disable=not progress, unit="batch")
    for start in bar:
        chunk = pending[start : start + batch_size]
        try:
            captions = captioner.caption_batch(chunk, detail=detail)
        except Exception as e:  # noqa: BLE001
            logger.warning(
                "Batch failed (%d imgs): %s — falling back to single.", len(chunk), e
            )
            captions = []
            for img in chunk:
                try:
                    captions.append(captioner.caption_one(img, detail=detail))
                except Exception as inner:  # noqa: BLE001
                    result.errors += 1
                    result.errors_list.append({"path": str(img), "error": str(inner)})
                    captions.append(None)

        for img, raw in zip(chunk, captions):
            if raw is None:
                continue
            text = apply_trigger(raw, trigger)
            out_path = img.with_suffix(output_ext)
            try:
                out_path.write_text(text, encoding="utf-8")
                result.captioned += 1
                result.files.append(str(out_path))
                if on_result:
                    on_result(img, text)
            except OSError as e:
                result.errors += 1
                result.errors_list.append({"path": str(out_path), "error": str(e)})

    logger.info(
        "Done. captioned=%d skipped=%d errors=%d total=%d",
        result.captioned,
        result.skipped,
        result.errors,
        result.total,
    )
    return result
