//! Direct-to-Neo4j Bolt client for the workspace graph-ingest writes.
//!
//! A narrow relocation of the harness `memory::memgraph::bolt` (Slice 0b):
//! only the verbs the workspace ingest + cleanup paths use — `upsert`,
//! `relate`, and `delete_workspace`. The harness keeps its full client (read
//! traversals, multihop, stats, …) for its own memory layer; this is the
//! workspace service's own write surface to the same Neo4j over Bolt.
//!
//! Env knobs mirror the harness/Python client so both reach the same DB:
//!   * `GRAPH_BOLT_URL` — default `bolt://127.0.0.1:7687`.
//!   * `GRAPH_USER` / `GRAPH_PASSWORD` — default empty (auth disabled).
//!   * `WYLDE_BOLT_CONNECT_TIMEOUT_SECS` — default 5s per attempt.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use neo4rs::{BoltList, BoltMap, BoltType, ConfigBuilder, Graph};
use serde_json::{json, Value};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::OnceCell;
use wylde_shared::ipc::{IpcError, Reply};

use super::cypher;
use super::query::{self, EdgeRow, GraphRows, NodeRow};
use super::schema as rel_schema;
use super::EntityPair;

/// Default Bolt URL — matches the harness/Python default.
pub const DEFAULT_BOLT_URL: &str = "bolt://127.0.0.1:7687";

/// Default per-attempt connect timeout. Override via
/// `WYLDE_BOLT_CONNECT_TIMEOUT_SECS`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default per-query timeout for a graph WRITE. Distinct from the 5s
/// connect timeout: a whole-repo `upsert` MERGEs thousands of Chunk +
/// Entity nodes, which takes far longer than connecting. The old code
/// bounded the entire upsert by `connect_timeout`, so a real index — e.g.
/// 16k chunks in one UNWIND — always timed out at 5s and wrote ZERO Chunk
/// nodes (the Workspaces graph tab then read back empty even with Neo4j up
/// and the vector index fully built). Batched writes (see [`WRITE_BATCH`])
/// keep each query small; this is the per-batch budget. Override via
/// `WYLDE_BOLT_WRITE_TIMEOUT_SECS`.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Max rows per write query. A single UNWIND over the whole repo is both
/// slow (one statement blocks until every MERGE lands) and memory-heavy on
/// the server; splitting into bounded batches keeps each query inside the
/// write timeout and lets large repos make steady progress. Override via
/// `WYLDE_BOLT_WRITE_BATCH`.
pub const WRITE_BATCH: usize = 500;

/// One statement in the workspace-teardown cascade.
pub(crate) struct TeardownStep {
    /// Result key the deleted-row count is reported under.
    pub key: &'static str,
    /// The statement to run.
    pub cypher: &'static str,
    /// Whether the statement takes a `$ws` parameter. The orphan-entity prune
    /// is global (it matches every Entity with no surviving mention), so it
    /// takes none.
    pub ws_scoped: bool,
}

/// The ordered statement sequence ONE full workspace teardown runs.
///
/// Declared once and consumed twice: [`BoltClient::delete_workspace`] executes
/// it against the live graph, and the in-memory teardown mock in
/// [`super::cleanup`]'s tests replays it against a fake node store. That shared
/// declaration is the point — it makes the mock's coverage track the real
/// cascade instead of restating it. Removing a step here deletes it from both
/// sides at once, which is what lets a unit test catch a missing sweep.
///
/// Concepts go first (#117): they were previously absent from the cascade
/// entirely, so a deleted workspace left its `Concept` nodes and `CHILD_OF`
/// edges behind forever, their `MEMBER` targets torn out by the entity prune.
pub(crate) const WORKSPACE_TEARDOWN_STEPS: &[TeardownStep] = &[
    TeardownStep {
        key: "concepts_deleted",
        cypher: cypher::DELETE_WORKSPACE_CONCEPTS,
        ws_scoped: true,
    },
    TeardownStep {
        key: "chunks_deleted",
        cypher: cypher::DELETE_WORKSPACE_CHUNKS,
        ws_scoped: true,
    },
    TeardownStep {
        key: "orphan_entities_deleted",
        cypher: cypher::DELETE_ORPHAN_ENTITIES,
        ws_scoped: false,
    },
];

fn write_batch_size() -> usize {
    std::env::var("WYLDE_BOLT_WRITE_BATCH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(WRITE_BATCH)
}

/// Run one write query bounded by `timeout`. Shared by the batched
/// `upsert` / `relate` loops so each sub-batch gets the full budget.
async fn run_timed(
    graph: &Graph,
    q: neo4rs::Query,
    timeout: Duration,
    what: &str,
) -> Result<(), (String, String)> {
    match tokio::time::timeout(timeout, graph.run(q)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err((error_codes::QUERY.to_owned(), format!("{what}: {e}"))),
        Err(_) => Err((
            error_codes::QUERY.to_owned(),
            format!("{what} timed out after {timeout:?}"),
        )),
    }
}

/// Process-wide cache of live `Graph` pools, keyed by `(uri, user)`.
///
/// One pool per distinct backend, shared by every [`BoltClient`] so a
/// reindex graph-write, a `workspaces.graph` read, and a watcher delta all
/// reuse the same keep-alive connection pool instead of each opening and
/// dropping their own (which churned `:7687` sockets into `TIME_WAIT`).
/// The cached `Graph` is never evicted — it lives for the process, exactly
/// like the wylde-ollama upstream HTTP client.
static SHARED_GRAPHS: OnceLock<TokioMutex<HashMap<String, Graph>>> = OnceLock::new();

/// Return a clone of the shared `Graph` for `cfg`'s backend, connecting
/// (once per distinct backend) on first use. The returned `Graph` is an
/// `Arc` handle over the pool; cloning it shares the live connections.
async fn shared_graph(cfg: &BoltConfig) -> Result<Graph, IpcError> {
    let key = format!("{}|{}", cfg.uri, cfg.user);
    let cache = SHARED_GRAPHS.get_or_init(|| TokioMutex::new(HashMap::new()));
    // Hold the async mutex across the (rare) connect so two racing first
    // callers can't open two pools for the same backend. Steady-state this
    // is an uncontended lock + HashMap hit, never a connect.
    let mut guard = cache.lock().await;
    if let Some(g) = guard.get(&key) {
        return Ok(g.clone());
    }
    let neo_cfg = ConfigBuilder::default()
        .uri(cfg.uri.clone())
        .user(cfg.user.clone())
        .password(cfg.password.clone())
        .build()
        .map_err(|e| IpcError::new(error_codes::CONFIG, format!("config: {e}")))?;
    let graph = Graph::connect(neo_cfg)
        .await
        .map_err(|e| IpcError::new(error_codes::CONNECT, format!("{}: {}", cfg.uri, e)))?;
    guard.insert(key, graph.clone());
    Ok(graph)
}

/// `bolt_*` error codes surfaced via [`Reply::err_msg`].
pub mod error_codes {
    pub const CONFIG: &str = "bolt_config";
    pub const CONNECT: &str = "bolt_connect";
    pub const QUERY: &str = "bolt_query";
    pub const DECODE: &str = "bolt_decode";
}

/// Static configuration captured at [`BoltClient::new`] time.
#[derive(Clone, Debug)]
pub struct BoltConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub connect_timeout: Duration,
    /// Per-query budget for a write (see [`DEFAULT_WRITE_TIMEOUT`]).
    pub write_timeout: Duration,
}

impl BoltConfig {
    /// Build a config from env, falling back to the shared defaults.
    pub fn from_env() -> Self {
        let connect_timeout = std::env::var("WYLDE_BOLT_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(Duration::from_secs_f64)
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let write_timeout = std::env::var("WYLDE_BOLT_WRITE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(Duration::from_secs_f64)
            .unwrap_or(DEFAULT_WRITE_TIMEOUT);
        Self {
            uri: std::env::var("GRAPH_BOLT_URL").unwrap_or_else(|_| DEFAULT_BOLT_URL.to_owned()),
            user: std::env::var("GRAPH_USER").unwrap_or_default(),
            password: std::env::var("GRAPH_PASSWORD").unwrap_or_default(),
            connect_timeout,
            write_timeout,
        }
    }
}

/// Bolt-backed graph write client. Lazily opens one [`Graph`] (neo4rs pool)
/// on the first verb; subsequent calls reuse it.
pub struct BoltClient {
    config: BoltConfig,
    graph: OnceCell<Graph>,
}

impl BoltClient {
    /// Build a client wired to the env-var defaults.
    pub fn new() -> Self {
        Self::with_config(BoltConfig::from_env())
    }

    /// Build a client around a fully-specified [`BoltConfig`].
    pub fn with_config(config: BoltConfig) -> Self {
        Self {
            config,
            graph: OnceCell::new(),
        }
    }

    /// Snapshot of the captured config — for diagnostics.
    pub fn config(&self) -> &BoltConfig {
        &self.config
    }

    /// Acquire (or open) the underlying [`Graph`].
    ///
    /// The `Graph` is **not** opened fresh per [`BoltClient`]: a new client
    /// is constructed on *every* `workspaces.graph` read, every reindex
    /// graph-write pass, and every watcher delta, so a per-instance pool
    /// opened then dropped on each call left a pool's worth of sockets in
    /// `TIME_WAIT` every time the client went out of scope. Under a large
    /// reindex with the GUI polling `workspaces.graph`, that per-call churn
    /// piled up Bolt (`:7687`) sockets in the OS ephemeral-port table and
    /// helped exhaust it (the embed path then failed to get a source port —
    /// see `outputs/embed-pooling-fix-report.md`).
    ///
    /// Instead every client of the same (uri, user) shares ONE long-lived
    /// `Graph` from a process-wide cache. `Graph` is a cheap `Arc` handle
    /// over a connection pool, so the per-instance `OnceCell` just memoises
    /// a clone of the shared pool; dropping a `BoltClient` drops the clone,
    /// never the pooled connections. Keep-alive + reuse across thousands of
    /// reads/writes then holds the live socket count to the pool size
    /// regardless of OS — the same fix the wylde-ollama HTTP client already
    /// applies to `:11434`.
    async fn graph(&self) -> Result<&Graph, IpcError> {
        self.graph
            .get_or_try_init(|| shared_graph(&self.config))
            .await
    }

    /// `upsert` — MERGE chunks + entities + MENTIONED_IN edges, plus any
    /// typed Entity→Entity relationships embedded in the chunks. Returns
    /// `{"ok": true, "count": N}`.
    pub async fn upsert(&self, chunks: Vec<Value>) -> Reply {
        if chunks.is_empty() {
            return Reply::ok(json!({"ok": true, "count": 0}));
        }
        let workspace_default = chunks
            .iter()
            .find_map(|c| {
                c.get("workspace")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_owned());

        let (batch, typed_rels) = coerce_upsert_batch(&chunks, &workspace_default);

        let count = batch.len();
        let write_timeout = self.config.write_timeout;
        let bs = write_batch_size();
        // Batched so a whole-repo upsert never blocks on one giant UNWIND
        // (which used to time out at 5s and write nothing). Each sub-batch
        // gets the full write budget; total time is unbounded by design —
        // graph-write is a fail-soft background pass.
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            for sub in batch.chunks(bs) {
                run_timed(
                    graph,
                    neo4rs::query(cypher::UPSERT_ENTITIES)
                        .param("batch", BoltType::List(batch_to_boltlist(sub))),
                    write_timeout,
                    "upsert",
                )
                .await?;
            }
            for (rel_type, pairs) in &typed_rels {
                let stmt = cypher::relate_typed(rel_type);
                for sub in pairs.chunks(bs) {
                    run_timed(
                        graph,
                        neo4rs::query(&stmt).param("pairs", BoltType::List(pairs_to_boltlist(sub))),
                        write_timeout,
                        &format!("upsert.{rel_type}"),
                    )
                    .await?;
                }
            }
            Ok::<_, (String, String)>(())
        };
        match fut.await {
            Ok(()) => Reply::ok(json!({"ok": true, "count": count})),
            Err((code, message)) => Reply::err_msg(code, message),
        }
    }

    /// `relate` — write typed Entity→Entity edges. `rel_type` MUST be a
    /// validated relation type. Returns `{"ok": true, "written": N}`.
    pub async fn relate(&self, rel_type: &str, pairs: Vec<EntityPair>) -> Reply {
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
                    Some(EntityEdge {
                        source: src,
                        target: tgt,
                    })
                }
            })
            .collect();
        if edges.is_empty() {
            return Reply::ok(json!({"ok": true, "written": 0}));
        }
        let count = edges.len();
        let stmt = cypher::relate_typed(&rel_type);
        let write_timeout = self.config.write_timeout;
        let bs = write_batch_size();
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            for sub in edges.chunks(bs) {
                run_timed(
                    graph,
                    neo4rs::query(&stmt).param("pairs", BoltType::List(pairs_to_boltlist(sub))),
                    write_timeout,
                    "relate",
                )
                .await?;
            }
            Ok::<_, (String, String)>(())
        };
        match fut.await {
            Ok(()) => Reply::ok(json!({"ok": true, "written": count})),
            Err((code, message)) => Reply::err_msg(code, message),
        }
    }

    /// `project_concepts` (TBS concept-system, thesis §2.2) — additively sync a
    /// workspace's authoritative concept set into the graph as `Concept` nodes +
    /// `MEMBER`/`CHILD_OF` edges, so the graph panel can render them. Replaces
    /// the workspace's prior projection (delete-then-upsert) to stay in step
    /// with the JSON store. Best-effort enrichment: the JSON store is the read
    /// path, so a failure here never blocks the build verb — callers log and
    /// move on. Returns `{ok, projected}`.
    ///
    /// `rows` are caller-built `{id, label, description, source, members:[name],
    /// parents:[id]}` maps (one per concept).
    pub async fn project_concepts(&self, workspace: &str, rows: Vec<Value>) -> Reply {
        let ws = workspace.trim().to_owned();
        if ws.is_empty() {
            return Reply::err_msg("bad_request", "'workspace' required");
        }
        let count = rows.len();
        let write_timeout = self.config.write_timeout;
        let bs = write_batch_size();
        let batch = concepts_to_boltlist(&ws, &rows);
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            // Clear the prior projection for this workspace, then re-upsert.
            run_timed(
                graph,
                neo4rs::query(cypher::DELETE_WORKSPACE_CONCEPTS).param("ws", ws.clone()),
                write_timeout,
                "project_concepts.delete",
            )
            .await?;
            for sub in batch.iter().collect::<Vec<_>>().chunks(bs) {
                let mut list = BoltList::new();
                for item in sub {
                    list.push((*item).clone());
                }
                run_timed(
                    graph,
                    neo4rs::query(cypher::UPSERT_CONCEPTS).param("batch", BoltType::List(list)),
                    write_timeout,
                    "project_concepts.upsert",
                )
                .await?;
            }
            Ok::<_, (String, String)>(())
        };
        match fut.await {
            Ok(()) => Reply::ok(json!({"ok": true, "projected": count})),
            Err((code, message)) => Reply::err_msg(code, message),
        }
    }

    /// `delete_workspace` — drop a workspace's whole graph footprint: its
    /// `Concept` projection, then every Chunk, then the now-orphaned Entity
    /// nodes. Returns counts so callers can confirm the cleanup landed. The
    /// full workspace-teardown cascade (delete + MRU eviction, #99) uses this.
    ///
    /// The concept sweep (#117) belongs to **teardown only**, which is why it
    /// lives here and not in [`delete_workspace_chunks`]: that primitive is
    /// shared with the full-reindex replace path, where the workspace lives on
    /// and its authored concepts MUST survive the recompute.
    ///
    /// Without it, deleting a workspace left its `Concept` nodes — and the
    /// `CHILD_OF` edges between them — in the graph forever. The orphan-entity
    /// prune `DETACH DELETE`s the entities a concept pointed at, so the
    /// surviving concepts were left with their `MEMBER` targets torn out: a
    /// workspace-shaped island of unreachable nodes that nothing ever collects.
    ///
    /// Concepts are swept *first* so a partial failure leaves the id queued for
    /// the next drain rather than reporting a teardown that only half landed.
    /// Every step is idempotent, so the retry is safe.
    ///
    /// The step list itself is [`WORKSPACE_TEARDOWN_STEPS`] — declared once so
    /// the in-memory teardown mock in [`super::cleanup`] replays the same
    /// sequence. A step dropped from that table stops being executed *and*
    /// stops being simulated, so the unit regression test fails with it.
    pub async fn delete_workspace(&self, workspace: &str) -> Reply {
        let ws = workspace.trim().to_owned();
        if ws.is_empty() {
            return Reply::err_msg("bad_request", "'workspace' required");
        }
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            let mut out = serde_json::Map::new();
            out.insert("ok".to_owned(), json!(true));
            out.insert("workspace".to_owned(), json!(ws));
            for step in WORKSPACE_TEARDOWN_STEPS {
                let params = if step.ws_scoped {
                    vec![("ws".to_owned(), BoltType::from(ws.clone()))]
                } else {
                    vec![]
                };
                let n = run_single_count(graph, step.cypher, params).await?;
                out.insert(step.key.to_owned(), json!(n));
            }
            Ok::<_, (String, String)>(Value::Object(out))
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

    /// DETACH DELETE every Chunk in a workspace, optionally pruning
    /// now-orphaned Entity nodes. The `prune_orphans` switch is what lets one
    /// primitive serve two callers:
    ///   * **workspace teardown** (delete / MRU-evict) → `true`: the workspace
    ///     is gone, so entities only its chunks mentioned should disappear too
    ///     ([`delete_workspace`]).
    ///   * **full-reindex replace** (#99) → `false`: the chunk id embeds mtime,
    ///     so a re-index re-keys every chunk and a bare MERGE would orphan the
    ///     superseded nodes; deleting the workspace's chunks first makes the
    ///     upsert a true replace. Orphan pruning is SKIPPED so the Entity nodes
    ///     — and the authored `Concept`→`Entity` / typed edges anchored on them
    ///     — survive the recompute (they are about to be re-MERGE'd). Replace
    ///     recomputes chunks WITHOUT nuking authored relations.
    ///
    /// Returns `{ok, workspace, chunks_deleted, orphan_entities_deleted}`.
    pub async fn delete_workspace_chunks(&self, workspace: &str, prune_orphans: bool) -> Reply {
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
            let orphans = if prune_orphans {
                run_single_count(graph, cypher::DELETE_ORPHAN_ENTITIES, vec![]).await?
            } else {
                0
            };
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
                format!("delete_workspace_chunks timed out after {timeout:?}"),
            ),
        }
    }

    /// `fetch_workspace_graph` — read the workspace's code graph: every
    /// `Entity` mentioned in one of the workspace's chunks (+ a representative
    /// file/language) and every typed edge anchored on those entities. Pure
    /// read — never mutates. Used by the `workspaces.graph` verb; the
    /// [`super::projection`] layer turns the returned [`GraphRows`] into the
    /// wire `WorkspaceGraph`. The query shapes live in [`super::query`].
    pub async fn fetch_workspace_graph(
        &self,
        workspace: &str,
    ) -> std::result::Result<GraphRows, IpcError> {
        let ws = workspace.trim().to_owned();
        if ws.is_empty() {
            return Err(IpcError::new("bad_request", "'workspace' required"));
        }
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await?;
            let nodes = fetch_node_rows(graph, &ws).await?;
            let edges = fetch_edge_rows(graph, &ws).await?;
            Ok::<_, IpcError>(GraphRows { nodes, edges })
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(IpcError::new(
                error_codes::QUERY,
                format!("fetch_workspace_graph timed out after {timeout:?}"),
            )),
        }
    }

    /// Acquire (or open) the pooled [`Graph`] for a sibling read module in
    /// `graph/` — Slice G-data's neighbourhood walk
    /// ([`super::neighborhood::LiveSource`]) issues its own multi-step Cypher
    /// against the shared pool rather than re-opening a connection per query.
    /// Crate-internal; the public read surface stays the typed verbs.
    pub(crate) async fn graph_handle(&self) -> std::result::Result<&Graph, IpcError> {
        self.graph().await
    }

    /// `delete_file_nodes` (Slice I — file watcher) — drop the graph
    /// footprint of a single file (or, for a deleted directory, its whole
    /// subtree) within `workspace`: DETACH DELETE every Chunk whose `path`
    /// equals `path` or sits under it, then optionally prune now-orphaned
    /// Entity nodes (reusing `delete_workspace`'s prune step).
    ///
    /// `prune_orphans` is the watcher's modify-vs-delete switch:
    ///   * **delete / rename-away** → `true`: an entity only this file
    ///     mentioned should disappear (the spec's "`foo` node should be gone").
    ///   * **modify (delete-then-reupsert)** → `false`: the stale Chunk nodes
    ///     are cleared so a changed mtime (→ changed chunk id) can't orphan
    ///     them, but the full-graph orphan scan is skipped — the very entities
    ///     are about to be re-MERGE'd by the upsert, and skipping it keeps the
    ///     per-file delta cheap (no global `MATCH (e:Entity)` sweep).
    ///
    /// Returns `{ok, workspace, path, chunks_deleted, orphan_entities_deleted}`.
    pub async fn delete_file_nodes(
        &self,
        workspace: &str,
        path: &str,
        prune_orphans: bool,
    ) -> Reply {
        let ws = workspace.trim().to_owned();
        let path = path.trim().to_owned();
        if ws.is_empty() {
            return Reply::err_msg("bad_request", "'workspace' required");
        }
        if path.is_empty() {
            return Reply::err_msg("bad_request", "'path' required");
        }
        // Subtree prefix: any chunk under `<path><sep>` belongs to a deleted
        // directory's descendants. For a plain file this matches nothing extra.
        let prefix = format!("{path}{}", std::path::MAIN_SEPARATOR);
        let timeout = self.config.connect_timeout;
        let fut = async {
            let graph = self.graph().await.map_err(|e| (e.code, e.message))?;
            let chunks_deleted = run_single_count(
                graph,
                cypher::DELETE_FILE_CHUNKS,
                vec![
                    ("ws".to_owned(), BoltType::from(ws.clone())),
                    ("path".to_owned(), BoltType::from(path.clone())),
                    ("prefix".to_owned(), BoltType::from(prefix.clone())),
                ],
            )
            .await?;
            let orphans = if prune_orphans {
                run_single_count(graph, cypher::DELETE_ORPHAN_ENTITIES, vec![]).await?
            } else {
                0
            };
            Ok::<_, (String, String)>(json!({
                "ok": true,
                "workspace": ws,
                "path": path,
                "chunks_deleted": chunks_deleted,
                "orphan_entities_deleted": orphans,
            }))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => Reply::ok(v),
            Ok(Err((code, message))) => Reply::err_msg(code, message),
            Err(_) => Reply::err_msg(
                error_codes::QUERY,
                format!("delete_file_nodes timed out after {timeout:?}"),
            ),
        }
    }
}

/// Run [`query::NODES_FOR_WORKSPACE`] and decode each row into a [`NodeRow`].
async fn fetch_node_rows(graph: &Graph, ws: &str) -> std::result::Result<Vec<NodeRow>, IpcError> {
    let mut rows = graph
        .execute(neo4rs::query(query::NODES_FOR_WORKSPACE).param("ws", ws.to_owned()))
        .await
        .map_err(|e| IpcError::new(error_codes::QUERY, format!("graph nodes: {e}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| IpcError::new(error_codes::DECODE, format!("graph nodes decode: {e}")))?
    {
        let name: String = row.get("name").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.push(NodeRow {
            name,
            file: row.get("file").unwrap_or_default(),
            language: row.get("language").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Run [`query::EDGES_FOR_WORKSPACE`] and decode each row into an [`EdgeRow`].
async fn fetch_edge_rows(graph: &Graph, ws: &str) -> std::result::Result<Vec<EdgeRow>, IpcError> {
    let mut rows = graph
        .execute(neo4rs::query(query::EDGES_FOR_WORKSPACE).param("ws", ws.to_owned()))
        .await
        .map_err(|e| IpcError::new(error_codes::QUERY, format!("graph edges: {e}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| IpcError::new(error_codes::DECODE, format!("graph edges decode: {e}")))?
    {
        let src: String = row.get("src").unwrap_or_default();
        let dst: String = row.get("dst").unwrap_or_default();
        let rel: String = row.get("rel").unwrap_or_default();
        if src.is_empty() || dst.is_empty() || rel.is_empty() {
            continue;
        }
        out.push(EdgeRow { src, dst, rel });
    }
    Ok(out)
}

impl Default for BoltClient {
    fn default() -> Self {
        Self::new()
    }
}

/// One row of the upsert $batch.
#[derive(Clone, Debug)]
struct UpsertRow {
    id: String,
    path: String,
    symbol: String,
    language: String,
    entities: Vec<String>,
    workspace: String,
}

/// One typed Entity→Entity edge pair after rel-type validation.
#[derive(Clone, Debug)]
struct EntityEdge {
    source: String,
    target: String,
}

/// Coerce caller-provided chunk JSON into the upsert batch + grouped
/// typed-rel buckets. Pure — unit-tested without a live Neo4j.
fn coerce_upsert_batch(
    chunks: &[Value],
    workspace_default: &str,
) -> (
    Vec<UpsertRow>,
    std::collections::BTreeMap<String, Vec<EntityEdge>>,
) {
    use std::collections::BTreeMap;

    let mut batch: Vec<UpsertRow> = Vec::with_capacity(chunks.len());
    let mut typed: BTreeMap<String, Vec<EntityEdge>> = BTreeMap::new();
    for c in chunks {
        let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        if id.is_empty() {
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
                typed.entry(rt).or_default().push(EntityEdge {
                    source: src,
                    target: tgt,
                });
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

/// Coerce caller-built concept rows (`{id,label,description,source,members,
/// parents}` JSON) into the `$batch` BoltList [`cypher::UPSERT_CONCEPTS`] reads.
/// Pure — unit-tested without a live Neo4j.
fn concepts_to_boltlist(workspace: &str, rows: &[Value]) -> BoltList {
    let mut list = BoltList::new();
    for r in rows {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let mut m = BoltMap::new();
        m.put("workspace".into(), BoltType::from(workspace.to_owned()));
        m.put("id".into(), BoltType::from(id.to_owned()));
        m.put(
            "label".into(),
            BoltType::from(
                r.get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
        );
        m.put(
            "description".into(),
            BoltType::from(
                r.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
        );
        m.put(
            "source".into(),
            BoltType::from(
                r.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
        );
        m.put(
            "members".into(),
            BoltType::List(str_array_to_boltlist(r.get("members"))),
        );
        m.put(
            "parents".into(),
            BoltType::List(str_array_to_boltlist(r.get("parents"))),
        );
        list.push(BoltType::Map(m));
    }
    list
}

/// A JSON string-array `Value` → `BoltList` of strings (empty for absent/non-array).
fn str_array_to_boltlist(v: Option<&Value>) -> BoltList {
    let mut list = BoltList::new();
    if let Some(arr) = v.and_then(Value::as_array) {
        for s in arr.iter().filter_map(Value::as_str) {
            list.push(BoltType::from(s.to_owned()));
        }
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

/// Run a write that returns a single `RETURN n` row; surface `n` as i64.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BoltClient>();
    }

    #[test]
    fn config_from_env_falls_back_to_defaults() {
        // Read defaults without mutating shared env (other tests may run).
        let cfg = BoltConfig {
            uri: DEFAULT_BOLT_URL.to_owned(),
            user: String::new(),
            password: String::new(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
        };
        assert_eq!(cfg.uri, "bolt://127.0.0.1:7687");
    }

    #[test]
    fn write_batch_size_is_bounded_and_overridable() {
        std::env::remove_var("WYLDE_BOLT_WRITE_BATCH");
        assert_eq!(write_batch_size(), WRITE_BATCH);
        // Must keep each UNWIND well under the size that times a real upsert
        // out — a whole-repo single batch is exactly the bug this fixes.
        const { assert!(WRITE_BATCH <= 1000) };
        // The default write budget must exceed the connect budget — the old
        // code conflated them and capped bulk writes at 5s.
        assert!(DEFAULT_WRITE_TIMEOUT > DEFAULT_CONNECT_TIMEOUT);
        std::env::set_var("WYLDE_BOLT_WRITE_BATCH", "128");
        assert_eq!(write_batch_size(), 128);
        std::env::set_var("WYLDE_BOLT_WRITE_BATCH", "0");
        assert_eq!(write_batch_size(), WRITE_BATCH);
        std::env::remove_var("WYLDE_BOLT_WRITE_BATCH");
    }

    #[test]
    fn coerce_drops_rows_with_empty_id() {
        let chunks = vec![
            json!({"id": "c1", "entities": ["foo"]}),
            json!({"id": "", "entities": ["bar"]}),
            json!({"entities": ["baz"]}),
            json!({"id": "c4"}),
        ];
        let (batch, _) = coerce_upsert_batch(&chunks, "default");
        let ids: Vec<&str> = batch.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c4"]);
    }

    #[test]
    fn coerce_defaults_workspace_when_chunk_omits_it() {
        let chunks = vec![
            json!({"id": "c1"}),
            json!({"id": "c2", "workspace": "ws-explicit"}),
            json!({"id": "c3", "workspace": ""}),
        ];
        let (batch, _) = coerce_upsert_batch(&chunks, "route-default");
        assert_eq!(batch[0].workspace, "route-default");
        assert_eq!(batch[1].workspace, "ws-explicit");
        assert_eq!(batch[2].workspace, "route-default");
    }

    #[test]
    fn concepts_to_boltlist_skips_idless_and_carries_fields() {
        let rows = vec![
            json!({
                "id": "dir:src/graph", "label": "Graph", "description": "d",
                "source": "directory_cluster",
                "members": ["alpha", "beta"], "parents": ["dir:src"]
            }),
            json!({ "label": "no id row" }), // dropped — no id
        ];
        let list = concepts_to_boltlist("ws-1", &rows);
        assert_eq!(list.len(), 1, "idless row dropped");
        // The surviving row is a Map carrying workspace + members list.
        let BoltType::Map(m) = list.iter().next().unwrap() else {
            panic!("expected a Map row");
        };
        assert!(matches!(m.get("workspace"), Ok(Some(BoltType::String(_)))));
        assert!(matches!(m.get("members"), Ok(Some(BoltType::List(l))) if l.len() == 2));
        assert!(matches!(m.get("parents"), Ok(Some(BoltType::List(l))) if l.len() == 1));
    }

    #[test]
    fn coerce_filters_invalid_typed_relationships() {
        let chunks = vec![json!({
            "id": "c1",
            "relationships": [
                {"type": "CALLS", "source": "a", "target": "b"},
                {"type": "calls", "source": "c", "target": "d"},
                {"type": "CALLS", "source": "x", "target": "x"},
                {"type": "CALLS", "source": "", "target": "z"},
                {"type": "RANDOM", "source": "p", "target": "q"},
                {"type": "IMPORTS", "source": " a ", "target": " b "},
            ]
        })];
        let (_, typed) = coerce_upsert_batch(&chunks, "default");
        assert_eq!(typed.get("CALLS").expect("CALLS").len(), 2);
        let imports = typed.get("IMPORTS").expect("IMPORTS");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].source, "a");
        assert_eq!(imports[0].target, "b");
        assert!(!typed.contains_key("RANDOM"));
    }
}
