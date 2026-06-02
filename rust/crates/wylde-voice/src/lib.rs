//! Voice service — STT / TTS / wake-word primitives over `\\.\pipe\wylde-voice`.
//!
//! Phase 11a of the Rust migration. The Python predecessor at
//! [`Voice/`](../../../../Voice/) hosted an orchestration loop
//! (`voice.toggle` / `voice.start_session`) on top of in-process
//! `faster_whisper` + `kokoro_onnx` + `openwakeword` engines. The Rust
//! port decomposes that into lower-level primitive actions
//! (`voice.transcribe`, `voice.synthesize`, `voice.wakeword.*`) so the
//! harness can compose the loop itself — the same shape every other
//! Rust service in the mesh follows.
//!
//! ## Slice 11.A — foundation (this crate's current scope)
//!
//! What ships in the foundation slice:
//!
//! * `voice.health` — service liveness probe.
//! * `voice.list_models` — enumerate Whisper / Kokoro snapshots present in
//!   the HuggingFace cache, plus the active backend (CPU / NPU). Cheap;
//!   does NOT load any model weights.
//! * `voice.transcribe` — NPU-path proof: loads a Whisper encoder ONNX
//!   via `ort` + the OpenVINO Execution Provider, runs one inference
//!   pass on the supplied audio, returns the encoder output shape. The
//!   autoregressive decoder loop + tokenizer integration are intentionally
//!   deferred (see Slice 11.A+ punchlist in
//!   [`actions::transcribe`]). This is the minimum that proves the spike's
//!   findings reach into the production service architecture.
//!
//! Lifecycle wiring lands alongside: `wylde-lifecycle::services::start_voice`
//! gains `WYLDE_WYLDE_VOICE_IMPL=python|rust` dispatch; default stays
//! `python` until end-to-end transcription parity is verified.
//!
//! ## Slice 11.B — speech synthesis (phoneme path)
//!
//! Adds the deterministic half of TTS:
//! * `voice.synthesize` — phoneme string → 16-bit PCM WAV (base64).
//!   CPU EP only (Kokoro's dynamic-shape inputs preclude OpenVINO VPUX
//!   the same way Whisper's decoder does — Phase 10 spike conclusion).
//!   The text → phonemes step stays in the Python orchestrator until
//!   Slice 11.B+ wires a Rust phonemiser or a thin Python subprocess.
//!
//! ## Slice 11.C — streaming variants
//!
//! Adds opt-in streaming pairs of the unary verbs, both using
//! [`wylde_shared::ipc::register_streaming_action_with_meta`]:
//!
//! * `voice.transcribe_stream` — same payload as `voice.transcribe`;
//!   emits one `encoder_complete` chunk after the Whisper encoder
//!   finishes, then one `token` chunk per decoder step carrying the
//!   cumulative-delta text (Whisper BPE → decode-cumulative-then-slice
//!   is the canonical streaming pattern for non-grapheme-aligned
//!   subword tokenisers), then a final `transcript_complete` summary
//!   with the full transcript + latency breakdown. The decoder loop
//!   runs on a `spawn_blocking` thread so heartbeats + cancellation
//!   propagate while ONNX runs.
//! * `voice.synthesize_stream` — same payload as `voice.synthesize`;
//!   splits phonemes at sentence-shaped terminators
//!   ([`crate::synth::tokenizer::split_phonemes`]) and emits one
//!   `audio_chunk` per sub-utterance (independently playable base64
//!   WAV at 24 kHz), wrapped in `synthesize_start` + `synthesize_complete`
//!   frames.
//!
//! ## Slice 11.D — mic + wake-word
//!
//! Adds the audio-in side of the pipeline:
//!
//! * `voice.mic.start` / `voice.mic.stop` — unary control of a singleton
//!   [`mic::MicCapture`] backed by `cpal`. Default input, mono, 16 kHz
//!   i16 (any source format is downmixed + linear-resampled inside the
//!   worker thread).
//! * `voice.mic.chunks` — streaming subscription to the active capture's
//!   chunk broadcast. Emits base64-encoded `pcm_s16le` frames.
//! * `voice.wakeword.start` / `voice.wakeword.stop` — unary control of a
//!   singleton [`wakeword::WakeWordListener`] that consumes 1280-sample
//!   (80 ms) frames off the mic broadcast, runs the openWakeWord
//!   3-stage ONNX pipeline (mel → embedding → classifier), and emits
//!   events when the score crosses the configured threshold (after
//!   cooldown).
//! * `voice.wakeword.events` — streaming subscription to the listener's
//!   detection broadcast.
//!
//! The cpal `Stream` is `!Send` + `!Sync` on every supported backend, so
//! the mic capture runs on a dedicated `std::thread` that owns the
//! stream from build through drop; chunks fan out via
//! `tokio::sync::broadcast`. See `mic::start_capture` for the full
//! ownership story.
//!
//! ## Slice 11.E — orchestrator port + cpal playback (partial)
//!
//! 2026-05-26 lands the infrastructure pieces of the cutover; the
//! default-flip + Python deletion stay punchlisted until the GUI-facing
//! surface (`voice.toggle` + 7 friends) is ported too.
//!
//! What's in the tree as of Slice 11.E (this slice):
//!
//! * [`playback`] — cpal speaker playback for the orchestrator's TTS
//!   hop. Counterpart to [`mic`] on the output side.
//! * [`orchestrator`] — Rust port of `Voice/orchestrator.py::run_session`:
//!   capture → STT → harness chat → TTS → speaker. Internal-only API
//!   (trait-shaped so tests can mock the harness + audio I/O); not yet
//!   bound to a public `voice.*` action.
//! * The four `voice.transcribe` / `voice.synthesize` (unary + streaming)
//!   catalog entries flipped deferred → active in
//!   `wylde-harness::tooling::tools::voice` — the model now sees them
//!   as callable tools backed by thin IPC bridges into this crate.
//!
//! ## Slice 11.E+ — GUI-facing cutover (2026-05-27)
//!
//! Ports the eight GUI-facing actions from `Voice/pipe.py` so the
//! `WYLDE_WYLDE_VOICE_IMPL` default can flip `python → rust`. Adds the
//! wake-word model scanner + downloader to the Phase 8 model registry
//! so `voice.pull_wake_word_model` has a Rust-side resolver. The
//! Python service stays on disk during a 2-4 week strangler-fig soak;
//! deletion is punchlisted for the cleanup slice (`Voice/` removal,
//! lifecycle Python-spawn fallback removal, sounddevice/openwakeword
//! python deps removal).
//!
//! What lands in this slice:
//!
//! * 8 GUI-facing actions on the wylde-voice pipe (handled by
//!   [`actions::session`]): `voice.toggle` / `voice.start_session` /
//!   `voice.end_session` / `voice.set_mode` / `voice.get_mode` /
//!   `voice.set_active_conversation` / `voice.get_status` /
//!   `voice.check_wake_word_model` / `voice.pull_wake_word_model` /
//!   `voice.subscribe_status`. Plus one bonus
//!   `voice.wake_word_pull_status` for polling in-flight pulls.
//! * Production wirings for the [`orchestrator`] traits in
//!   [`orchestrator_clients`]: a cpal-backed
//!   [`orchestrator_clients::MicSessionCapture`] + a
//!   [`orchestrator_clients::HarnessIpcClient`] that routes the three
//!   harness calls (`models.transcribe`, `chat.run_turn`,
//!   `models.synthesize`) over the shared IPC client.
//! * [`service_state::ServiceState`] — process-wide singleton hosting
//!   mode, active conversation, session, last error, wake-word flag,
//!   and the bounded ring of [`service_state::StatusEvent`]s the
//!   long-poll subscriber consumes.
//! * [`config_persist`] — `voice_config.json` reader/writer mirroring
//!   Python's path resolution so a strangler-fig flip preserves the
//!   user's saved mode.
//! * [`wakeword::download`] — first-run pull of the openWakeWord
//!   ONNX trio (`melspectrogram`, `embedding_model`, classifier) into
//!   `<wakeword_models_dir>/<vendor>/<name>/`. Tracked via a
//!   [`wakeword::download::PullJobs`] singleton so callers can poll
//!   completion.
//! * `model_registry::wakeword_scanner` (in wylde-harness) — walks the
//!   wakeword tree and emits `ModelEntry` records with
//!   `kind: Wakeword` so `list_models(Some(Kind::Wakeword), …)`
//!   surfaces installed bundles alongside HF + Ollama entries.
//!
//! What's deferred:
//!
//! * Bidirectional audio-chunk uplink for `voice.transcribe_stream`
//!   (streaming PCM in → token stream out).
//! * Python `Voice/` deletion — 2-4 week soak before the cleanup slice.
//!
//! ## Deferred from earlier slices
//!
//! * Slice 11.B+ — text-path phonemisation (espeak-ng FFI or
//!   `espeak-ng` Rust crate).
//!
//! ## Public entry points
//!
//! * [`service::install`] — register every `voice.*` action on the
//!   process-wide registry. Idempotent.
//! * [`service::stop`] — release model weights + drop the VRAM lease.
//! * [`service::reset_for_tests`] — clear singletons; for tests only.

pub mod actions;
pub mod config;
pub mod config_persist;
pub mod lease;
pub mod mic;
pub mod model_download;
pub mod model_registry_bridge;
pub mod orchestrator;
pub mod orchestrator_clients;
pub mod playback;
pub mod service;
pub mod service_state;
pub mod state;
pub mod synth;
pub mod transcribe;
pub mod vad;
pub mod wakeword;

pub use service::{install, reset_for_tests, stop};
