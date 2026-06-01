# Caption — image / video captioner for LoRA training datasets.
#
# In-process Florence-2 (MIT) by default; Qwen2.5-VL (Apache 2.0) and
# JoyCaption (Llama 3.1 CL, opt-in) backends are also supported via the
# same captioner factory.
#
# Public API:
#     from Wylde.Trainer.Caption.run import start_caption, stop_caption
#     from Wylde.Trainer.Caption.captioner import build_captioner, apply_trigger
#     from Wylde.Trainer.Caption.batch import caption_folder
#     from Wylde.Trainer.Caption.video import caption_video, caption_video_folder
