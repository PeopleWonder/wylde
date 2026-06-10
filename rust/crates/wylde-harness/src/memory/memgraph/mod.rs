//! `memgraph/` — Rust client for the `wylde-memgraph` graph service.
//!
//! Phase 7 of the Wylde Rust migration. Rust port of
//! `Core/harness/memory/memgraph.py` + `graph_retrieval.py`. The
//! companion `wylde-memgraph` Lifecycle service stays in Python, but
//! since the 2026-05-26 direct-Bolt cutover it owns *only* the bundled
//! Neo4j JVM lifecycle — its former named-pipe wire surface
//! (`\\.\pipe\wylde-memgraph`) is retired. The harness now reaches the
//! graph over Bolt directly (see [`bolt`]); the legacy pipe [`client`]
//! is kept as the dormant strangler escape-hatch + test seam only.
//!
//! ## Submodules
//!
//! * [`client`] — typed wrappers over the retired IPC routes
//!   (`health`, `traverse`, `multihop`, `relate`, …). Dormant in
//!   production (the pipe surface is gone); still used as the
//!   transport test seam. Transport is swappable for offline testing.
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
//! Like every memory submodule, this one reads
//! `WYLDE_HARNESS_MEMORY_IMPL`. Default flipped from `python` →
//! `rust` on 2026-05-26 (parity gate: 8/11 verbs match exactly;
//! `relate` / `unrelate` / `upsert_edge` diverge because the Python
//! Flask routes were always broken — wrong field name on relate /
//! unrelate, missing route on upsert_edge — so Bolt is the canonical
//! shape). The same 2026-05-26 cutover **retired the named-pipe
//! surface**: `Core/Memgraph/run.py` deleted the `graph_service` Flask
//! app and now only supervises the Neo4j JVM, so nothing binds
//! `\\.\pipe\wylde-memgraph` anymore.
//!
//! That leaves the two transports asymmetric:
//!
//! * [`bolt::BoltClient`] — direct Bolt to Neo4j via `neo4rs`. The
//!   live path (`WYLDE_HARNESS_MEMORY_IMPL=rust`, the default). The
//!   [`cypher`] module is the Cypher source of truth here, ported from
//!   the old `_driver.py` / `_routes_*.py`.
//! * [`client::Client`] — msgpack-over-named-pipe to the (now retired)
//!   pipe surface. The `WYLDE_HARNESS_MEMORY_IMPL=python` branch still
//!   selects it, but its server is gone, so that rollback target no
//!   longer answers. It survives only as (a) the strangler
//!   escape-hatch *shape* and (b) the [`transport::MemgraphTraversal`]
//!   test seam. The post-soak cleanup slice deletes it once the Bolt
//!   path's soak completes; doing so now would also force relocating
//!   the shared [`client::TraverseRequest`] / [`client::EntityPair`]
//!   request types and rewriting the transport mocks, so it is held
//!   back deliberately rather than rushed mid-soak.
//!
//! The Python `Core/Memgraph/` process stays alive regardless — it owns
//! the bundled Neo4j JVM lifecycle (spawns + supervises
//! `vendor/neo4j`), which must migrate to Rust before `Core/Memgraph/`
//! can be deleted entirely.

pub mod actions;
pub mod bolt;
pub mod client;
pub mod cypher;
pub mod graph_retrieval;
pub mod schema;
pub mod transport;

pub use bolt::{BoltClient, BoltConfig, DEFAULT_BOLT_URL, DRIVER_ERROR_TTL};
pub use client::{Client, EntityPair, TraverseRequest};
pub use graph_retrieval::{
    expand_by_graph, ExpandOptions, GraphHit, DEFAULT_HOPS, DEFAULT_MAX_EXTRA,
};
pub use schema::{
    relation_type_is_valid, BUCKET_CALLS_IMPORTS, BUCKET_CONFIGURES_EXPOSES, NODE_CHUNK,
    NODE_ENTITY, REL_CALLS, REL_CONFIGURES, REL_EXPOSES, REL_IMPORTS, REL_INHERITS,
    REL_MENTIONED_IN,
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
    /// `WYLDE_HARNESS_MEMORY_IMPL=python` — pipe to the Python
    /// `wylde-memgraph` service. NOTE: the pipe surface was retired in
    /// the 2026-05-26 direct-Bolt cutover (`Core/Memgraph/run.py` now
    /// only supervises the Neo4j JVM; nothing binds
    /// `\\.\pipe\wylde-memgraph`), so this branch reaches a dead pipe —
    /// the `python` rollback target no longer exists. Retained only as
    /// the strangler escape-hatch *shape* + the transport test seam
    /// until the post-soak cleanup deletes it. See module docs.
    Pipe(Client),
    /// `WYLDE_HARNESS_MEMORY_IMPL=rust` (default) — direct Bolt to Neo4j.
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

    async fn stats(&self) -> wylde_shared::ipc::Reply {
        match self {
            TraversalImpl::Pipe(c) => c.stats().await,
            TraversalImpl::Bolt(b) => b.stats().await,
        }
    }

    async fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> wylde_shared::ipc::Reply {
        match self {
            TraversalImpl::Pipe(c) => c.upsert_edge(source, label, target, weight_delta).await,
            TraversalImpl::Bolt(b) => b.upsert_edge(source, label, target, weight_delta).await,
        }
    }
}
