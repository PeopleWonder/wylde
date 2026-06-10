//! Tree-sitter sidecar — structural source parsing over the standard Wylde
//! pipe (`\\.\pipe\wylde-treesitter`, framed-msgpack action verbs).
//!
//! Greenfield Rust service (NOT a Python port). It parses source files with
//! tree-sitter and answers structural queries; planned consumers are the
//! RAG-ingest chunker (N8N) and the Memgraph entity extractor. See
//! `docs/plans/treesitter-sidecar.md` for the full design and slice plan.
//!
//! The full six-verb API surface (plan §"API surface") is live:
//!   * `treesitter.languages`        — enumerate the statically-linked grammars.
//!   * `treesitter.parse`            — parse inline `source` to a bounded AST sketch.
//!   * `treesitter.chunk`            — AST-boundary-aware chunking of a file by path.
//!   * `treesitter.extract_entities` — functions/classes/imports/calls (+ bases)
//!     for a file, shaped to feed `memgraph.upsert` entities +
//!     `memgraph.relate` CALLS/IMPORTS/INHERITS edges (no new Memgraph routes).
//!   * `treesitter.outline`          — nested symbol tree for a file (TBS Slice H).
//!   * `treesitter.highlight`        — syntax-highlight spans via the grammars'
//!     bundled queries (TBS Slice H).
//!
//! A loopback HTTP listener ([`http`]) serves the same handlers so N8N's HTTP
//! Request node can call the file-based verbs directly (it can't open a named
//! pipe). Grammars: Python, Rust, TypeScript, TSX, JavaScript, Markdown.
//!
//! Public entry points:
//!   * [`service::install`]        — register the action surface. Idempotent.
//!   * [`service::stop`]           — drain background workers (currently a no-op).
//!   * [`service::reset_for_tests`] — clear the registry; tests only.
//!   * [`http::serve`]             — bind the loopback HTTP front door.

pub mod chunk;
pub mod config;
pub mod entities;
pub mod highlight;
pub mod http;
pub mod outline;
pub mod parser;
pub mod service;

pub use service::{install, reset_for_tests, stop};
