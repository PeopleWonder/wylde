//! Chat-turn driver — Rust port of `Core/harness/turn/`.
//!
//! Phase 5 of the Wylde Rust migration. This submodule is the chat
//! brain: it receives a user message, builds prompt context, calls the
//! LLM via `wylde-ollama` over IPC, (in 5.C) decodes any tool calls,
//! dispatches them, feeds results back to the model, and emits a
//! stream of `TurnEvent` / `ToolEvent` chunks back to the caller.
//!
//! ## Slice 5.A scope (SHIPPED previously)
//!
//! * `chat.run_turn` (non-streaming) end-to-end against `wylde-ollama`
//!   — single LLM call, no tool round trips, no memory layer.
//!
//! ## Slice 5.B scope (THIS SLICE)
//!
//! * `chat.start_turn` — non-blocking; returns turn id, spawns the
//!   streaming turn task.
//! * `chat.cancel` — flips per-turn cancel flag; task observes between
//!   Ollama chunks.
//! * `chat.stream_turn` — STREAMING action; emits `TurnEvent` chunks
//!   (token / thinking / turn_complete / turn_aborted).
//! * `chat.stream_tools` — STREAMING action; emits `ToolEvent` chunks
//!   (idle in 5.B until 5.C lands tool decode).
//!
//! ## Slice 5.C scope (THIS SLICE)
//!
//! * [`salvage`] — port of the Python salvage parser
//!   (`_streaming.py:255-373`): fenced JSON / tag-wrapped /
//!   bare-balanced-brace tool-call recovery + dedupe hash.
//! * [`tool_round`] — port of `_tool_round.py`: per-call dispatch,
//!   tier gate, dedupe set, `MAX_TOOL_LOOPS` cap, memory-write event
//!   emission.
//! * [`actions`] — `drive_streaming_turn` now runs the multi-round
//!   tool-call loop; `chat.run_turn` populates `tool_calls_summary`.
//!
//! ## Deferred to subsequent slices
//!
//! * **5.D — flag flip.** Once parity tests cover ≥10 turn scripts and
//!   ≥1 week of dogfooding, flip `WYLDE_HARNESS_IMPL` default to
//!   `rust` and delete the Python driver.
//! * **Phase 6** — internal tool registry (today the
//!   [`crate::dispatch::call_internal_stub`] returns
//!   `not_implemented`).

pub mod actions;
pub mod chat_options;
pub mod context_gather;
/// Prompt eval/regression goldens (improvement plan B11) — test-only.
#[cfg(test)]
mod golden;
pub mod prompt;
pub mod prompt_assembly;
pub mod reasoning;
pub mod salvage;
/// Slot liveness net (memory plan M8) — test-only: each injected slot's
/// REAL producer asserted end-to-end through gather + render.
#[cfg(test)]
mod slot_liveness;
pub mod think_stream;
pub mod token_budget;
pub mod tool_round;
pub mod workspace_context;
