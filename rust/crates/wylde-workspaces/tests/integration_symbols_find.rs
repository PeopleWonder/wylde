//! Live integration for the `workspaces.symbols.find` verb (Slice F-data).
//!
//! Seeds a small, **uniquely-named** fixture corpus straight into Neo4j over
//! Bolt (via the existing write client — `upsert` + `relate`), then resolves
//! symbols through the verb's service path
//! ([`wylde_workspaces::graph::symbol_index::symbols_find`]) and asserts:
//!
//!   * an **exact** name returns that symbol with score `1.0` and its kind;
//!   * a **fuzzy** fragment returns real, ranked entries;
//!   * `limit` caps the result set;
//!   * file-less external edge targets are NOT indexed.
//!
//! No active in-memory index is installed here, so the verb takes its
//! **on-demand build** fallback — building a fresh index from the live graph.
//! That exercises `SymbolIndex::build` + `fetch_workspace_graph` + projection
//! against a real Bolt round-trip. Cleans up its own workspace at the end.
//!
//! Only **Neo4j** is required (no tree-sitter sidecar, no Ollama): the seed is
//! written directly. Entity names are prefixed with a per-run nonce because
//! the graph's Entity nodes are global-by-name (see `graph::query` docs).
//!
//! `#[ignore]`: needs Neo4j (`bolt://127.0.0.1:7687`, `auth=None`) live:
//!
//! ```bash
//! cargo test -p wylde-workspaces --test integration_symbols_find -- --ignored --nocapture
//! ```

#![cfg(windows)]

use serde_json::json;

use wylde_workspaces::graph::projection::NodeKind;
use wylde_workspaces::graph::symbol_index::symbols_find;
use wylde_workspaces::graph::{BoltClient, EntityPair};

fn nonce() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("sftest_{}_{}_", std::process::id(), micros)
}

#[tokio::test]
#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687) — run the stack first"]
async fn symbols_find_resolves_exact_and_fuzzy_from_live_neo4j() {
    let p = nonce();
    let ws = format!("{p}ws");
    let client = BoltClient::new();
    let n = |s: &str| format!("{p}{s}");

    // Two "files" of mentioned entities. `Greet` is an INHERITS base never
    // mentioned in a chunk → a file-less external node that must NOT be indexed.
    let dir = format!("C:/sftest/{ws}/src");
    let chunks = vec![
        json!({
            "id": format!("{p}chunk-a"),
            "path": format!("{dir}/config.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("config"), n("parse_config"), n("parse_args"), n("ConfigError")],
        }),
        json!({
            "id": format!("{p}chunk-b"),
            "path": format!("{dir}/render.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("render"), n("Renderer"), n("draw")],
        }),
    ];
    let up = client.upsert(chunks).await;
    assert!(up.ok, "seed upsert failed (is Neo4j up?): {:?}", up.error);

    // edges: parse_config calls parse_args; Renderer inherits Greet (external).
    assert!(
        client
            .relate(
                "CALLS",
                vec![EntityPair::new(n("parse_config"), n("parse_args"))]
            )
            .await
            .ok,
        "seed CALLS failed"
    );
    assert!(
        client
            .relate("INHERITS", vec![EntityPair::new(n("Renderer"), n("Greet"))])
            .await
            .ok,
        "seed INHERITS failed"
    );

    // ── exact: the prefixed name returns exactly that symbol, score 1.0 ──
    let exact = symbols_find(&ws, &n("parse_config"), None)
        .await
        .expect("symbols.find exact");
    assert_eq!(exact.query, n("parse_config"));
    let top = exact.matches.first().expect("at least one match");
    assert_eq!(top.entry.name, n("parse_config"));
    assert_eq!(
        top.entry.kind,
        NodeKind::Function,
        "call participant → Function"
    );
    assert!((top.score - 1.0).abs() < f32::EPSILON, "exact scores 1.0");
    assert!(
        top.entry.file.to_string_lossy().ends_with("config.rs"),
        "carries real file: {:?}",
        top.entry.file
    );

    // ── fuzzy: a fragment surfaces both parse_* symbols as real entries ──
    let fuzzy = symbols_find(&ws, &n("parse"), Some(20))
        .await
        .expect("symbols.find fuzzy");
    let names: Vec<&str> = fuzzy
        .matches
        .iter()
        .map(|m| m.entry.name.as_str())
        .collect();
    assert!(
        names.contains(&n("parse_config").as_str()) && names.contains(&n("parse_args").as_str()),
        "fuzzy surfaces both parse_* symbols: {names:?}"
    );
    assert!(
        fuzzy
            .matches
            .iter()
            .all(|m| m.score > 0.0 && m.score <= 1.0),
        "scores normalised to (0,1]"
    );

    // ── the external INHERITS base is file-less → not indexed ───────────
    let ext = symbols_find(&ws, &n("Greet"), None)
        .await
        .expect("find Greet");
    assert!(
        ext.matches.iter().all(|m| m.entry.name != n("Greet")),
        "file-less external node excluded from the index: {:?}",
        ext.matches
    );

    // ── limit caps the result set ───────────────────────────────────────
    let capped = symbols_find(&ws, &n(""), Some(2))
        .await
        .expect("find capped");
    assert!(
        capped.matches.len() <= 2,
        "limit honoured: {}",
        capped.matches.len()
    );

    // ── cleanup ─────────────────────────────────────────────────────────
    let del = client.delete_workspace(&ws).await;
    assert!(del.ok, "cleanup delete_workspace failed: {:?}", del.error);
    let after = symbols_find(&ws, &n("parse_config"), None)
        .await
        .expect("post-cleanup find");
    assert!(after.matches.is_empty(), "symbols cleaned up");
}
