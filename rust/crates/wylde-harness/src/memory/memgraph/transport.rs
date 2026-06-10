//! Shared transport surface — the small slice of verbs
//! [`super::graph_retrieval::expand_by_graph`] and
//! [`super::actions::run_graph_query`] need to call. Implemented by
//! both transports:
//!
//! * [`super::client::Client`] — msgpack-over-named-pipe to the
//!   Python `wylde-memgraph` service (the `python` strangler-fig
//!   branch).
//! * [`super::bolt::BoltClient`] — direct Bolt to Neo4j via `neo4rs`
//!   (the `rust` branch).
//!
//! Keeping the trait tight lets [`super::current_traversal_impl`]
//! flip the implementation behind one env var while the abstraction
//! cost stays negligible.
//!
//! ## Verb surface
//!
//! The trait carries the verbs the *tool catalog* dispatches through —
//! the two retrieval verbs (`traverse` / `multihop`) plus `stats` and
//! `upsert_edge`. The latter two were added when `rag.graph_stats` and
//! the RAG feedback writer were found hardcoding a concrete pipe
//! [`Client`] — which, after the 2026-05-26 direct-Bolt cutover retired
//! the `\\.\pipe\wylde-memgraph` surface, meant they were talking to a
//! pipe nothing serves. Routing them through this trait makes them honor
//! the same strangler selection the retrieval path already does (Bolt by
//! default), so the whole tool-facing graph surface picks one transport.
//!
//! The remaining write/admin verbs (`ensure_schema`, `upsert`,
//! `relate`, `delete_path`, ...) still pick a concrete transport at
//! their call sites and stay off the trait.
//!
//! ## Native async fns in traits
//!
//! Rust 1.85 stabilised async fns in traits; we use the bare syntax.
//! Trait objects (`dyn MemgraphTraversal`) are deliberately NOT used —
//! every call site has a concrete client type, so static dispatch
//! works and we avoid the `Send` bound bookkeeping `dyn` futures
//! require.

use std::future::Future;

use wylde_shared::ipc::Reply;

use super::bolt::BoltClient;
use super::client::{Client, TraverseRequest};

/// Minimal traversal surface — what
/// [`super::graph_retrieval::expand_by_graph`] and
/// [`super::actions::run_graph_query`] dispatch through.
///
/// The trait methods desugar to `fn → impl Future + Send` rather than
/// `async fn` so the public surface carries the `Send` bound
/// explicitly. With the bare `async fn` syntax clippy refuses public
/// traits (`async_fn_in_trait`) because auto traits like `Send`
/// can't be expressed without a breaking change.
pub trait MemgraphTraversal: Send + Sync {
    /// `POST /traverse` — entity-anchored chunk discovery.
    fn traverse(&self, req: TraverseRequest) -> impl Future<Output = Reply> + Send;

    /// `POST /multihop` — multi-hop entity expansion to chunks.
    fn multihop(
        &self,
        entities: Vec<String>,
        expand_hops: u32,
        limit: u32,
    ) -> impl Future<Output = Reply> + Send;

    /// `GET /stats` — graph-wide node/edge counts. Backs the
    /// `rag.graph_stats` tool.
    fn stats(&self) -> impl Future<Output = Reply> + Send;

    /// `POST /upsert_edge` — MERGE-style weighted edge upsert. Backs the
    /// RAG reader→writer feedback loop.
    fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> impl Future<Output = Reply> + Send;
}

impl MemgraphTraversal for Client {
    async fn traverse(&self, req: TraverseRequest) -> Reply {
        Client::traverse(self, req).await
    }

    async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        Client::multihop(self, entities, expand_hops, limit).await
    }

    async fn stats(&self) -> Reply {
        Client::stats(self).await
    }

    async fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> Reply {
        Client::upsert_edge(self, source, label, target, weight_delta).await
    }
}

impl MemgraphTraversal for BoltClient {
    async fn traverse(&self, req: TraverseRequest) -> Reply {
        BoltClient::traverse(self, req).await
    }

    async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        BoltClient::multihop(self, entities, expand_hops, limit).await
    }

    async fn stats(&self) -> Reply {
        BoltClient::stats(self).await
    }

    async fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> Reply {
        BoltClient::upsert_edge(self, source, label, target, weight_delta).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_bolt_client_both_implement_transport() {
        // Compile-time check — if either type drops the impl this
        // file won't compile.
        fn assert_traversal<T: MemgraphTraversal>() {}
        assert_traversal::<Client>();
        assert_traversal::<BoltClient>();
    }
}
