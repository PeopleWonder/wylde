#!/usr/bin/env python3
"""download_models.py — pre-fetch captioner weights into the HuggingFace Hub cache.

Run once on a new machine; otherwise the captioner downloads on first use.
Weights land in the standard HF cache (``~/.cache/huggingface/hub`` by
default, or wherever ``HF_HUB_CACHE`` / ``HUGGINGFACE_HUB_CACHE`` /
``HF_HOME`` points). This way model_registry's HF scanner picks them up
and any other Wylde service that needs Florence-2 / Qwen-VL shares the
same on-disk weights instead of duplicating gigabytes per service.

Usage::

    python download_models.py                    # Florence-2-large (default)
    python download_models.py --backend florence --variant base
    python download_models.py --backend qwen --variant 3b
    python download_models.py --all              # Florence-large + Qwen-3B
"""

from __future__ import annotations

import argparse
import logging
import os
import sys
from pathlib import Path

try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    configure_logging = None  # type: ignore[assignment]
if configure_logging is not None:
    configure_logging()
log = logging.getLogger("download_models")

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


def _resolve_hub_dir() -> Path:
    """Mirror model_registry/_hf_scanner._resolve_hub_dir for parity logging."""
    for env in ("HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"):
        v = os.getenv(env)
        if v:
            return Path(v).expanduser()
    hf_home = os.getenv("HF_HOME")
    if hf_home:
        return Path(hf_home).expanduser() / "hub"
    return Path.home() / ".cache" / "huggingface" / "hub"


def download_florence(variant: str = "large") -> bool:
    from transformers import AutoModelForCausalLM, AutoProcessor

    hf_id = FLORENCE_HF_IDS.get(variant)
    if not hf_id:
        log.error("Unknown Florence variant: %s", variant)
        return False
    log.info("Fetching %s (MIT)...", hf_id)
    try:
        AutoModelForCausalLM.from_pretrained(hf_id, trust_remote_code=True)
        AutoProcessor.from_pretrained(hf_id, trust_remote_code=True)
        log.info("  Done: %s", hf_id)
        return True
    except Exception as e:  # noqa: BLE001
        log.error("  Failed: %s", e)
        return False


def download_qwen(variant: str = "3b") -> bool:
    from transformers import Qwen2_5_VLForConditionalGeneration, AutoProcessor

    hf_id = QWEN_HF_IDS.get(variant)
    if not hf_id:
        log.error("Unknown Qwen variant: %s", variant)
        return False
    log.info("Fetching %s (Apache 2.0)...", hf_id)
    try:
        Qwen2_5_VLForConditionalGeneration.from_pretrained(hf_id)
        AutoProcessor.from_pretrained(hf_id)
        log.info("  Done: %s", hf_id)
        return True
    except Exception as e:  # noqa: BLE001
        log.error("  Failed: %s", e)
        return False


def download_joycaption() -> bool:
    from transformers import LlavaForConditionalGeneration, AutoProcessor

    log.warning(
        "JoyCaption is built on Llama 3.1 and inherits the Llama 3.1 Community "
        "Licence (700M-MAU clause, acceptable-use policy). NOT Apache/MIT. "
        "Proceeding per explicit user request."
    )
    log.info("Fetching %s (Llama 3.1 Community Licence)...", JOYCAPTION_HF_ID)
    try:
        # Download weights without instantiating on GPU (saves VRAM during setup).
        LlavaForConditionalGeneration.from_pretrained(
            JOYCAPTION_HF_ID, low_cpu_mem_usage=True
        )
        AutoProcessor.from_pretrained(JOYCAPTION_HF_ID)
        log.info("  Done: %s", JOYCAPTION_HF_ID)
        return True
    except Exception as e:  # noqa: BLE001
        log.error("  Failed: %s", e)
        return False


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument(
        "--backend",
        choices=["florence", "qwen", "joycaption"],
        default="florence",
    )
    p.add_argument(
        "--variant",
        default=None,
        help="florence: base|large|base-ft|large-ft   qwen: 3b|7b",
    )
    p.add_argument(
        "--all",
        action="store_true",
        help="Download Florence-2-large AND Qwen2.5-VL-3B (no JoyCaption)",
    )
    args = p.parse_args()

    log.info("HF Hub cache dir: %s", _resolve_hub_dir())

    ok = True
    if args.all:
        ok &= download_florence("large")
        ok &= download_qwen("3b")
    elif args.backend == "florence":
        ok &= download_florence(args.variant or "large")
    elif args.backend == "qwen":
        ok &= download_qwen(args.variant or "3b")
    elif args.backend == "joycaption":
        ok &= download_joycaption()

    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
