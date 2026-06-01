"""captioner.py — unified captioner over Florence-2 (MIT), Qwen2.5-VL (Apache 2.0)
and JoyCaption (Llama 3.1 CL).

All three backends produce a single caption string per image. Florence is
faster and purpose-built for dense captions; Qwen follows instruction
prompts and can be steered via ``config.yaml:qwen.prompt``; JoyCaption is
slower / heavier but produces higher-quality LoRA-style captions.

Weights are loaded from the standard HuggingFace Hub cache (no
``cache_dir`` override). Set ``HF_HUB_CACHE`` / ``HUGGINGFACE_HUB_CACHE``
/ ``HF_HOME`` to relocate. This way the model_registry HF scanner picks
up the weights automatically and other Wylde services share them.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, List, Optional, Sequence, Union

from PIL import Image

from . import config as C

logger = logging.getLogger(__name__)

ImageLike = Union[str, Path, Image.Image]


def _load_image(src: ImageLike) -> Image.Image:
    if isinstance(src, Image.Image):
        img = src
    else:
        img = Image.open(str(src))
    if img.mode != "RGB":
        img = img.convert("RGB")
    return img


def _resolve_device_dtype() -> tuple[str, Any]:
    import torch

    if C.DEVICE:
        device = C.DEVICE
    else:
        device = "cuda" if torch.cuda.is_available() else "cpu"

    dtype_map = {
        "float16": torch.float16,
        "fp16": torch.float16,
        "half": torch.float16,
        "bfloat16": torch.bfloat16,
        "bf16": torch.bfloat16,
        "float32": torch.float32,
        "fp32": torch.float32,
        "auto": torch.float16 if device.startswith("cuda") else torch.float32,
    }
    dtype = dtype_map.get(C.DTYPE, torch.float16)
    if device == "cpu" and dtype == torch.float16:
        dtype = torch.float32  # fp16 on CPU is pointless and often unsupported
    return device, dtype


# Florence-2 backend
class FlorenceCaptioner:
    """Microsoft Florence-2 (MIT licence)."""

    def __init__(self, variant: str = "large"):
        from transformers import AutoModelForCausalLM, AutoProcessor
        import torch

        hf_id = C.FLORENCE_HF_IDS.get(variant)
        if not hf_id:
            raise ValueError(
                f"Unknown Florence variant '{variant}'. Options: {list(C.FLORENCE_HF_IDS)}"
            )

        self.device, self.dtype = _resolve_device_dtype()
        logger.info(
            "Loading Florence-2 (%s) on %s [%s]", hf_id, self.device, self.dtype
        )

        _loaded: Any = AutoModelForCausalLM.from_pretrained(
            hf_id,
            torch_dtype=self.dtype,
            trust_remote_code=True,
        )
        self.model = _loaded.to(self.device).eval()

        self.processor = AutoProcessor.from_pretrained(
            hf_id,
            trust_remote_code=True,
        )
        self.hf_id = hf_id
        self.torch = torch

    @property
    def model_id(self) -> str:
        return self.hf_id

    def caption_one(self, image: ImageLike, detail: str = "detailed") -> str:
        return self.caption_batch([image], detail=detail)[0]

    def caption_batch(
        self,
        images: Sequence[ImageLike],
        detail: str = "detailed",
    ) -> List[str]:
        if not images:
            return []

        task = C.FLORENCE_TASK_BY_DETAIL.get(detail.lower(), "<MORE_DETAILED_CAPTION>")
        pil_images = [_load_image(x) for x in images]

        inputs = self.processor(
            text=[task] * len(pil_images),
            images=pil_images,
            return_tensors="pt",
            padding=True,
        )
        inputs = {k: v.to(self.device) for k, v in inputs.items()}
        if "pixel_values" in inputs:
            inputs["pixel_values"] = inputs["pixel_values"].to(self.dtype)

        with self.torch.inference_mode():
            generated_ids = self.model.generate(
                input_ids=inputs["input_ids"],
                pixel_values=inputs["pixel_values"],
                max_new_tokens=1024,
                num_beams=3,
                do_sample=False,
            )

        results: List[str] = []
        for i, pil in enumerate(pil_images):
            text = self.processor.batch_decode(
                generated_ids[i : i + 1], skip_special_tokens=False
            )[0]
            parsed = self.processor.post_process_generation(
                text, task=task, image_size=(pil.width, pil.height)
            )
            caption = (
                parsed.get(task, "").strip()
                if isinstance(parsed, dict)
                else str(parsed).strip()
            )
            results.append(caption)
        return results


# Qwen2.5-VL backend
class QwenCaptioner:
    """Qwen2.5-VL-Instruct (Apache 2.0 licence)."""

    def __init__(self, variant: str = "3b"):
        from transformers import Qwen2_5_VLForConditionalGeneration, AutoProcessor
        import torch

        hf_id = C.QWEN_HF_IDS.get(variant)
        if not hf_id:
            raise ValueError(
                f"Unknown Qwen variant '{variant}'. Options: {list(C.QWEN_HF_IDS)}"
            )

        self.device, self.dtype = _resolve_device_dtype()
        logger.info("Loading %s on %s [%s]", hf_id, self.device, self.dtype)

        self.model = Qwen2_5_VLForConditionalGeneration.from_pretrained(
            hf_id,
            torch_dtype=self.dtype,
            device_map=self.device if self.device != "cpu" else None,
        ).eval()
        self.processor = AutoProcessor.from_pretrained(hf_id)
        self.hf_id = hf_id
        self.torch = torch

    @property
    def model_id(self) -> str:
        return self.hf_id

    def caption_one(self, image: ImageLike, detail: str = "detailed") -> str:
        return self.caption_batch([image], detail=detail)[0]

    def caption_batch(
        self,
        images: Sequence[ImageLike],
        detail: str = "detailed",
    ) -> List[str]:
        if not images:
            return []
        from qwen_vl_utils import process_vision_info

        prompt = C.QWEN_PROMPT
        pil_images = [_load_image(x) for x in images]

        messages_batch = []
        for pil in pil_images:
            messages_batch.append(
                [
                    {
                        "role": "user",
                        "content": [
                            {"type": "image", "image": pil},
                            {"type": "text", "text": prompt},
                        ],
                    }
                ]
            )

        texts = [
            self.processor.apply_chat_template(
                m, tokenize=False, add_generation_prompt=True
            )
            for m in messages_batch
        ]
        image_inputs, video_inputs = process_vision_info(messages_batch)

        inputs = self.processor(
            text=texts,
            images=image_inputs,
            videos=video_inputs,
            padding=True,
            return_tensors="pt",
        )
        inputs = inputs.to(self.device)

        with self.torch.inference_mode():
            generated_ids = self.model.generate(
                **inputs,
                max_new_tokens=C.QWEN_MAX_NEW_TOKENS,
                do_sample=False,
            )
        trimmed = [out[len(inp) :] for inp, out in zip(inputs.input_ids, generated_ids)]
        decoded = self.processor.batch_decode(
            trimmed,
            skip_special_tokens=True,
            clean_up_tokenization_spaces=True,
        )
        return [d.strip() for d in decoded]


# JoyCaption backend  (Llama 3.1 Community Licence. NOT fully permissive)
class JoyCaptioner:
    """fancyfeast/llama-joycaption-beta-one-hf-llava — LLaVA on Llama 3.1 8B.

    Licence note: inherits Llama 3.1 Community Licence. Use requires accepting
    Meta's terms (700M-MAU clause, acceptable-use policy). NOT Apache/MIT.
    """

    def __init__(self, load_in_4bit: Optional[bool] = None):
        from transformers import LlavaForConditionalGeneration, AutoProcessor
        import torch

        hf_id = C.JOYCAPTION_HF_ID
        self.device, self.dtype = _resolve_device_dtype()
        load_4bit = C.JOYCAPTION_LOAD_IN_4BIT if load_in_4bit is None else load_in_4bit

        # JoyCaption is an 8B Llama, ~16GB fp16 does NOT fit 16GB cards once the
        # vision tower + KV cache are loaded. Prefer bf16 over fp16 for numerical
        # stability on Llama-family weights.
        if self.dtype == torch.float16:
            self.dtype = torch.bfloat16

        logger.info(
            "Loading JoyCaption (%s) on %s [%s, 4bit=%s]",
            hf_id,
            self.device,
            self.dtype,
            load_4bit,
        )

        model_kwargs = {"torch_dtype": self.dtype}

        if load_4bit:
            try:
                from transformers import BitsAndBytesConfig

                # Vision tower + projector must stay in bf16 — they run f.multi_head_attention
                # which does not dispatch through bitsandbytes Params4bit. Only the LLM
                # sub-tree is actually quantized.
                model_kwargs["quantization_config"] = BitsAndBytesConfig(
                    load_in_4bit=True,
                    bnb_4bit_compute_dtype=torch.bfloat16,
                    bnb_4bit_quant_type="nf4",
                    bnb_4bit_use_double_quant=True,
                    llm_int8_skip_modules=["vision_tower", "multi_modal_projector"],
                )
                model_kwargs["device_map"] = "auto"
            except ImportError as e:
                raise RuntimeError(
                    "4-bit mode requires `bitsandbytes`. pip install bitsandbytes>=0.43.0"
                ) from e
        else:
            model_kwargs["device_map"] = self.device if self.device != "cpu" else None

        self.model = LlavaForConditionalGeneration.from_pretrained(
            hf_id,
            **model_kwargs,
        ).eval()
        self.processor = AutoProcessor.from_pretrained(hf_id)
        self.hf_id = hf_id
        self.torch = torch
        self.load_4bit = load_4bit

    @property
    def model_id(self) -> str:
        return self.hf_id

    def _build_prompt(self, detail: str) -> str:
        """Map Caption's detail levels onto JoyCaption prompt presets."""
        override = C.JOYCAPTION_PROMPT
        if override:
            return override
        length_map = {"brief": "short", "normal": "medium length", "detailed": "long"}
        length = length_map.get(detail.lower(), "long")
        style = C.JOYCAPTION_CAPTION_TYPE
        # Built from JoyCaption's own prompt recipes, tuned for LoRA captions.
        if style == "descriptive":
            return (
                f"Write a {length} descriptive caption for this image in a formal tone. "
                "Include the subject, appearance, clothing, pose, expression, setting, "
                "lighting, composition, and style. Do not add speculative or judgmental commentary."
            )
        if style == "stable_diffusion":
            return (
                "Write a Stable Diffusion prompt for this image. Use concise comma-separated "
                "descriptors covering subject, clothing, pose, setting, lighting, style."
            )
        if style == "booru":
            return "Write a list of Booru-like tags for this image, comma-separated."
        if style == "training":
            return (
                f"Write a {length} caption for this image suitable for training a "
                "text-to-image model. Be specific and descriptive; avoid speculation. "
                "Cover subject, clothing, pose, setting, lighting, style, camera angle."
            )
        return f"Write a {length} descriptive caption for this image."

    def caption_one(self, image: ImageLike, detail: str = "detailed") -> str:
        return self.caption_batch([image], detail=detail)[0]

    def caption_batch(
        self,
        images: Sequence[ImageLike],
        detail: str = "detailed",
    ) -> List[str]:
        if not images:
            return []

        prompt = self._build_prompt(detail)
        pil_images = [_load_image(x) for x in images]

        results: List[str] = []
        # JoyCaption (LLaVA) doesn't batch cleanly across differently-sized images
        # without careful padding; loop per-image for correctness.
        for pil in pil_images:
            convo = [
                {"role": "system", "content": "You are a helpful image captioner."},
                {"role": "user", "content": prompt},
            ]
            convo_text = self.processor.apply_chat_template(
                convo,
                tokenize=False,
                add_generation_prompt=True,
            )
            inputs = self.processor(
                text=[convo_text],
                images=[pil],
                return_tensors="pt",
            ).to(self.device)
            if "pixel_values" in inputs:
                inputs["pixel_values"] = inputs["pixel_values"].to(self.dtype)

            with self.torch.inference_mode():
                gen = self.model.generate(
                    **inputs,
                    max_new_tokens=C.JOYCAPTION_MAX_NEW_TOKENS,
                    do_sample=False,
                    suppress_tokens=None,
                )[0]
            gen = gen[inputs["input_ids"].shape[1] :]
            text = self.processor.tokenizer.decode(
                gen,
                skip_special_tokens=True,
                clean_up_tokenization_spaces=False,
            ).strip()
            results.append(text)
        return results


# Factory
def build_captioner(
    backend: Optional[str] = None,
    florence_variant: Optional[str] = None,
    qwen_variant: Optional[str] = None,
    joy_load_4bit: Optional[bool] = None,
) -> Any:
    b = (backend or C.BACKEND).lower()
    if b == "florence":
        return FlorenceCaptioner(variant=florence_variant or C.FLORENCE_VARIANT)
    if b == "qwen":
        return QwenCaptioner(variant=qwen_variant or C.QWEN_VARIANT)
    if b in ("joy", "joycaption"):
        return JoyCaptioner(load_in_4bit=joy_load_4bit)
    raise ValueError(f"Unknown backend '{b}'. Options: florence, qwen, joycaption")


def apply_trigger(caption: str, trigger: Optional[str]) -> str:
    trigger = (trigger or "").strip()
    if not trigger:
        return caption
    sep = " " if trigger.endswith((",", ".", ":")) else ", "
    return f"{trigger}{sep}{caption}".strip()
