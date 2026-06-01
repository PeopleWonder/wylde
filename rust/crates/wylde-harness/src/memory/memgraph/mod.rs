//! `memgraph/` — Rust client for the `wylde-memgraph` graph service.
//!
//! Phase 7 of the Wylde Rust migration. Rust port of
//! `Core/harness/memory/memgraph.py` + `graph_retrieval.py`. The
//! companion `wylde-memgraph` Lifecycle service stays in Python — it
//! owns the bundled Neo4j JVM and serves a named-pipe wire surface
//! (`\\.\pipe\wylde-memgraph`); this module is the **client** half that
//! the harness uses to talk to it.
//!
//! ## Submodules
//!
//! * [`client`] — typed wrappers over the IPC routes
//!   (`health`, `traverse`, `multihop`, `relate`, …). Transport is
//!   swappable for offline testing.
//! * [`schema`] — node label + relation-type constants the service
//!   accepts on its `/traverse` and `/relate` routes.
//! * [`graph_retrieval`] — Stage-2 graph-distance expansion for the
//!   RAG retrieval pipeline. Calls `client::Client::traverse` /
//!   `multihop` and ranks chunks by inverse hop distance.
//! * [`actions`] — `meta.graph_query` tool handler — the same hybrid
//!   graph + vector retrieval that the Python tool exposes, restricted
//!   to the entity-only path until the Rust RAG port lands.
//!
//! ## Crate choice — neo4rs, not rsmgclient
//!
//! The Phase 7.A handoff doc named `rsmgclient` (Memgraph the product's
//! Bolt binding) as the planned crate. The actual backing DB the
//! `wylde-memgraph` service supervises is **Neo4j Community 5.x** via
//! `vendor/neo4j/bin/neo4j.bat`, NOT Memgraph the product —
//! `rsmgclient` wraps Memgraph's `libmgclient.dll` and is the wrong
//! library. [`bolt`] uses `neo4rs` (pure Rust, tokio-async,
//! Windows-clean) and reaches the same `bolt://127.0.0.1:7687` that
//! Python's `Core/Memgraph/graph_service/_driver.py::_get_driver`
//! already talks to.
//!
//! ## Strangler-fig
//!
//! Like every memory submodule, this one is gated by
//! `WYLDE_HARNESS_MEMORY_IMPL`. Default flipped from `python` →
//! `rust` on 2026-05-26 (parity gate: 8/11 verbs match exactly;
//! `relate` / `unrelate` / `upsert_edge` diverge because the Python
//! Flask routes were always broken — wrong field name on relate /
//! unrelate, missing route on upsert_edge — so Bolt is the canonical
//! shape). Two transports live side-by-side:
//!
//! * [`client::Client`] — msgpack-over-named-pipe to the Python
//!   `wylde-memgraph` service. Selected when
//!   `WYLDE_HARNESS_MEMORY_IMPL=python` (rollback escape hatch). Same
//!   Cypher eventually, but with a Flask round-trip.
//! * [`bolt::BoltClient`] — direct Bolt to Neo4j via `neo4rs`. Selected
//!   when `WYLDE_HARNESS_MEMORY_IMPL=rust` (default). Skips the pipe +
//!   Flask hop entirely; the [`cypher`] module is the Cypher source of
//!   truth on this path, ported from `_driver.py` / `_routes_*.py`.
//!
//! The Python service stays alive for the 2–4-week soak window — the
//! JVM-lifecycle ownership (it spawns + supervises `vendor/neo4j`)
//! needs the cleanup slice to migrate before `Core/Memgraph/` can go
//! away entirely.

pub mod actions;
pub mod bolt;
pub mod client;
pub mod cypher;
pub mod graph_retrieval;
pub mod schema;
pub mod transport;

pub use bolt::{BoltClient, BoltConfig, DEFAULT_BOLT_URL, DRIVER_ERROR_TTL};
pub use client::{Client, EntityPair, TraverseRequest};
pub use graph_retrieval::{expand_by_graph, ExpandOptions, GraphHit, DEFAULT_HOPS, DEFAULT_MAX_EXTRA};
pub use schema::{
    relation_type_is_valid, BUCKET_CALLS_IMPORTS, BUCKET_CONFIGURES_EXPOSES, NODE_CHUNK,
    NODE_ENTITY, REL_CALLS, REL_CONFIGURES, REL_EXPOSES, REL_IMPORTS, REL_INHERITS, REL_MENTIONED_IN,
};
pub use transport::MemgraphTraversal;

/// Pick the right traversal transport for the active strangler-fig
/// branch. `WYLDE_HARNESS_MEMORY_IMPL=rust` returns
/// [`TraversalImpl::Bolt`]; anything else (including the default)
/// returns [`TraversalImpl::Pipe`].
///
/// Both variants implement [`MemgraphTraversal`] so callers can use
/// the result with `expand_by_graph` / `run_graph_query` without
/// further branching.
pub fn current_traversal_impl() -> TraversalImpl {
    if super::impl_for() == "rust" {
        TraversalImpl::Bolt(BoltClient::new())
    } else {
        TraversalImpl::Pipe(Client::new())
    }
}

/// Strangler-fig dispatch envelope. Constructed by
/// [`current_traversal_impl`].
pub enum TraversalImpl {
    /// `WYLDE_HARNESS_MEMORY_IMPL=python` (default) — pipe to the
    /// Python `wylde-memgraph` service.
    Pipe(Client),
    /// `WYLDE_HARNESS_MEMORY_IMPL=rust` — direct Bolt to Neo4j.
    Bolt(BoltClient),
}

impl TraversalImpl {
    /// Human-readable tag for logs / diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            TraversalImpl::Pipe(_) => "pipe",
            TraversalImpl::Bolt(_) => "bolt",
        }
    }
}

impl MemgraphTraversal for TraversalImpl {
    async fn traverse(&self, req: TraverseRequest) -> wylde_shared::ipc::Reply {
        match self {
            TraversalImpl::Pipe(c) => c.traverse(req).await,
            TraversalImpl::Bolt(b) => b.traverse(req).await,
        }
    }

    async fn multihop(
        &self,
        entities: Vec<String>,
        expand_hops: u32,
        limit: u32,
    ) -> wylde_shared::ipc::Reply {
        match self {
            TraversalImpl::Pipe(c) => c.multihop(entities, expand_hops, limit).await,
            TraversalImpl::Bolt(b) => b.multihop(entities, expand_hops, limit).await,
        }
    }
}
