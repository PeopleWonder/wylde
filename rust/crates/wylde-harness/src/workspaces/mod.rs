//! `workspaces/` — config-file-backed workspaces (redesign).
//!
//! **Conceptual path:** `Core/Harness/Workspaces/` in Aaron's
//! Python-style mental model. The actual Rust home is this module,
//! `crate::workspaces`, per the everything-Rust direction.
//!
//! ## What this is (and what it replaces)
//!
//! A workspace is **configuration that shapes prompt building**, not a
//! service with a CRUD verb surface. The prior verb-driven design
//! (`rag.workspaces.*` + the registry-only port in
//! [`crate::memory::workspaces`]) treated workspaces like a mini
//! database. The redesign reframes them as config files the harness
//! reads: the turn driver picks up `workspace_id` off `chat.run_turn`
//! and the prompt builder injects that workspace's context — RAG scope,
//! persona, and the workspace-layer memory tier — into the system
//! prompt.
//!
//! See `docs/plans/workspaces-redesign-2026-06-04.md` for the full
//! design, migration plan, and open questions. This module is a
//! **scaffold only** — types + signatures + TODOs, no logic yet.
//!
//! ## Submodule map
//!
//! Each submodule maps to a `Core/Harness/Workspaces/<Subfolder>/` in
//! the conceptual layout:
//!
//! * [`registry`] — the **store of workspace configurations** (Aaron's
//!   "workspaces memory of some sort that stores each workspace
//!   configuration"). Owns `workspaces.json` + `state.json`. This is the
//!   *config* tier; do not confuse it with [`memory`] below.
//! * [`memory`] — the **per-workspace memory-entries layer**: the middle
//!   tier of the 3-layer memory architecture (long-term / **workspace**
//!   / short-term). One bucket of curated entries per workspace, fetched
//!   and scored for prompt injection.
//! * [`persona`] — optional per-workspace persona override (a system
//!   prompt modifier stored as `persona.md`).
//! * [`rag`] — translates a workspace's folder into a RAG query scope so
//!   retrieval is bounded to the workspace's files.
//! * [`prompt`] — assembles this workspace's contribution to a chat
//!   turn's system prompt from persona + memory + RAG.
//! * [`api`] — the minimal IPC verb surface (writes + active-selection;
//!   reads go straight to `workspaces.json` from the GUI).
//!
//! ## Why two "memory" concepts live side by side
//!
//! `registry` is storage-of-configs; `memory` is the memory *tier*.
//! Aaron uses "memory" loosely for both. The names here are chosen so
//! the code reads unambiguously: **registry = configs, memory =
//! entries.**

pub mod api;
pub mod memory;
pub mod persona;
pub mod prompt;
pub mod rag;
pub mod registry;

// ── Public surface (re-exported so the scaffold is part of the crate's
// public API — keeps `cargo check` warning-free while the bodies are
// still TODO) ──────────────────────────────────────────────────────────

pub use memory::{WorkspaceMemoryEntry, WorkspaceMemoryQuery};
pub use persona::PersonaOverride;
pub use prompt::WorkspaceContext;
pub use rag::WorkspaceRagScope;
pub use registry::{WorkspaceDefinition, WorkspaceState};
