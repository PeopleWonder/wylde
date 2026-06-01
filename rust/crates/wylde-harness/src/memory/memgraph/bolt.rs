//! Direct-to-Neo4j Bolt path — used when
//! `WYLDE_HARNESS_MEMORY_IMPL=rust`.
//!
//! The Python `Core/Memgraph/` service wraps the same bundled Neo4j
//! JVM (`vendor/neo4j/bin/neo4j.bat`) with a Flask app behind a named
//! pipe. Going direct here skips the pipe + Flask + msgpack round-trip
//! and talks to Neo4j over Bolt the same way the Python service's
//! `_driver.py::_get_driver` does — minus the eager
//! `verify_connectivity` call that doesn't exist on `neo4rs`.
//!
//! ## Driver-model differences vs Python
//!
//! The Python `neo4j-python-driver`'s `GraphDatabase.driver()` opens a
//! pooled connection eagerly and runs `verify_connectivity()` at
//! construction; the Python service negatively caches that failure
//! for [`DRIVER_ERROR_TTL`] so a downed Neo4j doesn't turn every
//! request into a fresh connect attempt.
//!
//! `neo4rs::Graph::connect` is **lazy** — it builds the pool but does
//! not actually open a socket until the first `execute()`. Each
//! `execute()` against an unreachable Neo4j returns a fast IO error,
//! so the Python-style negative cache is not needed at this layer
//! (the OS already gives us cheap, immediate refusal). If profiling
//! later shows hot-loop reconnects (e.g. against a JVM mid-restart),
//! reintroduce the cache around [`BoltClient::graph`].
//!
//! ## Env knobs (mirror Python)
//!
//! * `GRAPH_BOLT_URL` — default `bolt://127.0.0.1:7687`.
//! * `GRAPH_USER` / `GRAPH_PASSWORD` — default empty (auth disabled,
//!   matching the Wylde user's `auth=None` config in `_driver.py`).
//! * `WYLDE_BOLT_CONNECT_TIMEOUT_SECS` — default 5s. Bounds the
//!   per-connection-attempt wait so a wrong URI fails the verb in
//!   ~connect_timeout, not in the driver's internal retry budget.
//!
//! ## Strangler-fig
//!
//! Strictly the `rust` branch of the strangler-fig switch. The
//! `python` branch keeps using [`super::client::Client`] over the
//! shared-IPC pipe, which itself reaches the Flask-fronted Cypher
//! that lives in `Core/Memgraph/graph_service/_routes_*.py`. Both
//! paths terminate at the same Neo4j Bolt port.

use std::time::Duration;

use neo4rs::{BoltList, BoltMap, BoltType, ConfigBuilder, Graph};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use wylde_shared::ipc::{IpcError, Reply};

use super::cypher;
use super::schema as rel_schema;

/// Default Bolt URL — matches Python's `_BOLT_URL` default.
pub const DEFAULT_BOLT_URL: &str = "bolt://127.0.0.1:7687";

/// Default per-attempt connect timeout. Mirrors Python's `_driver.py`
/// `connection_timeout=5.0`. Override via
/// `WYLDE_BOLT_CONNECT_TIMEOUT_SECS`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Stub of the Python negative-cache TTL. Not honoured at this layer
/// (see module docs); kept for documentation and so the future cache
/// re-introduction has the same default. Mirrors Python's
/// `_DRIVER_ERROR_TTL_S`.
pub const DRIVER_ERROR_TTL: Duration = Duration::from_secs(30);

/// `bolt_*` error codes the harness surfaces via [`Reply::err_msg`].
/// Kept narrow so callers can match on the code without parsing the
/// driver's message.
pub mod error_codes {
    /// Config build failed (URI parse, etc.).
    pub const CONFIG: &str = "bolt_config";
    /// Connect / pool acquisition failed.
    pub const CONNECT: &str = "bolt_connect";
    /// Query executed but Neo4j returned an error, or the underlying
    /// IO failed (Neo4j unreachable, refused, etc.). `neo4rs`'s lazy
    /// connect model means refusal surfaces here, not at CONNECT.
    pub const QUERY: &str = "bolt_query";
    /// Row decode failed (unexpected schema / type mismatch).
    pub const DECODE: &str = "bolt_decode";
}

/// Static configuration captured at [`BoltClient::new`] time. Env
/// vars read once so tests rebinding `GRAPH_*` per-test don't race.
#[derive(Clone, Debug)]
pub struct BoltConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub connect_timeout: Duration,
}

impl BoltConfig {
    /// Build a config from env (`GRAPH_BOLT_URL` / `GRAPH_USER` /
    /// `GRAPH_PASSWORD` / `WYLDE_BOLT_CONNECT_TIMEOUT_SECS`). Falls
    /// back to the Python defaults.
    pub fn from_env() -> Self {
        let connect_timeout = std::env::var("WYLDE_BOLT_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(Duration::from_secs_f64)
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        Self {
            uri: std::env::var("GRAPH_BOLT_URL").unwrap_or_else(|_| DEFAULT_BOLT_URL.to_owned()),
            user: std::env::var("GRAPH_USER").unwrap_or_default(),
            password: std::env::var("GRAPH_PASSWORD").unwrap_or_default(),
            connect_timeout,
        }
    }
}

/// Bolt-backed memgraph client. Lazily opens one [`Graph`] (i.e. one
/// neo4rs pool) on the first verb; subsequent calls reuse it. `Graph`
/// is `Arc`-shared, so cloning the client is cheap.
pub struct BoltClient {
    config: BoltConfig,
    graph: OnceCell<Graph>,
}

impl BoltClient {
    /// Build a client wired to the env-var defaults.
    pub fn new() -> Self {
        Self::with_config(BoltConfig::from_env())
    }

    /// Build a client around an explicit URI. Tests use this to point
    /// at a sandboxed instance or a deliberately-bad URI for the
    /// error-path tests. Inherits the rest of the defaults.
    pub fn for_uri(uri: impl Into<String>) -> Self {
        Self::with_config(BoltConfig {
            uri: uri.into(),
            user: String::new(),
            password: String::new(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        })
    }

    /// Build a client around a fully-specified [`BoltConfig`].
    pub fn with_config(config: BoltConfig) -> Self {
        Self {
            config,
            graph: OnceCell::new(),
        }
    }

    /// Snapshot of the captured config — for diagnostics and the
    /// integration test's URI logging.
    pub fn config(&self) -> &BoltConfig {
        &self.config
    }

    /// Acquire (or open) the underlying [`Graph`]. The pool is
    /// `OnceCell`-cached; concurrent first-callers serialise behind
    /// one connect attempt automatically.
    async fn graph(&self) -> Result<&Graph, IpcError> {
        self.graph
            .get_or_try_init(|| async {
                let cfg = ConfigBuilder::default()
                    .uri(self.config.uri.clone())
                    .user(self.config.user.clone())
                    .password(self.config.password.clone())
                    .build()
                    .map_err(|e| IpcError::new(error_codes::CONFIG, format!("config: {e}")))?;
                Graph::connect(cfg).await.map_err(|e| {
                    IpcError::new(
                        error_codes::CONNECT,
                        format!("{}: {}", self.config.uri, e),
                    )
                })
            })
            .await
    }

    /// `health` — connectivity probe. Issues `RETURN 1 AS ok` and
    /// returns a [`Reply::ok`] envelope shaped the same way the Python
    /// `/health` route returns it (`{"ok": bool}`). Connect / query
    /// failures surface as [`Reply::err_msg`] with a `bolt_*` code.
    ///
    /// The whole thing is bounded by [`BoltConfig::connect_timeout`]
    /// so a wrong URI fails the verb in ~5s, not in neo4rs's internal
    /// reconnect budget.
    pub async fn health(&self) -> Reply {
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            let mut result = graph
                .execute(neo4rs::query("RETURN 1 AS ok"))
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("health probe: {e}")))?;
            match result.next().await {
                Ok(Some(_)) => Ok(true),
                Ok(None) => Ok(false),
                Err(e) => Err((error_codes::DECODE.to_owned(), format!("health decode: {e}"))),
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(ok)) => Reply::ok(json!({ "ok": ok })),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("health probe timed out after {timeout:?}"),
            ),
        }
    }

    /// `ensure_schema` — idempotent index creation. Runs each
    /// statement in [`cypher::SCHEMA_STATEMENTS`] inside one
    /// transaction. Returns `{ "ok": true }` on success.
    ///
    /// Python's `_routes_core.ensure_schema` swallows per-statement
    /// errors (Neo4j sometimes raises if the index exists on a
    /// different shape); we mirror that — each statement runs
    /// independently and a failure on one doesn't abort the rest.
    pub async fn ensure_schema(&self) -> Reply {
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            for stmt in cypher::SCHEMA_STATEMENTS {
                if let Err(e) = graph.run(neo4rs::query(stmt)).await {
                    // Match Python's debug-log + continue behaviour;
                    // an idempotent re-add commonly errors and that's
                    // OK.
                    tracing::debug!(stmt = stmt, error = %e, "ensure_schema stmt skipped");
                }
            }
            Ok::<_, (String, String)>(())
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(())) => Reply::ok(json!({ "ok": true })),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("ensure_schema timed out after {timeout:?}"),
            ),
        }
    }

    /// `delete_path` — DETACH DELETE every Chunk for one source path.
    /// Returns `{"ok": true}` on success.
    pub async fn delete_path(&self, path: &str) -> Reply {
        let path = path.to_owned();
        self.run_void(&format!("delete_path({path})"), move |graph| {
            let path = path.clone();
            Box::pin(async move {
                graph
                    .run(neo4rs::query(cypher::DELETE_PATH).param("path", path))
                    .await
                    .map_err(|e| (error_codes::QUERY.to_owned(), format!("delete_path: {e}")))
            })
        })
        .await
    }

    /// `delete_workspace` — drop every Chunk in a workspace, then
    /// prune now-orphaned Entity nodes. Mirrors the two-step Python
    /// route. Returns counts so callers can confirm the bloat is
    /// actually gone:
    /// `{"ok": true, "workspace": ws, "chunks_deleted": N, "orphan_entities_deleted": M}`.
    pub async fn delete_workspace(&self, workspace: &str) -> Reply {
        let ws = workspace.trim().to_owned();
        if ws.is_empty() {
            return Reply::err_msg("bad_request", "'workspace' required");
        }
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;

            let chunks_deleted = run_single_count(
                graph,
                cypher::DELETE_WORKSPACE_CHUNKS,
                vec![("ws".to_owned(), BoltType::from(ws.clone()))],
            )
            .await?;
            let orphans = run_single_count(graph, cypher::DELETE_ORPHAN_ENTITIES, vec![]).await?;

            Ok::<_, (String, String)>(json!({
                "ok": true,
                "workspace": ws,
                "chunks_deleted": chunks_deleted,
                "orphan_entities_deleted": orphans,
            }))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => Reply::ok(v),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("delete_workspace timed out after {timeout:?}"),
            ),
        }
    }

    /// `traverse` — typed-edge graph expansion with per-relation-type
    /// depth budgets. Rust port of `_routes_traverse.traverse`. The
    /// **`workspace` field on [`TraverseRequest`] is honoured** —
    /// fixing the Python `Core/harness/memory/memgraph.py::traverse`
    /// signature that silently dropped it (see
    /// `wylde_memgraph_python_client_bugs.md`).
    ///
    /// Returns `{"ok": true, "chunks": [...]}` matching the Python
    /// route's envelope.
    pub async fn traverse(&self, req: super::client::TraverseRequest) -> Reply {
        if req.entities.is_empty() {
            return Reply::ok(json!({"ok": true, "chunks": []}));
        }
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            let chunks = traverse_impl(graph, &req).await?;
            Ok::<_, (String, String)>(json!({"ok": true, "chunks": chunks}))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => Reply::ok(v),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("traverse timed out after {timeout:?}"),
            ),
        }
    }

    /// `upsert` — MERGE chunks + entities + MENTIONED_IN edges, plus
    /// any typed Entity→Entity relationships embedded in the chunks.
    /// Rust port of `_routes_core.upsert`. Returns
    /// `{"ok": true, "count": N}`.
    ///
    /// `chunks` are opaque JSON dicts shaped the same way the Python
    /// route reads them: each carries `id`, optional `path` / `symbol`
    /// / `language` / `workspace` / `entities` / `relationships`.
    pub async fn upsert(&self, chunks: Vec<Value>) -> Reply {
        if chunks.is_empty() {
            return Reply::ok(json!({"ok": true, "count": 0}));
        }
        let workspace_default = chunks
            .iter()
            .find_map(|c| c.get("workspace").and_then(Value::as_str).map(str::to_owned))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_owned());

        let (batch, typed_rels) = coerce_upsert_batch(&chunks, &workspace_default);

        let count = batch.len();
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            graph
                .run(
                    neo4rs::query(cypher::UPSERT_ENTITIES)
                        .param("batch", BoltType::List(batch_to_boltlist(&batch))),
                )
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("upsert: {e}")))?;
            for (rel_type, pairs) in &typed_rels {
                let stmt = cypher::relate_typed(rel_type);
                graph
                    .run(
                        neo4rs::query(&stmt)
                            .param("pairs", BoltType::List(pairs_to_boltlist(pairs))),
                    )
                    .await
                    .map_err(|e| {
                        (
                            error_codes::QUERY.to_owned(),
                            format!("upsert.{rel_type}: {e}"),
                        )
                    })?;
            }
            Ok::<_, (String, String)>(())
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(())) => Reply::ok(json!({"ok": true, "count": count})),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("upsert timed out after {timeout:?}"),
            ),
        }
    }

    /// `multihop` — multi-hop entity expansion. Starts from `entities`,
    /// walks `expand_hops * 2` Entity-Chunk-Entity hops to collect
    /// co-mentioned entities, then returns chunks ranked by how many
    /// of the expanded set they touch.
    ///
    /// **Bug-fix vs the Python client:** Python's
    /// `Core/harness/memory/memgraph.py::multihop` sent `start` /
    /// `max_hops` keys but the server route reads `entities` /
    /// `expand_hops`. The Rust API uses the right parameter names by
    /// construction, so the field-name bug is structurally
    /// impossible. See `wylde_memgraph_python_client_bugs.md`.
    ///
    /// Returns `{"ok": true, "expanded_entities": [...], "chunks": [...]}`
    /// matching the Python route envelope.
    pub async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        if entities.is_empty() {
            return Reply::ok(json!({
                "ok": true,
                "expanded_entities": [],
                "chunks": [],
            }));
        }
        let expand_hops = expand_hops.clamp(1, 3);
        let limit = limit.clamp(1, 100);
        let depth = expand_hops * 2;
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;

            // Step 1 — expand
            let expand_q = cypher::multihop_expand(depth);
            let mut rows = graph
                .execute(
                    neo4rs::query(&expand_q)
                        .param("names", BoltType::List(strings_to_boltlist(&entities))),
                )
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("multihop expand: {e}")))?;

            let mut expanded: Vec<String> = match rows.next().await {
                Ok(Some(row)) => row
                    .get::<Vec<String>>("names")
                    .unwrap_or_default(),
                Ok(None) => Vec::new(),
                Err(e) => {
                    return Err((
                        error_codes::DECODE.to_owned(),
                        format!("multihop expand decode: {e}"),
                    ))
                }
            };
            // Mirror Python — union with the seeds so a chunk that
            // mentions ONLY a seed (no co-mentioned neighbour) still
            // counts.
            for s in &entities {
                if !expanded.iter().any(|n| n == s) {
                    expanded.push(s.clone());
                }
            }

            if expanded.is_empty() {
                return Ok::<_, (String, String)>(json!({
                    "ok": true,
                    "expanded_entities": [],
                    "chunks": [],
                }));
            }

            // Step 2 — chunks
            let mut chunk_rows = graph
                .execute(
                    neo4rs::query(cypher::MULTIHOP_CHUNKS)
                        .param("names", BoltType::List(strings_to_boltlist(&expanded)))
                        .param("limit", BoltType::from(limit as i64)),
                )
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("multihop chunks: {e}")))?;

            let mut result: Vec<Value> = Vec::new();
            let mut rank = 0i64;
            while let Ok(Some(row)) = chunk_rows.next().await {
                let id: String = match row.get("id") {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let path: String = row.get("path").unwrap_or_default();
                let symbol: String = row.get("symbol").unwrap_or_default();
                let language: String = row.get("language").unwrap_or_default();
                let hits: i64 = row.get("hits").unwrap_or(0);
                result.push(json!({
                    "id": id,
                    "path": path,
                    "symbol": symbol,
                    "language": language,
                    "graph_score": hits as f64,
                    "graph_rank": rank,
                }));
                rank += 1;
            }

            // Python caps `expanded_entities[:60]` in the response.
            let mut expanded_out = expanded;
            expanded_out.truncate(60);

            Ok::<_, (String, String)>(json!({
                "ok": true,
                "expanded_entities": expanded_out,
                "chunks": result,
            }))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => Reply::ok(v),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("multihop timed out after {timeout:?}"),
            ),
        }
    }

    /// `relate` — write typed Entity→Entity edges. `rel_type` MUST be
    /// one of [`super::schema`]'s validated relation types
    /// (`CALLS` / `IMPORTS` / `INHERITS` / `CONFIGURES` / `EXPOSES`);
    /// anything else returns a `bad_request` envelope.
    ///
    /// Returns `{"ok": true, "written": N}` mirroring the Python route.
    pub async fn relate(&self, rel_type: &str, pairs: Vec<super::client::EntityPair>) -> Reply {
        if pairs.is_empty() {
            return Reply::ok(json!({"ok": true, "written": 0}));
        }
        let rel_type = rel_type.trim().to_uppercase();
        if !rel_schema::relation_type_is_valid(&rel_type) {
            return Reply::err_msg(
                "bad_request",
                format!("rel_type {rel_type:?} not in vocabulary"),
            );
        }
        let edges: Vec<EntityEdge> = pairs
            .into_iter()
            .filter_map(|p| {
                let src = p.source.trim().to_owned();
                let tgt = p.target.trim().to_owned();
                if src.is_empty() || tgt.is_empty() || src == tgt {
                    None
                } else {
                    Some(EntityEdge { source: src, target: tgt })
                }
            })
            .collect();
        if edges.is_empty() {
            return Reply::ok(json!({"ok": true, "written": 0}));
        }
        let count = edges.len();
        let stmt = cypher::relate_typed(&rel_type);
        let payload = BoltType::List(pairs_to_boltlist(&edges));
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            graph
                .run(neo4rs::query(&stmt).param("pairs", payload))
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("relate: {e}")))?;
            Ok::<_, (String, String)>(())
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(())) => Reply::ok(json!({"ok": true, "written": count})),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("relate timed out after {timeout:?}"),
            ),
        }
    }

    /// `unrelate` — delete typed Entity→Entity edges. Same validation
    /// rules as [`Self::relate`]. Returns
    /// `{"ok": true, "deleted": N}`.
    pub async fn unrelate(&self, rel_type: &str, pairs: Vec<super::client::EntityPair>) -> Reply {
        if pairs.is_empty() {
            return Reply::ok(json!({"ok": true, "deleted": 0}));
        }
        let rel_type = rel_type.trim().to_uppercase();
        if !rel_schema::relation_type_is_valid(&rel_type) {
            return Reply::err_msg(
                "bad_request",
                format!("rel_type {rel_type:?} not in vocabulary"),
            );
        }
        let edges: Vec<EntityEdge> = pairs
            .into_iter()
            .filter_map(|p| {
                let src = p.source.trim().to_owned();
                let tgt = p.target.trim().to_owned();
                if src.is_empty() || tgt.is_empty() {
                    None
                } else {
                    Some(EntityEdge { source: src, target: tgt })
                }
            })
            .collect();
        if edges.is_empty() {
            return Reply::ok(json!({"ok": true, "deleted": 0}));
        }
        let count = edges.len();
        let stmt = cypher::unrelate_typed(&rel_type);
        let payload = BoltType::List(pairs_to_boltlist(&edges));
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            graph
                .run(neo4rs::query(&stmt).param("pairs", payload))
                .await
                .map_err(|e| (error_codes::QUERY.to_owned(), format!("unrelate: {e}")))?;
            Ok::<_, (String, String)>(())
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(())) => Reply::ok(json!({"ok": true, "deleted": count})),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("unrelate timed out after {timeout:?}"),
            ),
        }
    }

    /// `upsert_edge` — MERGE-style weighted edge upsert. Used by the
    /// RAG feedback loop: a successful cited retrieval strengthens
    /// the `source -[label]-> target` edge, a miss leaves a
    /// low-weight trail. Returns `{"ok": true}`.
    pub async fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> Reply {
        let label = label.trim().to_uppercase();
        if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Reply::err_msg("bad_request", format!("invalid edge label {label:?}"));
        }
        let stmt = cypher::upsert_edge(&label);
        let source = source.to_owned();
        let target = target.to_owned();
        self.run_void(&format!("upsert_edge({label})"), move |graph| {
            let stmt = stmt.clone();
            let source = source.clone();
            let target = target.clone();
            Box::pin(async move {
                graph
                    .run(
                        neo4rs::query(&stmt)
                            .param("source", BoltType::from(source))
                            .param("target", BoltType::from(target))
                            .param("weight_delta", BoltType::from(weight_delta)),
                    )
                    .await
                    .map_err(|e| (error_codes::QUERY.to_owned(), format!("upsert_edge: {e}")))
            })
        })
        .await
    }

    /// `stats` — five counts: entities, chunks, mentions, communities,
    /// typed_relationships. Issued as separate queries because Python
    /// does the same and a single multi-statement query would lock the
    /// reader across statements unnecessarily.
    pub async fn stats(&self) -> Reply {
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            let entities = scalar_count(graph, cypher::stats::COUNT_ENTITIES).await?;
            let chunks = scalar_count(graph, cypher::stats::COUNT_CHUNKS).await?;
            let mentions = scalar_count(graph, cypher::stats::COUNT_MENTIONS).await?;
            let communities = scalar_count(graph, cypher::stats::COUNT_COMMUNITIES).await?;
            let typed = scalar_count(graph, cypher::stats::COUNT_TYPED_RELATIONSHIPS).await?;
            Ok::<_, (String, String)>(json!({
                "ok": true,
                "entities": entities,
                "chunks": chunks,
                "mentions": mentions,
                "communities": communities,
                "typed_relationships": typed,
            }))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => Reply::ok(v),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("stats timed out after {timeout:?}"),
            ),
        }
    }

    /// Internal helper for "run a query that has no return value". Wraps
    /// the connect-or-error / timeout boilerplate so verb bodies stay
    /// readable.
    async fn run_void<F>(&self, label: &str, run: F) -> Reply
    where
        F: for<'a> FnOnce(
            &'a Graph,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), (String, String)>>
                    + Send
                    + 'a,
            >,
        >,
    {
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            run(graph).await
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(())) => Reply::ok(json!({"ok": true})),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("{label} timed out after {timeout:?}"),
            ),
        }
    }
}

/// One row of the upsert $batch. Mirrors the row shape Python's
/// `_routes_core.upsert` builds before binding.
#[derive(Clone, Debug)]
struct UpsertRow {
    id: String,
    path: String,
    symbol: String,
    language: String,
    entities: Vec<String>,
    workspace: String,
}

/// One typed Entity→Entity edge pair (`{"source": ..., "target": ...}`)
/// after rel-type validation.
#[derive(Clone, Debug)]
struct EntityEdge {
    source: String,
    target: String,
}

/// Coerce the caller-provided chunk JSON values into the upsert batch
/// shape + grouped typed-rel buckets. Pulled out so unit tests can
/// pin the coercion rules without needing a live Neo4j.
fn coerce_upsert_batch(
    chunks: &[Value],
    workspace_default: &str,
) -> (Vec<UpsertRow>, std::collections::BTreeMap<String, Vec<EntityEdge>>) {
    use std::collections::BTreeMap;

    let mut batch: Vec<UpsertRow> = Vec::with_capacity(chunks.len());
    let mut typed: BTreeMap<String, Vec<EntityEdge>> = BTreeMap::new();
    for c in chunks {
        let id = c
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if id.is_empty() {
            // Same as Python's missing-id row: it would KeyError on
            // c["id"]. We drop the row instead of crashing.
            continue;
        }
        let row = UpsertRow {
            id,
            path: c
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            symbol: c
                .get("symbol")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            language: c
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            entities: c
                .get("entities")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            workspace: c
                .get("workspace")
                .and_then(Value::as_str)
                .map(|s| s.to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| workspace_default.to_owned()),
        };
        batch.push(row);

        if let Some(rels) = c.get("relationships").and_then(Value::as_array) {
            for rel in rels {
                let rt = rel
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_uppercase();
                let src = rel
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                let tgt = rel
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if !rel_schema::relation_type_is_valid(&rt)
                    || src.is_empty()
                    || tgt.is_empty()
                    || src == tgt
                {
                    continue;
                }
                typed
                    .entry(rt)
                    .or_default()
                    .push(EntityEdge { source: src, target: tgt });
            }
        }
    }
    (batch, typed)
}

fn batch_to_boltlist(batch: &[UpsertRow]) -> BoltList {
    let mut list = BoltList::new();
    for row in batch {
        let mut m = BoltMap::new();
        m.put("id".into(), BoltType::from(row.id.clone()));
        m.put("path".into(), BoltType::from(row.path.clone()));
        m.put("symbol".into(), BoltType::from(row.symbol.clone()));
        m.put("language".into(), BoltType::from(row.language.clone()));
        m.put("workspace".into(), BoltType::from(row.workspace.clone()));
        let mut ents = BoltList::new();
        for e in &row.entities {
            ents.push(BoltType::from(e.clone()));
        }
        m.put("entities".into(), BoltType::List(ents));
        list.push(BoltType::Map(m));
    }
    list
}

fn pairs_to_boltlist(pairs: &[EntityEdge]) -> BoltList {
    let mut list = BoltList::new();
    for p in pairs {
        let mut m = BoltMap::new();
        m.put("source".into(), BoltType::from(p.source.clone()));
        m.put("target".into(), BoltType::from(p.target.clone()));
        list.push(BoltType::Map(m));
    }
    list
}

/// `traverse` body — extracted so it can run inside the timeout
/// wrapper without dragging the verb body further to the right.
async fn traverse_impl(
    graph: &Graph,
    req: &super::client::TraverseRequest,
) -> Result<Vec<Value>, (String, String)> {
    use std::collections::BTreeMap;

    let workspace = req
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let decay_alpha = req.decay_alpha.unwrap_or(0.4);

    // Python clamps both bucket depths to `min(rel_depths[k], max_hops, 4)`.
    let (depth_calls, depth_cfg) = bucket_depths(req);

    let names: Vec<String> = req.entities.clone();
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();

    for (cypher_text, bucket_name, depth) in [
        (
            cypher::traverse_bucket(cypher::REL_ALT_CALLS, depth_calls, workspace.is_some()),
            "calls_imports",
            depth_calls,
        ),
        (
            cypher::traverse_bucket(cypher::REL_ALT_CFG, depth_cfg, workspace.is_some()),
            "configures_exposes",
            depth_cfg,
        ),
    ] {
        // depth==0 with a high max_hops still issues the query — the
        // Cypher quantifier `*0..0` matches the seed itself only.
        let _ = depth; // currently unused after Cypher build, kept for clarity
        let mut q = neo4rs::query(&cypher_text)
            .param("names", BoltType::List(strings_to_boltlist(&names)));
        if let Some(ws) = workspace {
            q = q.param("ws", BoltType::from(ws.to_owned()));
        }
        let mut rows = match graph.execute(q).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(bucket = bucket_name, error = %e, "traverse bucket failed");
                continue;
            }
        };
        while let Ok(Some(row)) = rows.next().await {
            let id: String = match row.get("id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            if id.is_empty() {
                continue;
            }
            let path: String = row.get("path").unwrap_or_default();
            let symbol: String = row.get("symbol").unwrap_or_default();
            let language: String = row.get("language").unwrap_or_default();
            let best_depth: i64 = row.get("best_depth").unwrap_or(0);
            let seeds: f64 = row.get::<i64>("seeds_touching").map(|n| n as f64).unwrap_or(0.0);
            let typed_depth = best_depth.max(0) as u32;
            let decay = (-decay_alpha * typed_depth as f64).exp();
            let score = seeds * decay;
            let entry = json!({
                "id": id,
                "path": path,
                "symbol": symbol,
                "language": language,
                "best_depth": typed_depth,
                "seeds_touching": seeds,
                "decay": decay,
                "bucket": bucket_name,
                "graph_score": score,
            });
            let cur = merged.get(&id).and_then(|v| v.get("graph_score")).and_then(Value::as_f64);
            if cur.map(|s| score > s).unwrap_or(true) {
                merged.insert(id, entry);
            }
        }
    }

    // Rank: descending by graph_score, then by best_depth asc, then by path.
    let mut ranked: Vec<Value> = merged.into_values().collect();
    ranked.sort_by(|a, b| {
        let sa = a["graph_score"].as_f64().unwrap_or(0.0);
        let sb = b["graph_score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let da = a["best_depth"].as_u64().unwrap_or(0);
                let db = b["best_depth"].as_u64().unwrap_or(0);
                da.cmp(&db)
            })
            .then_with(|| {
                let pa = a["path"].as_str().unwrap_or("");
                let pb = b["path"].as_str().unwrap_or("");
                pa.cmp(pb)
            })
    });
    let limit = (req.limit as usize).max(1);
    ranked.truncate(limit);

    let chunks: Vec<Value> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "id": r["id"],
                "path": r["path"],
                "symbol": r["symbol"],
                "language": r["language"],
                "graph_rank": i,
                "graph_score": r["graph_score"],
                "graph_depth": r["best_depth"],
                "graph_bucket": r["bucket"],
            })
        })
        .collect();
    Ok(chunks)
}

fn strings_to_boltlist(items: &[String]) -> BoltList {
    let mut list = BoltList::new();
    for s in items {
        list.push(BoltType::from(s.clone()));
    }
    list
}

/// Clamp the per-bucket traversal depths against `max_hops` and the
/// hard ceiling 4 — mirrors Python's
/// `max(0, min(int(rel_depths.get(...)), max_hops, 4))`.
fn bucket_depths(req: &super::client::TraverseRequest) -> (u32, u32) {
    let max_hops = req.max_hops;
    let (mut depth_calls, mut depth_cfg) = (1u32, 2u32);
    if let Some(rd) = &req.rel_depths {
        for (bucket, d) in rd {
            match bucket.as_str() {
                "calls_imports" => depth_calls = *d,
                "configures_exposes" => depth_cfg = *d,
                _ => {}
            }
        }
    }
    (depth_calls.min(max_hops).min(4), depth_cfg.min(max_hops).min(4))
}

/// `run_single_count` — common Cypher pattern: run a write that
/// returns a single `RETURN n` row, surface `n` as an i64. Used by
/// `delete_workspace` for its two-step Chunk + Entity prune.
async fn run_single_count(
    graph: &Graph,
    cypher_text: &str,
    params: Vec<(String, BoltType)>,
) -> Result<i64, (String, String)> {
    let mut q = neo4rs::query(cypher_text);
    for (k, v) in params {
        q = q.param(&k, v);
    }
    let mut rows = graph
        .execute(q)
        .await
        .map_err(|e| (error_codes::QUERY.to_owned(), format!("count: {e}")))?;
    match rows.next().await {
        Ok(Some(row)) => Ok(row.get::<i64>("n").unwrap_or(0)),
        Ok(None) => Ok(0),
        Err(e) => Err((error_codes::DECODE.to_owned(), format!("count decode: {e}"))),
    }
}

/// Single-scalar count read — same pattern as [`run_single_count`]
/// but for read-only queries like the `/stats` counts.
async fn scalar_count(graph: &Graph, cypher_text: &str) -> Result<i64, (String, String)> {
    run_single_count(graph, cypher_text, vec![]).await
}

impl Default for BoltClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot + restore guard for an environment variable. Several
/// tests below mutate `GRAPH_*` env vars; this helper keeps them from
/// leaking into sibling tests. Lives in `super::common::TEST_ENV_LOCK`
/// — every test that mutates env locks that mutex first.
#[cfg(test)]
pub(crate) struct EnvGuard {
    name: &'static str,
    prev: Option<String>,
}

#[cfg(test)]
impl EnvGuard {
    /// Snapshot the current value, then `set` for the duration of the
    /// guard.
    pub fn set(name: &'static str, value: &str) -> Self {
        let prev = std::env::var(name).ok(); // wylde-check: discard-result-ok
        std::env::set_var(name, value);
        Self { name, prev }
    }

    /// Snapshot, then `remove`.
    pub fn remove(name: &'static str) -> Self {
        let prev = std::env::var(name).ok(); // wylde-check: discard-result-ok
        std::env::remove_var(name);
        Self { name, prev }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.name, v),
            None => std::env::remove_var(self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test env serialisation — every test in this module that
    /// pokes `GRAPH_*` env vars locks this first.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::memory::common::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn config_from_env_falls_back_to_python_defaults() {
        let _g = env_lock();
        let _u = EnvGuard::remove("GRAPH_BOLT_URL");
        let _user = EnvGuard::remove("GRAPH_USER");
        let _pw = EnvGuard::remove("GRAPH_PASSWORD");
        let _t = EnvGuard::remove("WYLDE_BOLT_CONNECT_TIMEOUT_SECS");
        let cfg = BoltConfig::from_env();
        assert_eq!(cfg.uri, DEFAULT_BOLT_URL);
        assert!(cfg.user.is_empty());
        assert!(cfg.password.is_empty());
        assert_eq!(cfg.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
    }

    #[test]
    fn config_from_env_reads_overrides() {
        let _g = env_lock();
        let _u = EnvGuard::set("GRAPH_BOLT_URL", "bolt://example.invalid:9999");
        let _user = EnvGuard::set("GRAPH_USER", "u");
        let _pw = EnvGuard::set("GRAPH_PASSWORD", "p");
        let _t = EnvGuard::set("WYLDE_BOLT_CONNECT_TIMEOUT_SECS", "1.5");
        let cfg = BoltConfig::from_env();
        assert_eq!(cfg.uri, "bolt://example.invalid:9999");
        assert_eq!(cfg.user, "u");
        assert_eq!(cfg.password, "p");
        assert_eq!(cfg.connect_timeout, Duration::from_secs_f64(1.5));
    }

    #[tokio::test]
    async fn health_returns_bolt_error_against_unreachable_uri() {
        // Deliberately unreachable host with a short connect-timeout
        // so the test fails the verb in ~1s, not in neo4rs's internal
        // retry budget. `bolt_connect` (eager pool open failed) and
        // `bolt_query` (lazy pool, first execute() got IO refused) are
        // both acceptable — the precise stage depends on neo4rs
        // patch-level behaviour.
        let client = BoltClient::with_config(BoltConfig {
            uri: "bolt://127.0.0.1:1".to_owned(),
            user: String::new(),
            password: String::new(),
            connect_timeout: Duration::from_secs(1),
        });
        let reply = client.health().await;
        assert!(!reply.ok, "health against dead URI must be !ok");
        let err = reply.error.expect("error envelope");
        assert!(
            err.code == error_codes::CONNECT || err.code == error_codes::QUERY,
            "expected bolt_connect or bolt_query, got {err:?}"
        );
    }

    /// Sanity-check that the public surface is `Send + Sync`
    /// (the harness shares clients across tasks).
    #[test]
    fn client_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BoltClient>();
    }

    // ── coerce_upsert_batch (pure, no Neo4j needed) ─────────────────

    #[test]
    fn coerce_drops_rows_with_empty_id() {
        let chunks = vec![
            json!({"id": "c1", "entities": ["foo"]}),
            json!({"id": "", "entities": ["bar"]}),    // dropped
            json!({"entities": ["baz"]}),               // dropped (no id)
            json!({"id": "c4"}),
        ];
        let (batch, _) = coerce_upsert_batch(&chunks, "default");
        let ids: Vec<&str> = batch.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c4"]);
    }

    #[test]
    fn coerce_defaults_workspace_from_route_default_when_chunk_omits_it() {
        let chunks = vec![
            json!({"id": "c1"}),
            json!({"id": "c2", "workspace": "ws-explicit"}),
            json!({"id": "c3", "workspace": ""}),  // empty -> route default
        ];
        let (batch, _) = coerce_upsert_batch(&chunks, "route-default");
        assert_eq!(batch[0].workspace, "route-default");
        assert_eq!(batch[1].workspace, "ws-explicit");
        assert_eq!(batch[2].workspace, "route-default");
    }

    #[test]
    fn coerce_filters_invalid_typed_relationships() {
        let chunks = vec![json!({
            "id": "c1",
            "relationships": [
                {"type": "CALLS", "source": "a", "target": "b"},
                {"type": "calls", "source": "c", "target": "d"},    // case
                {"type": "CALLS", "source": "x", "target": "x"},    // self
                {"type": "CALLS", "source": "", "target": "z"},     // empty
                {"type": "RANDOM", "source": "p", "target": "q"},   // not in vocab
                {"type": "IMPORTS", "source": " a ", "target": " b "}, // trim
            ]
        })];
        let (_, typed) = coerce_upsert_batch(&chunks, "default");
        // CALLS is normalised so "calls" becomes "CALLS" — that means
        // we have CALLS: 2 valid pairs (the original lowercase one is
        // upper-cased and ends up here), IMPORTS: 1.
        let calls = typed.get("CALLS").expect("CALLS bucket");
        assert_eq!(calls.len(), 2, "{calls:?}");
        let imports = typed.get("IMPORTS").expect("IMPORTS bucket");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].source, "a");
        assert_eq!(imports[0].target, "b");
        assert!(!typed.contains_key("RANDOM"), "RANDOM must be rejected");
    }

    #[test]
    fn coerce_extracts_entities_as_strings_only() {
        let chunks = vec![json!({
            "id": "c1",
            "entities": ["foo", 42, null, "bar"],
        })];
        let (batch, _) = coerce_upsert_batch(&chunks, "default");
        assert_eq!(batch[0].entities, vec!["foo".to_owned(), "bar".to_owned()]);
    }

    // ── bucket_depths (pure) ────────────────────────────────────────

    fn req_with(
        max_hops: u32,
        depths: Option<Vec<(&str, u32)>>,
    ) -> crate::memory::memgraph::client::TraverseRequest {
        crate::memory::memgraph::client::TraverseRequest {
            entities: vec!["seed".into()],
            max_hops,
            limit: 10,
            workspace: None,
            decay_alpha: None,
            rel_depths: depths.map(|d| {
                d.into_iter()
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect()
            }),
        }
    }

    #[test]
    fn bucket_depths_default_one_and_two_when_no_overrides() {
        let req = req_with(3, None);
        assert_eq!(bucket_depths(&req), (1, 2));
    }

    #[test]
    fn bucket_depths_clamp_against_max_hops() {
        let req = req_with(1, Some(vec![("calls_imports", 3), ("configures_exposes", 3)]));
        assert_eq!(bucket_depths(&req), (1, 1));
    }

    #[test]
    fn bucket_depths_clamp_against_hard_ceiling_of_four() {
        let req = req_with(10, Some(vec![("calls_imports", 99), ("configures_exposes", 99)]));
        assert_eq!(bucket_depths(&req), (4, 4));
    }

    #[test]
    fn bucket_depths_unknown_buckets_dont_replace_defaults() {
        let req = req_with(5, Some(vec![("weird_bucket", 9)]));
        assert_eq!(bucket_depths(&req), (1, 2));
    }
}
