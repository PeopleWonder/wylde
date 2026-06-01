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
//! Keeping the trait minimal (just `traverse` + `multihop`) keeps the
//! abstraction cost negligible while letting [`super::dispatch`]
//! flip the implementation behind one env var.
//!
//! ## Why not a full verb surface?
//!
//! The full verb set (`health`, `ensure_schema`, `upsert`, `relate`,
//! ...) doesn't need a runtime switch in this slice — those callers
//! pick a concrete transport directly. Only the two retrieval verbs
//! the tool catalog dispatches through are shared, so the trait stays
//! tight.
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
}

impl MemgraphTraversal for Client {
    async fn traverse(&self, req: TraverseRequest) -> Reply {
        Client::traverse(self, req).await
    }

    async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        Client::multihop(self, entities, expand_hops, limit).await
    }
}

impl MemgraphTraversal for BoltClient {
    async fn traverse(&self, req: TraverseRequest) -> Reply {
        BoltClient::traverse(self, req).await
    }

    async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        BoltClient::multihop(self, entities, expand_hops, limit).await
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
