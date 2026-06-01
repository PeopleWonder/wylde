# Wylde resource_monitor

GPU VRAM lease broker — priority-based admission control across every
service that competes for VRAM (Ollama, Voice, Caption, RAG, Trainer).

- Pipe: `\\.\pipe\wylde-vram-broker`
- Run: `python run.py` (the Lifecycle daemon spawns this as the fourth
  Core constituent)
- Install: `pip install -r requirements.txt`
