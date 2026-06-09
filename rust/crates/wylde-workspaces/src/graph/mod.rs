//! `graph/` — the workspace service's direct-Bolt write surface to Neo4j.
//!
//! A narrow relocation of the harness `memory::memgraph` (Slice 0b): only
//! what the workspace graph-ingest pipeline ([`crate::rag::indexer::graph_writer`])
//! needs — `upsert` + `relate` + `delete_workspace`, the relation vocabulary,
//! and the [`EntityPair`] request type. The harness retains its full memgraph
//! client (read traversals, multihop, stats) for its own memory layer; the
//! two are independent per-service clients reaching the same Neo4j over Bolt.

pub mod bolt;
pub mod cypher;
pub mod schema;

// ── Slice B (Phase 1) — the read API ─────────────────────────────────────
// Read-only `workspaces.graph` surface. Separate from the write surface
// above (`bolt`/`cypher`/`schema`): `query` holds the Cypher reads + row
// types, `projection` turns rows into the wire `WorkspaceGraph`, `api` is the
// verb. The write half is untouched.
pub mod api;
pub mod projection;
pub mod query;

// ── Slice F-data (Phase 1) — in-memory symbol index + `symbols.find` ──────
// Built FROM the Slice B read path (`fetch_workspace_graph` → `projection`);
// adds nothing to the write/read query surface above. Self-contained in
// `symbol_index` (data structure + verb + lifecycle).
pub mod symbol_index;
// ── Slice G-data (Phase 1) — symbol_context read verb ────────────────────
// k-hop neighbourhood walk (callers/callees/types/siblings) + body fetch.
// Read-only, like Slice B; reuses the write surface's `BoltClient` pool via
// `BoltClient::graph_handle`. Self-contained in `neighborhood` (Cypher, live
// source, walk, and the verb handler) so it doesn't touch `api`/`query`.
pub mod neighborhood;

use serde::{Deserialize, Serialize};

pub use bolt::{BoltClient, BoltConfig, DEFAULT_BOLT_URL};
pub use neighborhood::{ContextRel, RelatedSymbol, Symbol, SymbolContext};
pub use projection::{
    Cluster, Edge, Node, NodeKind, NodeStyle, Position, RelType, WorkspaceGraph,
};
pub use symbol_index::{SymbolEntry, SymbolIndex, SymbolMatch, SymbolsFindResponse};
pub use schema::{
    relation_type_is_valid, NODE_CHUNK, NODE_ENTITY, REL_CALLS, REL_CONFIGURES, REL_EXPOSES,
    REL_IMPORTS, REL_INHERITS, REL_MENTIONED_IN,
};

/// One typed Entity→Entity edge endpoint pair for a `relate` write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityPair {
    pub source: String,
    pub target: String,
}

impl EntityPair {
    /// Build a pair from any string-likes.
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }
}
