//! Live end-to-end for the workspace graph-ingest path
//! (`workspaces::rag::indexer::graph_writer`).
//!
//! Drives the REAL pipeline — `write_graph` → `treesitter.extract_entities`
//! over the sidecar pipe → `memgraph.upsert` + `relate` over direct Bolt —
//! against a running stack, then reads the graph back over Bolt to confirm
//! workspace-scoped Chunk nodes + `CALLS`/`IMPORTS`/`INHERITS` edges landed.
//!
//! `#[ignore]`: needs the tree-sitter sidecar (`\\.\pipe\wylde-treesitter`)
//! and Neo4j (`bolt://127.0.0.1:7687`, the Wylde user's `auth=None` config)
//! both live — e.g. after `wylde-lifecycle.exe` has booted them. Run with:
//!
//! ```bash
//! cargo test -p wylde-harness --test workspaces_graph_ingest_live -- --ignored --nocapture
//! ```
//!
//! Cleans up its own workspace (`delete_workspace`) at the end, so it never
//! leaves rows in the production graph.

#![cfg(windows)]

use wylde_workspaces::graph::BoltClient;
use wylde_workspaces::rag::indexer::{graph_writer, walk};
use wylde_workspaces::registry::WorkspaceDefinition;

/// The corpus to ingest — the harness's own `workspaces/` tree. It has
/// `impl Trait for T` (→ INHERITS), cross-fn calls (→ CALLS), and `use`
/// imports (→ IMPORTS), so all three edge types are exercised.
const CORPUS: &str = "src";

fn unique_ws() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("wstest-graph-ingest-{}-{}", std::process::id(), nonce)
}

async fn graph() -> neo4rs::Graph {
    let uri = std::env::var("GRAPH_BOLT_URL").unwrap_or_else(|_| "bolt://127.0.0.1:7687".into());
    let cfg = neo4rs::ConfigBuilder::default()
        .uri(uri)
        .user(std::env::var("GRAPH_USER").unwrap_or_default())
        .password(std::env::var("GRAPH_PASSWORD").unwrap_or_default())
        .build()
        .expect("bolt config");
    neo4rs::Graph::connect(cfg).await.expect("connect neo4j")
}

async fn scalar(g: &neo4rs::Graph, q: neo4rs::Query) -> i64 {
    let mut rows = g.execute(q).await.expect("query");
    match rows.next().await {
        Ok(Some(row)) => row.get::<i64>("n").unwrap_or(0),
        _ => 0,
    }
}

#[tokio::test]
#[ignore = "requires live tree-sitter sidecar + Neo4j (run the stack first)"]
async fn real_corpus_ingest_writes_workspace_scoped_graph() {
    let ws = unique_ws();
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    let folder = folder.to_string_lossy().into_owned();

    let mut def = WorkspaceDefinition::new(&folder);
    def.id = ws.clone();

    let chunks = walk::walk_and_chunk(&folder);
    eprintln!("[ingest] corpus={folder}");
    eprintln!("[ingest] walked {} chunks", chunks.len());

    // ── the real path: pipe extract + bolt upsert/relate ────────────
    let out = graph_writer::write_graph(&def, &chunks).await;
    eprintln!("[ingest] outcome = {out:?}");
    assert!(
        out.error.is_none(),
        "graph-write failed (is the stack up?): {:?}",
        out.error
    );
    assert!(out.files_parsed > 0, "no files parsed — sidecar reachable?");
    assert!(out.chunk_nodes > 0, "no chunk nodes built");

    // ── read the graph back over Bolt ───────────────────────────────
    let g = graph().await;

    let chunk_nodes = scalar(
        &g,
        neo4rs::query("MATCH (c:Chunk {workspace: $ws}) RETURN count(c) AS n")
            .param("ws", ws.clone()),
    )
    .await;
    let mentions = scalar(
        &g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[r:MENTIONED_IN]-(:Entity) RETURN count(r) AS n",
        )
        .param("ws", ws.clone()),
    )
    .await;
    // Typed edges, attributed to the workspace via its mentioned entities.
    let calls = scalar(
        &g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)-[r:CALLS]->(:Entity) \
             RETURN count(DISTINCT r) AS n",
        )
        .param("ws", ws.clone()),
    )
    .await;
    let imports = scalar(
        &g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)-[r:IMPORTS]->(:Entity) \
             RETURN count(DISTINCT r) AS n",
        )
        .param("ws", ws.clone()),
    )
    .await;
    let inherits = scalar(
        &g,
        neo4rs::query(
            "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)-[r:INHERITS]->(:Entity) \
             RETURN count(DISTINCT r) AS n",
        )
        .param("ws", ws.clone()),
    )
    .await;

    eprintln!("[verify] Chunk nodes (workspace-scoped) = {chunk_nodes}");
    eprintln!("[verify] MENTIONED_IN edges            = {mentions}");
    eprintln!("[verify] CALLS edges (ws entities)      = {calls}");
    eprintln!("[verify] IMPORTS edges (ws entities)    = {imports}");
    eprintln!("[verify] INHERITS edges (ws entities)   = {inherits}");

    // Sample up to 10 CALLS edges anchored on this workspace's entities.
    let mut sample = g
        .execute(
            neo4rs::query(
                "MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)-[:CALLS]->(t:Entity) \
                 RETURN DISTINCT e.name AS s, t.name AS t LIMIT 10",
            )
            .param("ws", ws.clone()),
        )
        .await
        .expect("sample query");
    eprintln!("[verify] sample CALLS edges:");
    while let Ok(Some(row)) = sample.next().await {
        let s: String = row.get("s").unwrap_or_default();
        let t: String = row.get("t").unwrap_or_default();
        eprintln!("           {s} -CALLS-> {t}");
    }

    // ── assertions ──────────────────────────────────────────────────
    assert!(chunk_nodes > 0, "expected workspace-scoped Chunk nodes");
    assert!(mentions > 0, "expected MENTIONED_IN edges");
    assert!(inherits > 0, "expected INHERITS edges (impl Trait for T)");
    assert!(calls > 0, "expected CALLS edges");
    assert!(imports > 0, "expected IMPORTS edges");

    // ── cleanup ─────────────────────────────────────────────────────
    let del = BoltClient::new().delete_workspace(&ws).await;
    eprintln!("[cleanup] delete_workspace = {:?}", del.data);
    assert!(del.ok, "cleanup delete_workspace failed: {:?}", del.error);

    // Confirm the workspace's chunks are gone.
    let remaining = scalar(
        &g,
        neo4rs::query("MATCH (c:Chunk {workspace: $ws}) RETURN count(c) AS n").param("ws", ws),
    )
    .await;
    assert_eq!(remaining, 0, "workspace chunks should be cleaned up");
}
