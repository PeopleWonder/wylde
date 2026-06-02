//! Tree-sitter sidecar — structural source parsing over the standard Wylde
//! pipe (`\\.\pipe\wylde-treesitter`, framed-msgpack action verbs).
//!
//! Greenfield Rust service (NOT a Python port). It parses source files with
//! tree-sitter and answers structural queries; planned consumers are the
//! RAG-ingest chunker (N8N) and the Memgraph entity extractor. See
//! `docs/plans/treesitter-sidecar.md` for the full design and slice plan.
//!
//! **Slice 1 (this revision):** scaffold only — the crate builds, the pipe
//! server comes up, and exactly two verbs are live:
//!   * `treesitter.languages` — enumerate the statically-linked grammars.
//!   * `treesitter.parse`      — parse inline `source` to a bounded AST sketch.
//!
//! Only the Python grammar is linked. No consumers are wired (no N8N HTTP
//! listener, no Memgraph), and the chunk/entities/outline/highlight verbs
//! land in Slices 2–5.
//!
//! Public entry points:
//!   * [`service::install`]        — register the action surface. Idempotent.
//!   * [`service::stop`]           — drain background workers (currently a no-op).
//!   * [`service::reset_for_tests`] — clear the registry; tests only.

pub mod config;
pub mod parser;
pub mod service;

pub use service::{install, reset_for_tests, stop};
