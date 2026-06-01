//! Chat panel — primary chat surface in the gpui-era GUI.
//!
//! Replaces `Core/GUI/src/components/InferenceBar.svelte`.  The Svelte
//! version was a 1300-line component that fused the message log, the
//! InferenceBar, the workspace picker, the model picker, voice
//! capture, and conversation-list coordination.  The gpui port pulls
//! the primary surface — prompt-in / streamed-text-out, plus the
//! workspace MRU, inline consent prompts, model picker, and tool
//! activity strip — into one panel and leaves the rest to follow-on
//! slices:
//!
//!   * Voice mic + speak toggles → Voice panel slice (Phase 11.E
//!     surface is the parent).
//!   * Conversation list / multi-tab → Conversations panel slice.
//!
//! What this slice (5.1) owns:
//!   * Message log with user / assistant bubbles + system info.
//!     Streaming chunks accumulate on the in-flight assistant bubble.
//!   * **Markdown rendering** in assistant bubbles — paragraphs, heads,
//!     bold/italic/code, fenced code blocks, ordered/bullet lists,
//!     links (open in OS default browser).
//!   * **TextInput widget** (`wylde-gpui-input`) replaces the slice-5
//!     hand-rolled keyboard dispatch.  Multi-line; Enter submits,
//!     Shift+Enter newline.
//!   * **Stop button** that drops the in-flight `PipeStream` AND fires
//!     `chat.cancel` server-side.
//!   * **Tool activity strip** subscribed to `chat.stream_tools`;
//!     surfaces "Wylde is consulting …" near the InferenceBar without
//!     polluting the bubble log.
//!   * **Model picker pill** lists Ollama-visible models; selection is
//!     forwarded as the `model` field of `chat.start_turn`.
//!   * InferenceBar at the bottom: prompt textbox, send/stop button,
//!     workspace MRU-5 dropdown, native folder picker, model picker.
//!   * Inline consent cards subscribed to `consent.stream_pending`.

pub mod chat_panel;
pub mod ipc;
pub mod markdown;

pub use chat_panel::ChatPanel;
