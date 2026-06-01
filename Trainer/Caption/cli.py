"""cli.py — command-line entry point for humans.

LLM-callable tools live under ``Wylde/Trainer/Caption/tools/``; the CLI
here is for direct human use of the captioner module.

Usage
-----
    # Caption every image in a folder (writes .txt alongside each)
    python -m Wylde.Trainer.Caption.cli /path/to/dataset

    # With SDXL-style trigger word
    python -m Wylde.Trainer.Caption.cli C:\\lora\\mysubject --trigger "ohwx woman"

    # Detailed (paragraph) captions with Qwen2.5-VL instead of Florence
    python -m Wylde.Trainer.Caption.cli C:\\lora\\style --backend qwen --detail detailed

    # Caption a single video (samples frames and merges captions)
    python -m Wylde.Trainer.Caption.cli --video C:\\wan_training\\clip.mp4

    # Caption every video in a folder
    python -m Wylde.Trainer.Caption.cli --videos C:\\wan_training

    # Overwrite existing .txt files
    python -m Wylde.Trainer.Caption.cli /path --overwrite
"""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path
from typing import Optional, Sequence

from . import config as C
from .batch import caption_folder, caption_file, find_images  # noqa: F401
from .captioner import build_captioner
from .video import caption_video, caption_video_folder

logger = logging.getLogger("Wylde.Trainer.Caption.cli")


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="caption",
        description="Batch captioner for LoRA training datasets (Florence-2 / Qwen2.5-VL / JoyCaption).",
    )
    p.add_argument("path", nargs="?", help="Folder of images OR single image file")
    p.add_argument("--video", help="Caption a single video file")
    p.add_argument("--videos", help="Caption every video in this folder")

    # Model
    p.add_argument(
        "--backend",
        choices=["florence", "qwen", "joycaption"],
        default=None,
        help="florence (MIT, default) | qwen (Apache 2.0) | "
        "joycaption (Llama 3.1 Community Licence, opt-in, higher quality)",
    )
    p.add_argument("--florence-variant", choices=list(C.FLORENCE_HF_IDS), default=None)
    p.add_argument("--qwen-variant", choices=list(C.QWEN_HF_IDS), default=None)
    p.add_argument(
        "--joy-4bit",
        dest="joy_4bit",
        action="store_true",
        default=None,
        help="Force JoyCaption 4-bit nf4 quantization (needs bitsandbytes)",
    )
    p.add_argument(
        "--joy-fp",
        dest="joy_4bit",
        action="store_false",
        help="Disable 4-bit — load JoyCaption in bf16 (needs 24GB+ VRAM)",
    )

    # Caption
    p.add_argument(
        "--detail",
        choices=["brief", "normal", "detailed"],
        default=None,
        help="Caption length (Florence only; Qwen uses its prompt).",
    )
    p.add_argument(
        "--trigger", default=None, help="Trigger word prefix for the caption."
    )
    p.add_argument(
        "--output-ext", default=None, help="Caption file extension (default .txt)"
    )
    p.add_argument(
        "--overwrite", action="store_true", help="Overwrite existing caption files"
    )
    p.add_argument(
        "--no-recursive",
        dest="recursive",
        action="store_false",
        default=None,
        help="Do NOT recurse into subfolders",
    )
    p.add_argument(
        "--extensions", help="Comma-separated image extensions, e.g. .jpg,.png"
    )
    p.add_argument(
        "--batch-size", type=int, default=None, help="Images per forward pass"
    )

    # Video
    p.add_argument("--video-mode", choices=["count", "fps", "interval_s"], default=None)
    p.add_argument("--frame-count", type=int, default=None)
    p.add_argument("--target-fps", type=float, default=None)
    p.add_argument("--interval-s", type=float, default=None)
    p.add_argument(
        "--aggregate", choices=["all", "first", "middle", "summary"], default=None
    )
    p.add_argument("--write-frames", action="store_true", default=None)

    p.add_argument("-v", "--verbose", action="store_true")
    return p


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    from Core.shared.logging_setup import configure_logging

    configure_logging(level=logging.DEBUG if args.verbose else logging.INFO)

    if not any([args.path, args.video, args.videos]):
        print("error: supply a path, --video, or --videos", file=sys.stderr)
        return 2

    captioner = build_captioner(
        backend=args.backend,
        florence_variant=args.florence_variant,
        qwen_variant=args.qwen_variant,
        joy_load_4bit=args.joy_4bit,
    )

    exts = None
    if args.extensions:
        exts = tuple(e.strip().lower() for e in args.extensions.split(",") if e.strip())

    # Video single
    if args.video:
        r = caption_video(
            captioner,
            args.video,
            detail=args.detail,
            trigger=args.trigger,
            output_ext=args.output_ext,
            overwrite=args.overwrite or None,
            mode=args.video_mode,
            frame_count=args.frame_count,
            target_fps=args.target_fps,
            interval_s=args.interval_s,
            aggregate=args.aggregate,
            write_frames=args.write_frames,
        )
        print(f"[video] {r.path}")
        print(f"  frames={r.frames_sampled}")
        print(f"  caption: {r.caption}")
        return 0

    # Video folder
    if args.videos:
        rs = caption_video_folder(
            captioner,
            args.videos,
            recursive=args.recursive,
            detail=args.detail,
            trigger=args.trigger,
            output_ext=args.output_ext,
            overwrite=args.overwrite or None,
            mode=args.video_mode,
            frame_count=args.frame_count,
            target_fps=args.target_fps,
            interval_s=args.interval_s,
            aggregate=args.aggregate,
            write_frames=args.write_frames,
        )
        print(f"Captioned {len(rs)} videos in {args.videos}")
        return 0

    # Image path (file or folder)
    path = Path(args.path)
    if path.is_file():
        text = caption_file(
            captioner,
            path,
            detail=args.detail if args.detail is not None else C.DETAIL,
            trigger=args.trigger if args.trigger is not None else C.TRIGGER,
            output_ext=args.output_ext if args.output_ext is not None else C.OUTPUT_EXT,
            overwrite=bool(args.overwrite),
        )
        if text is None:
            print(f"[skip] {path} (caption exists, use --overwrite)")
        else:
            print(f"[image] {path}")
            print(f"  caption: {text}")
        return 0

    # Folder
    result = caption_folder(
        captioner,
        path,
        detail=args.detail,
        trigger=args.trigger,
        output_ext=args.output_ext,
        overwrite=args.overwrite or None,
        recursive=args.recursive,
        extensions=exts,
        batch_size=args.batch_size,
        progress=True,
    )
    print(
        f"\nDone. captioned={result.captioned} "
        f"skipped={result.skipped} errors={result.errors} total={result.total}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
