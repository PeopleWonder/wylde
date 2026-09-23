//! Live integration for the `workspaces.symbol_context` read verb (Slice
//! G-data).
//!
//! Seeds a small, **uniquely-named** corpus straight into Neo4j over Bolt
//! (`upsert` + `relate`, same as the Slice B graph test), writes one real
//! source file to disk so the body-fetch path has something to read, then
//! pulls the focal symbol's context back through the verb code path
//! (`graph::neighborhood::symbol_context`) and asserts:
//!   * callers (2) + callees (3) all surface at `hop_distance = 1`,
//!   * a 2-hop walk reaches the indirect callee + assigns `hop_distance = 2`,
//!   * `types_used` (INHERITS + IMPORTS) and `siblings` (co-file) surface,
//!   * `include_body=true` loads the focal's real source body + line,
//!   * the per-hop perf budget is met (1-hop < 500ms, 3-hop < 1.1s).
//!
//! Cleans up its own workspace (graph + the temp file) at the end.
//!
//! Entity names are nonce-prefixed because the graph's Entity nodes + typed
//! edges are global-by-name (see `graph::query` docs); the prefix keeps the
//! source-scoped reads from colliding with anything another run wrote.
//!
//! `#[ignore]`: needs Neo4j (`bolt://127.0.0.1:7687`, the Wylde user's
//! `auth=None` config) live. Run with:
//!
//! ```bash
//! cargo test -p wylde-workspaces --test integration_symbol_context -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::time::Instant;

use serde_json::json;

use wylde_workspaces::graph::projection::NodeKind;
use wylde_workspaces::graph::{neighborhood, BoltClient, ContextRel, EntityPair};

fn nonce() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    format!("gdtest_{}_{}_", std::process::id(), micros)
}

#[tokio::test]
#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687) — run the stack first"]
async fn symbol_context_returns_full_neighbourhood_from_live_neo4j() {
    let p = nonce();
    let ws = format!("{p}ws");
    let n = |s: &str| format!("{p}{s}");
    let client = BoltClient::new();

    // Write the focal's real source file so include_body has content to read.
    // `focal` is defined here; its body runs to the next blank line.
    let td = tempfile::tempdir().expect("tempdir");
    let focal_path = td.path().join("focal.rs").to_string_lossy().into_owned();
    let focal_src = format!(
        "use std::io;\n\n\
         fn {focal}() {{\n    \
             {callee1}();\n    \
             {callee2}();\n    \
             {callee3}();\n\
         }}\n\n\
         fn {sib}() {{}}\n",
        focal = n("focal"),
        callee1 = n("callee1"),
        callee2 = n("callee2"),
        callee3 = n("callee3"),
        sib = n("sibling"),
    );
    std::fs::write(&focal_path, &focal_src).expect("write focal source");

    // ── seed the graph ──────────────────────────────────────────────────
    // focal.rs mentions: focal, its three callees, a sibling, the inherited
    // base, the imported module. caller1/caller2 live in their own files.
    let dir = td.path().to_string_lossy().into_owned();
    let chunks = vec![
        json!({
            "id": format!("{p}chunk-focal"),
            "path": focal_path,
            "workspace": ws,
            "language": "rust",
            "entities": [
                n("focal"), n("callee1"), n("callee2"), n("callee3"),
                n("sibling"), n("Base"), n("std_io"),
            ],
        }),
        json!({
            "id": format!("{p}chunk-callers"),
            "path": format!("{dir}/callers.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("caller1"), n("caller2")],
        }),
        json!({
            "id": format!("{p}chunk-deep"),
            "path": format!("{dir}/deep.rs"),
            "workspace": ws,
            "language": "rust",
            "entities": [n("deep")],
        }),
    ];
    let up = client.upsert(chunks).await;
    assert!(up.ok, "seed upsert failed (is Neo4j up?): {:?}", up.error);

    // Call edges: caller1/caller2 → focal; focal → callee1/2/3; callee1 → deep.
    let calls = vec![
        EntityPair::new(n("caller1"), n("focal")),
        EntityPair::new(n("caller2"), n("focal")),
        EntityPair::new(n("focal"), n("callee1")),
        EntityPair::new(n("focal"), n("callee2")),
        EntityPair::new(n("focal"), n("callee3")),
        EntityPair::new(n("callee1"), n("deep")),
    ];
    // focal IMPORTS std_io, INHERITS Base.
    let imports = vec![EntityPair::new(n("focal"), n("std_io"))];
    let inherits = vec![EntityPair::new(n("focal"), n("Base"))];
    assert!(client.relate("CALLS", calls).await.ok, "seed CALLS");
    assert!(client.relate("IMPORTS", imports).await.ok, "seed IMPORTS");
    assert!(
        client.relate("INHERITS", inherits).await.ok,
        "seed INHERITS"
    );

    // Warm the Bolt pool + query planner BEFORE the timed reads. The OI-1
    // per-hop budget (200ms + 300ms × hops) measures the *traversal* cost, not
    // the one-time connection handshake and first-query plan compilation a
    // freshly-booted Neo4j pays — which lands ~500ms on the very first call and
    // would otherwise blow the 1-hop budget on cold-start alone (the warm 3-hop
    // read below runs in tens of ms). This throwaway read primes both caches so
    // the timed measurement reflects steady-state per-hop cost, the thing the
    // budget is actually about.
    let _ = neighborhood::symbol_context(&ws, &n("focal"), Some(1), false, false)
        .await
        .expect("warmup read")
        .expect("focal resolves");

    // ── 1-hop read through the verb code path ───────────────────────────
    let t1 = Instant::now();
    let ctx = neighborhood::symbol_context(&ws, &n("focal"), Some(1), true, true)
        .await
        .expect("symbol_context read")
        .expect("focal resolves");
    let one_hop_ms = t1.elapsed().as_millis();

    eprintln!(
        "[symbol_context] 1-hop: callers={} callees={} types={} siblings={} \
         hops_traversed={} took_ms={} wall_ms={}",
        ctx.callers.len(),
        ctx.callees.len(),
        ctx.types_used.len(),
        ctx.siblings.len(),
        ctx.hops_traversed,
        ctx.took_ms,
        one_hop_ms,
    );

    // Focal identity + body.
    assert_eq!(ctx.symbol.id, n("focal"));
    assert_eq!(
        ctx.symbol.kind,
        NodeKind::Module,
        "import endpoint → Module"
    );
    assert!(ctx.symbol.file.to_string_lossy().ends_with("focal.rs"));
    assert_eq!(ctx.symbol.line, 3, "fn defined on line 3");
    let body = ctx.symbol.body.as_deref().expect("body loaded");
    assert!(
        body.starts_with(&format!("fn {}()", n("focal"))),
        "body: {body}"
    );
    assert!(body.contains(&n("callee1")), "body has callee1: {body}");
    assert!(!body.contains(&n("sibling")), "body stops at blank line");

    // 2 callers + 3 callees at hop 1.
    let caller_names: Vec<&str> = ctx.callers.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(caller_names, vec![n("caller1"), n("caller2")], "callers");
    assert!(ctx.callers.iter().all(|r| r.hop_distance == 1));
    assert!(ctx.callers.iter().all(|r| r.rel_type == ContextRel::Calls));

    let callee_names: Vec<&str> = ctx.callees.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        callee_names,
        vec![n("callee1"), n("callee2"), n("callee3")],
        "callees"
    );
    assert!(ctx.callees.iter().all(|r| r.hop_distance == 1));
    assert_eq!(ctx.hops_traversed, 1, "1-hop request → 1 traversed");

    // types_used: INHERITS Base (Class) + IMPORTS std_io (Module).
    let base = ctx
        .types_used
        .iter()
        .find(|r| r.name == n("Base"))
        .expect("Base");
    assert_eq!(base.rel_type, ContextRel::Inherits);
    assert_eq!(base.kind, NodeKind::Class);
    let io = ctx
        .types_used
        .iter()
        .find(|r| r.name == n("std_io"))
        .expect("std_io");
    assert_eq!(io.rel_type, ContextRel::Imports);
    assert_eq!(io.kind, NodeKind::Module);

    // siblings: the focal's co-file entities (e.g. `sibling`), labelled SiblingOf.
    assert!(
        ctx.siblings.iter().any(|r| r.name == n("sibling")),
        "sibling surfaces: {:?}",
        ctx.siblings.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(ctx
        .siblings
        .iter()
        .all(|r| r.rel_type == ContextRel::SiblingOf));
    // The focal must never be its own sibling/caller/callee.
    assert!(ctx.siblings.iter().all(|r| r.name != n("focal")));

    // ── 2-hop: the indirect callee `deep` appears at hop_distance 2 ──────
    let ctx2 = neighborhood::symbol_context(&ws, &n("focal"), Some(2), false, false)
        .await
        .expect("2-hop read")
        .expect("focal resolves");
    let deep = ctx2
        .callees
        .iter()
        .find(|r| r.name == n("deep"))
        .expect("deep at 2 hops");
    assert_eq!(deep.hop_distance, 2, "callee1→deep is 2 hops from focal");
    assert_eq!(ctx2.hops_traversed, 2);
    assert!(ctx2.symbol.body.is_none(), "include_body=false ⇒ no body");

    // ── 3-hop perf budget (OI-1: 200 + 300*3 = 1100ms) ──────────────────
    let t3 = Instant::now();
    let _ = neighborhood::symbol_context(&ws, &n("focal"), Some(3), true, false)
        .await
        .expect("3-hop read")
        .expect("focal resolves");
    let three_hop_ms = t3.elapsed().as_millis();
    eprintln!("[symbol_context] perf: 1-hop={one_hop_ms}ms 3-hop={three_hop_ms}ms");

    assert!(one_hop_ms < 500, "1-hop budget: {one_hop_ms}ms < 500ms");
    assert!(
        three_hop_ms < 1100,
        "3-hop budget: {three_hop_ms}ms < 1100ms"
    );

    // Unknown symbol → not found (Ok(None)).
    let missing = neighborhood::symbol_context(&ws, &n("ghost"), Some(1), true, true)
        .await
        .expect("read ok");
    assert!(missing.is_none(), "ghost symbol ⇒ not found");

    // ── cleanup ─────────────────────────────────────────────────────────
    let del = client.delete_workspace(&ws).await;
    assert!(del.ok, "cleanup delete_workspace failed: {:?}", del.error);
    let after = neighborhood::symbol_context(&ws, &n("focal"), Some(1), false, false)
        .await
        .expect("post-cleanup read");
    assert!(after.is_none(), "focal gone after cleanup");
}
