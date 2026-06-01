"""tools/ollama/ — Ollama VRAM lifecycle.

preload / evict a single model, list what's loaded, or run an LRU sweep when
total VRAM exceeds a threshold. the Wylde user uses these to free VRAM before
launching ComfyUI, or to warm a model ahead of an inference burst.

Talks to Ollama over HTTP (the only HTTP this layer makes — Ollama is an
external system, not a Wylde component).
"""
