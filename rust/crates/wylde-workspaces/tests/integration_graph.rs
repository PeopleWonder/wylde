//! Live integration for the `workspaces.graph` read verb (Slice B).
//!
//! Seeds a small, **uniquely-named** fixture corpus straight into Neo4j over
//! Bolt (via the existing write client — `upsert` + `relate`), then reads it
//! back through the new read path (`graph::api::graph`) and asserts the
//! projected `WorkspaceGraph` has the expected nodes, edges, kinds, and
//! clusters. Cleans up its own workspace at the end.
//!
//! Only **Neo4j** is required (no tree-sitter sidecar, no Ollama): the seed
//! is written directly, so the test is deterministic and exercises *this
//! slice's* read/projection code against a real Bolt round-trip.
//!
//! Entity names are prefixed with a per-run nonce because the graph's Entity
//! nodes + typed edges are global-by-name (see `graph::query` docs); the
//! prefix guarantees the source-scoped edge read can't pick up edges any
//! other workspace wrote.
//!
//! `#[ignore]`: needs Neo4j (`bolt://127.0.0.1:7687`, the Wylde user's
//! `auth=None` config) live. Run with:
//!
//! ```bash
//! cargo test -p wylde-workspaces --test integration_graph -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::sync::OnceLock;

use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};

use wylde_workspaces::graph::projection::NodeKind;
use wylde_workspaces::graph::{api, BoltClient, EntityPair, RelType};

/// Serialize every test in this binary against the one shared Neo4j.
///
/// `cargo test` runs a binary's tests multi-threaded by default, but the graph
/// is global-by-name: `delete_workspace`'s orphan-entity prune and `stats()`
/// are graph-wide, and concurrent tests also pile connections onto the freshly-
/// booted (cold-planner) JVM, which intermittently can't service a query inside
/// the connect timeout — surfacing as flaky `ok:false` operation failures on
/// the live-graph CI leg (#216). Holding this lock for the whole test gives each
/// one exclusive DB access, mirroring `wylde-harness`'s `memgraph_live` DB_LOCK.
async fn db_guard() -> MutexGuard<'static, ()> {
    static DB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    DB_LOCK.get_or_init(|| Mutex::new(())).lock().await
}

fn nonce() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("bgtest_{}_{}_", std::process::id(), micros)
}

#[tokio::test]
#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687) — run the stack first"]
async fn graph_verb_returns_expected_shape_from_live_neo4j() {
    let _db = db_guard().await;
    let p = nonce();
    let ws = format!("{p}ws");
    let client = BoltClient::new();

    // Three "files" under .../src, each a chunk carrying its mentioned
    // entities (module + symbols + call targets + import targets). Bases of
    // an INHERITS edge are deliberately NOT mentioned (mirrors real ingest),
    // so they surface only as external edge targets.
    let dir = format!("C:/bgtest/{ws}/src");
    let n = |s: &str| format!("{p}{s}");
    let chunks = vec![
        json!({
            "id": format!("{p}chunk-a"),
            "path": format!("{dir}/a.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("a"), n("alpha"), n("beta"), n("std_fmt")],
        }),
        json!({
            "id": format!("{p}chunk-b"),
            "path": format!("{dir}/b.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("b"), n("Robot"), n("hello"), n("wave")],
        }),
        json!({
            "id": format!("{p}chunk-c"),
            "path": format!("{dir}/c.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("c"), n("gamma"), n("alpha"), n("std_collections")],
        }),
    ];

    let up = client.upsert(chunks).await;
    assert!(up.ok, "seed upsert failed (is Neo4j up?): {:?}", up.error);

    let calls = vec![
        EntityPair::new(n("alpha"), n("beta")),
        EntityPair::new(n("hello"), n("wave")),
        EntityPair::new(n("gamma"), n("alpha")),
    ];
    let imports = vec![
        EntityPair::new(n("a"), n("std_fmt")),
        EntityPair::new(n("c"), n("std_collections")),
    ];
    // Greet is the only endpoint never mentioned in a chunk → external node.
    let inherits = vec![EntityPair::new(n("Robot"), n("Greet"))];
    assert!(client.relate("CALLS", calls).await.ok, "seed CALLS failed");
    assert!(
        client.relate("IMPORTS", imports).await.ok,
        "seed IMPORTS failed"
    );
    assert!(
        client.relate("INHERITS", inherits).await.ok,
        "seed INHERITS failed"
    );

    // ── read it back through the verb's code path ──────────────────────
    let g = api::graph(&ws).await.expect("graph read");

    let wire = serde_json::to_vec(&g).expect("serialize");
    eprintln!(
        "[graph] nodes={} edges={} clusters={} wire_bytes={}",
        g.nodes.len(),
        g.edges.len(),
        g.clusters.len(),
        wire.len()
    );
    for e in &g.edges {
        eprintln!("        {} -{:?}-> {}", e.src, e.rel_type, e.dst);
    }

    // 11 mentioned entities + 1 external (Greet).
    assert_eq!(g.nodes.len(), 12, "node count");
    assert_eq!(
        g.edges.len(),
        6,
        "edge count (3 CALLS + 2 IMPORTS + 1 INHERITS)"
    );

    let by_rel = |r: RelType| g.edges.iter().filter(|e| e.rel_type == r).count();
    assert_eq!(by_rel(RelType::Calls), 3, "CALLS");
    assert_eq!(by_rel(RelType::Imports), 2, "IMPORTS");
    assert_eq!(by_rel(RelType::Inherits), 1, "INHERITS");

    // Sample edges look meaningful (real, uniquely-named symbols).
    let has = |s: String, d: String, r: RelType| {
        g.edges
            .iter()
            .any(|e| e.src == s && e.dst == d && e.rel_type == r)
    };
    assert!(has(n("alpha"), n("beta"), RelType::Calls), "alpha→beta");
    assert!(has(n("gamma"), n("alpha"), RelType::Calls), "gamma→alpha");
    assert!(has(n("hello"), n("wave"), RelType::Calls), "hello→wave");
    assert!(
        has(n("Robot"), n("Greet"), RelType::Inherits),
        "Robot→Greet"
    );

    let kind_of = |name: &str| g.nodes.iter().find(|nd| nd.id == name).map(|nd| nd.kind);
    assert_eq!(
        kind_of(&n("a")),
        Some(NodeKind::Module),
        "import src → Module"
    );
    assert_eq!(
        kind_of(&n("std_collections")),
        Some(NodeKind::Module),
        "import tgt → Module"
    );
    assert_eq!(
        kind_of(&n("Robot")),
        Some(NodeKind::Class),
        "inherit → Class"
    );
    assert_eq!(
        kind_of(&n("Greet")),
        Some(NodeKind::Class),
        "inherit tgt → Class"
    );
    assert_eq!(
        kind_of(&n("alpha")),
        Some(NodeKind::Function),
        "call → Function"
    );

    // The external base has no source file; the mentioned ones do.
    let greet = g.nodes.iter().find(|nd| nd.id == n("Greet")).unwrap();
    assert_eq!(greet.file.as_os_str().len(), 0, "external node has no file");
    let alpha = g.nodes.iter().find(|nd| nd.id == n("alpha")).unwrap();
    assert!(alpha.file.to_string_lossy().ends_with("a.rs"), "alpha file");

    // Clusters: one per file parent dir → all 11 mentioned nodes under
    // .../src; the external Greet is unclustered.
    assert_eq!(g.clusters.len(), 1, "one cluster (the src dir)");
    let c = &g.clusters[0];
    assert_eq!(c.member_ids.len(), 11, "all mentioned nodes clustered");
    assert!(
        !c.member_ids.contains(&n("Greet")),
        "external node unclustered"
    );
    assert!(
        c.parent_breadcrumb.ends_with(&["src".to_owned()]),
        "breadcrumb ends with src: {:?}",
        c.parent_breadcrumb
    );

    // ── cleanup ────────────────────────────────────────────────────────
    let del = client.delete_workspace(&ws).await;
    assert!(del.ok, "cleanup delete_workspace failed: {:?}", del.error);

    // The workspace's graph is gone — a re-read yields an empty graph.
    let after = api::graph(&ws).await.expect("post-cleanup read");
    assert!(after.nodes.is_empty(), "nodes cleaned up");
    assert!(after.edges.is_empty(), "edges cleaned up");
}

/// #117 — workspace teardown must sweep the `Concept` projection.
///
/// The live counterpart to the unit regression in `graph::cleanup` (whose
/// FakeGraph can only prove the cascade *declares* the step). This proves the
/// Cypher actually removes the rows: seed a two-level concept hierarchy over
/// real entities, tear the workspace down, and assert the reported
/// `concepts_deleted` matches — then tear down a second time and assert it
/// reports **zero**. That second pass is the load-bearing one: a non-zero count
/// on the first call only proves the statement ran, whereas a zero on the
/// second proves the nodes are genuinely gone rather than re-matched forever.
///
/// Before the fix `concepts_deleted` did not exist at all — teardown ran chunks
/// + orphan entities only, and the concepts stayed in Memgraph permanently with
/// their `MEMBER` targets torn out by the entity prune.
#[tokio::test]
#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687) — run the stack first"]
async fn teardown_sweeps_concept_projection_from_live_neo4j() {
    let _db = db_guard().await;
    let p = nonce();
    let ws = format!("{p}ws_concepts");
    let client = BoltClient::new();
    let n = |s: &str| format!("{p}{s}");

    // Entities exist only as chunk mentions — the MEMBER targets a concept
    // points at. They are what the orphan prune deletes on teardown.
    let chunks = vec![json!({
        "id": format!("{p}chunk-c"),
        "path": format!("C:/bgtest/{ws}/src/c.rs"),
        "workspace": ws,
        "language": "rust",
        "entities": [n("auth"), n("token")],
    })];
    let up = client.upsert(chunks).await;
    assert!(up.ok, "seed upsert failed: {:?}", up.error);

    // A parent concept and its child, over those entities.
    let rows = vec![
        json!({
            "id": "sem:security",
            "label": "Security",
            "description": "",
            "source": "semantic",
            "members": [n("auth"), n("token")],
            "parents": [],
        }),
        json!({
            "id": "sem:login",
            "label": "Login",
            "description": "",
            "source": "semantic",
            "members": [n("auth")],
            "parents": ["sem:security"],
        }),
    ];
    let proj = client.project_concepts(&ws, rows).await;
    assert!(proj.ok, "project_concepts failed: {:?}", proj.error);
    assert_eq!(proj.data.get("projected").and_then(|v| v.as_u64()), Some(2));

    // ── teardown ───────────────────────────────────────────────────────
    let del = client.delete_workspace(&ws).await;
    assert!(del.ok, "delete_workspace failed: {:?}", del.error);
    assert_eq!(
        del.data.get("concepts_deleted").and_then(|v| v.as_i64()),
        Some(2),
        "teardown must delete both Concept nodes (#117); got {:?}",
        del.data
    );

    // The proof: a second teardown finds nothing left to delete.
    let again = client.delete_workspace(&ws).await;
    assert!(
        again.ok,
        "second delete_workspace failed: {:?}",
        again.error
    );
    assert_eq!(
        again.data.get("concepts_deleted").and_then(|v| v.as_i64()),
        Some(0),
        "Concept nodes survived teardown (#117); got {:?}",
        again.data
    );
    assert_eq!(
        again.data.get("chunks_deleted").and_then(|v| v.as_i64()),
        Some(0),
        "chunks survived teardown"
    );
}
