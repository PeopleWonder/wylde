"""config.py — constants and shared state for Caption.

Loads ``config.yaml`` next to this module (or whatever ``CAPTION_CONFIG``
points at). Service shell config (port/host) was dropped when Caption
moved to in-process — only captioner-relevant knobs remain.

Model weights live in the standard HuggingFace Hub cache
(``~/.cache/huggingface/hub`` by default, or ``HF_HUB_CACHE`` /
``HUGGINGFACE_HUB_CACHE`` / ``HF_HOME`` if set) so that model_registry's
HF scanner can discover them and so that other Wylde services share the
same weights instead of duplicating gigabytes per service.
"""

from __future__ import annotations

import logging
import os
import threading
from pathlib import Path

import yaml

SERVICE_DIR = Path(__file__).parent

_config_path_env = os.getenv("CAPTION_CONFIG")
_config_path = (
    Path(_config_path_env) if _config_path_env else SERVICE_DIR / "config.yaml"
)
if _config_path.is_file():
    with open(_config_path, encoding="utf-8") as f:
        _cfg = yaml.safe_load(f) or {}
else:
    _cfg = {}

# Logging — opt-in; the harness usually owns the root logger.
_log_level = os.getenv(
    "CAPTION_LOG_LEVEL", _cfg.get("logging", {}).get("level", "INFO")
)
try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    configure_logging = None  # type: ignore[assignment]
if configure_logging is not None:
    configure_logging(level=getattr(logging, _log_level.upper(), logging.INFO))
logger = logging.getLogger(__name__)

# Model
_m = _cfg.get("model", {})
BACKEND = os.getenv("CAPTION_BACKEND", _m.get("backend", "florence")).lower()
FLORENCE_VARIANT = os.getenv(
    "CAPTION_FLORENCE_VARIANT", _m.get("florence_variant", "large")
).lower()
QWEN_VARIANT = os.getenv("CAPTION_QWEN_VARIANT", _m.get("qwen_variant", "3b")).lower()
DTYPE = os.getenv("CAPTION_DTYPE", _m.get("dtype", "float16")).lower()
DEVICE = os.getenv("CAPTION_DEVICE", _m.get("device") or "")  # "" = auto

# HuggingFace model IDs
FLORENCE_HF_IDS = {
    "base": "microsoft/Florence-2-base",
    "large": "microsoft/Florence-2-large",
    "base-ft": "microsoft/Florence-2-base-ft",
    "large-ft": "microsoft/Florence-2-large-ft",
}
QWEN_HF_IDS = {
    "3b": "Qwen/Qwen2.5-VL-3B-Instruct",
    "7b": "Qwen/Qwen2.5-VL-7B-Instruct",
}
JOYCAPTION_HF_ID = "fancyfeast/llama-joycaption-beta-one-hf-llava"

# Caption
_c = _cfg.get("caption", {})
DETAIL = os.getenv("CAPTION_DETAIL", _c.get("detail", "detailed")).lower()
TRIGGER = os.getenv("CAPTION_TRIGGER", _c.get("trigger", ""))
OUTPUT_EXT = _c.get("output_ext", ".txt")
OVERWRITE = bool(_c.get("overwrite", False))

FLORENCE_TASK_BY_DETAIL = {
    "brief": "<CAPTION>",
    "normal": "<DETAILED_CAPTION>",
    "detailed": "<MORE_DETAILED_CAPTION>",
}

# Batch
_b = _cfg.get("batch", {})
IMAGE_EXTENSIONS = tuple(
    e.lower()
    for e in _b.get("image_extensions", [".jpg", ".jpeg", ".png", ".webp", ".bmp"])
)
RECURSIVE = bool(_b.get("recursive", True))
BATCH_SIZE = int(_b.get("batch_size", 4))

# Video
_v = _cfg.get("video", {})
VIDEO_EXTENSIONS = tuple(
    e.lower()
    for e in _v.get("video_extensions", [".mp4", ".mov", ".mkv", ".webm", ".avi"])
)
VIDEO_SAMPLE_MODE = _v.get("sample_mode", "count")
VIDEO_FRAME_COUNT = int(_v.get("frame_count", 8))
VIDEO_TARGET_FPS = float(_v.get("target_fps", 1.0))
VIDEO_INTERVAL_S = float(_v.get("interval_s", 2.0))
VIDEO_AGGREGATE = _v.get("aggregate", "summary")
VIDEO_WRITE_FRAMES = bool(_v.get("write_frames", False))
VIDEO_FRAMES_SUBDIR = _v.get("frames_subdir", "frames")

# Qwen
_q = _cfg.get("qwen", {})
QWEN_PROMPT = _q.get(
    "prompt",
    "Describe this image in detail for a text-to-image training caption.",
)
QWEN_MAX_NEW_TOKENS = int(_q.get("max_new_tokens", 256))

# JoyCaption (Llama 3.1 Community Licence — opt-in)
_j = _cfg.get("joycaption", {})
JOYCAPTION_CAPTION_TYPE = os.getenv(
    "CAPTION_JOY_TYPE", _j.get("caption_type", "training")
).lower()
JOYCAPTION_PROMPT = os.getenv("CAPTION_JOY_PROMPT", _j.get("prompt", "") or "")
JOYCAPTION_LOAD_IN_4BIT = os.getenv(
    "CAPTION_JOY_4BIT", str(_j.get("load_in_4bit", True))
).lower() in ("1", "true", "yes", "on")
JOYCAPTION_MAX_NEW_TOKENS = int(_j.get("max_new_tokens", 320))

# Shared state — used by run.py to track the lazily-loaded captioner.
state_lock = threading.Lock()
service_state = {
    "model_loaded": False,
    "backend": BACKEND,
    "model_id": "",
    "device": "unknown",
    "dtype": DTYPE,
    "total_captioned": 0,
    "last_error": "",
}
