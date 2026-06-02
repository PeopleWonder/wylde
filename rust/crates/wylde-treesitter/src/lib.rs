//! Tree-sitter sidecar — structural source parsing over the standard Wylde
//! pipe (`\\.\pipe\wylde-treesitter`, framed-msgpack action verbs).
//!
//! Greenfield Rust service (NOT a Python port). It parses source files with
//! tree-sitter and answers structural queries; planned consumers are the
//! RAG-ingest chunker (N8N) and the Memgraph entity extractor. See
//! `docs/plans/treesitter-sidecar.md` for the full design and slice plan.
//!
//! **Slice 2 (this revision):** the chunk surface + its N8N front door.
//! Live verbs:
//!   * `treesitter.languages` — enumerate the statically-linked grammars.
//!   * `treesitter.parse`      — parse inline `source` to a bounded AST sketch.
//!   * `treesitter.chunk`      — AST-boundary-aware chunking of a file by path.
//!
//! A loopback HTTP listener ([`http`]) serves the same handlers so N8N's HTTP
//! Request node can call `/chunk` directly (it can't open a named pipe). Only
//! the Python grammar is linked; the extract_entities/outline/highlight verbs
//! and the remaining grammars land in Slices 3–5.
//!
//! Public entry points:
//!   * [`service::install`]        — register the action surface. Idempotent.
//!   * [`service::stop`]           — drain background workers (currently a no-op).
//!   * [`service::reset_for_tests`] — clear the registry; tests only.
//!   * [`http::serve`]             — bind the loopback HTTP front door.

pub mod chunk;
pub mod config;
pub mod http;
pub mod parser;
pub mod service;

pub use service::{install, reset_for_tests, stop};
