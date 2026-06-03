//! Wylde_Study extension — standalone Rust MCP server.
//!
//! A Rust replacement for the Python `Extensions/Wylde_Study/` extension
//! (today served through `Extensions/_shim/server.py`). It speaks the minimum
//! MCP-over-stdio subset the `wylde-extension-bridge` host drives
//! (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`,
//! `ping`) and exposes the **same five tools** the Python handler does, with
//! the **same MCP tool names** read off `Extensions/Wylde_Study/manifest.json`:
//!
//!   * `study_index_page` — index a page into episodic memory.
//!   * `study_query`       — RAG-search the indexed corpus.
//!   * `study_summarize`   — LLM summary + key points.
//!   * `study_explain`     — LLM plain-language explanation.
//!   * `study_flashcards`  — LLM-generated Q/A cards.
//!
//! ## The data path is the port (not the transport)
//!
//! Python Study reaches the harness by importing `Core.harness.memory.rag`
//! and `Core.harness.backend.backend_routing` **as in-process libraries** — a
//! coupling a Rust binary cannot reuse. Instead this crate calls the S2a pipe
//! verbs on `wylde-harness`:
//!
//!   * `study_index_page` → `rag.add_episodic`
//!   * `study_query`      → `rag.search`
//!   * `study_summarize` / `study_explain` / `study_flashcards` →
//!     `chat.complete`
//!
//! The tool outputs are re-shaped to match the Python handler's JSON dicts so
//! existing consumers (and the MCP `structuredContent` payload) don't break.
//! See `docs/plans/wylde-study-port-verification.md` for the capability map
//! and the known `chat.complete` fidelity gaps (no system-role / format /
//! temperature knob, no `backend` field) called out in [`tools`].
//!
//! Public entry point:
//!   * [`mcp::serve`] — run the MCP stdio server loop until stdin closes.

pub mod config;
pub mod harness;
pub mod jsonparse;
pub mod mcp;
pub mod tools;

pub use mcp::serve;
