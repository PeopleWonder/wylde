//! Workspace graph-ingest — the entity-extraction + graph-write half of
//! a workspace's RAG ingest, folded into the harness so Workspaces owns
//! its ingest pipeline end-to-end.
//!
//! ## What this replaces
//!
//! Until 2026-06-07 the structural-graph half of ingest lived in the N8N
//! `rag-ingest.json` workflow (the "Build Graph + Attach Entities" node
//! plus a Memgraph upsert + three `relate` calls). The vector half was
//! ported to Rust in PR #18 ([`super`]) but the graph half was left in
//! N8N, so a fresh Rust ingest embedded chunks **without** ever writing
//! Chunk/Entity nodes or `CALLS`/`IMPORTS`/`INHERITS` edges. This module
//! closes that gap; the N8N workflow is retired.
//!
//! ## Pipeline (per workspace pass)
//!
//! For every file already walked + chunked by [`super::walk`]:
//!   1. Call `treesitter.extract_entities` over the **pipe**
//!      (`\\.\pipe\wylde-treesitter`) — the canonical harness→sidecar
//!      transport, same hop the harness `tooling::resource::resources::treesitter`
//!      uses.
//!   2. Attach the returned entity names to each of that file's chunks by
//!      line range (mirroring the retired N8N node), and build the typed
//!      Entity→Entity edge pairs (`CALLS` caller→callee, `IMPORTS`
//!      module→imported, `INHERITS` class→base).
//!
//! Then, once per pass:
//!   3. `memgraph.upsert(chunks)` — MERGE Chunk + Entity nodes +
//!      `MENTIONED_IN` edges, via the harness's direct-Bolt
//!      [`BoltClient`] (the live, default graph transport).
//!   4. `memgraph.relate(rel_type, pairs)` once per edge type.
//!
//! ## Fail-soft, by design
//!
//! Graph-write runs **alongside** the embed step ([`super::reindex_full`]
//! / [`super::reindex_delta`]) and must never endanger it. Every failure
//! mode degrades to a [`GraphOutcome`] with `error` set, never a panic or
//! a propagated `Err`:
//!   * **Unsupported language** (most workspace files — `.md`, `.txt`)
//!     or a per-file parse error → skip that file, keep going
//!     (`continueOnFail`).
//!   * **Sidecar unreachable** (a transport/`pipe_*` error) → stop
//!     re-dialling a dead pipe for every remaining file; upsert whatever
//!     was already built and record the outage.
//!   * **Graph backend down** → record the error; the embed half is
//!     unaffected.
//!
//! ## Workspace scoping
//!
//! Each Chunk node carries `workspace = <workspace_id>` (the property the
//! existing `upsert` Cypher sets and `traverse`'s workspace filter reads).
//! Entity nodes and typed edges are global-by-name — a function `foo` is
//! one node regardless of workspace — which is the existing graph schema's
//! deliberate shape (see `memgraph::cypher::UPSERT_ENTITIES`); we reuse it
//! rather than fork a parallel per-workspace entity space. Cleanup of a
//! workspace's graph footprint is therefore `BoltClient::delete_workspace`
//! (drops the workspace's chunks, then prunes now-orphaned entities).

use std::collections::HashSet;

use serde_json::{json, Value};
use wylde_shared::ipc::{self, IpcError, Reply};

use crate::config::Config;
use crate::graph::{BoltClient, EntityPair, REL_CALLS, REL_IMPORTS, REL_INHERITS};
use crate::registry::WorkspaceDefinition;

use super::chunk_id;
use super::walk::Chunk;

/// Result of one graph-write pass. Counts are post-dedup (what actually
/// hit the graph); `error` carries the first non-fatal failure, if any.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphOutcome {
    /// Files that parsed and contributed entities.
    pub files_parsed: u32,
    /// Files skipped — unsupported language or a per-file parse error.
    pub files_skipped: u32,
    /// Chunk nodes upserted.
    pub chunk_nodes: u32,
    /// `CALLS` edges written.
    pub calls: u32,
    /// `IMPORTS` edges written.
    pub imports: u32,
    /// `INHERITS` edges written.
    pub inherits: u32,
    /// First non-fatal failure (sidecar/backend outage). The vector embed
    /// pass is independent and proceeds regardless.
    pub error: Option<String>,
}

/// Extract entities for one file. Implemented by the real pipe client and
/// by a test mock. Private — kept off any public surface so the bare
/// `impl Future` return needs no `Send`-bound bookkeeping at call sites.
trait EntityExtractor {
    fn extract(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Value, IpcError>> + Send;
}

/// The graph write surface this module needs — exactly the two verbs the
/// retired N8N workflow called. Implemented by [`BoltClient`] and a test
/// mock. Deliberately narrower than the harness `memgraph::MemgraphTraversal`
/// (which is read-only); the write verbs live on the concrete client.
trait GraphSink {
    fn upsert(&self, chunks: Vec<Value>) -> impl std::future::Future<Output = Reply> + Send;
    fn relate(
        &self,
        rel_type: &str,
        pairs: Vec<EntityPair>,
    ) -> impl std::future::Future<Output = Reply> + Send;
}

/// Real entity source: one `treesitter.extract_entities` pipe hop per file.
struct PipeExtractor {
    service: String,
}

impl EntityExtractor for PipeExtractor {
    fn extract(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Value, IpcError>> + Send {
        let service = self.service.clone();
        let payload = json!({ "path": path });
        async move { ipc::call_action(&service, "treesitter.extract_entities", payload).await }
    }
}

impl GraphSink for BoltClient {
    fn upsert(&self, chunks: Vec<Value>) -> impl std::future::Future<Output = Reply> + Send {
        BoltClient::upsert(self, chunks)
    }

    fn relate(
        &self,
        rel_type: &str,
        pairs: Vec<EntityPair>,
    ) -> impl std::future::Future<Output = Reply> + Send {
        let rel = rel_type.to_owned();
        async move { BoltClient::relate(self, &rel, pairs).await }
    }
}

/// Entry point: extract entities for every file among `chunks` and write
/// the resulting Chunk/Entity nodes + typed edges to the graph. Fail-soft
/// (see the module docs) — safe to call from the index passes alongside
/// the embed step.
pub async fn write_graph(def: &WorkspaceDefinition, chunks: &[Chunk]) -> GraphOutcome {
    let cfg = Config::get();
    let extractor = PipeExtractor {
        service: cfg.treesitter_service.clone(),
    };
    let sink = BoltClient::new();
    ingest_graph(&def.id, chunks, &extractor, &sink).await
}

/// Testable core: drive `extractor` + `sink` over `chunks`. Split from
/// [`write_graph`] so unit tests can mock the sidecar reply + graph writes
/// without any live infrastructure.
async fn ingest_graph<E, S>(
    workspace_id: &str,
    chunks: &[Chunk],
    extractor: &E,
    sink: &S,
) -> GraphOutcome
where
    E: EntityExtractor,
    S: GraphSink,
{
    // Group chunks by source file, preserving order. BTreeMap keeps the
    // per-file extraction order deterministic (handy for tests + logs).
    let mut by_path: std::collections::BTreeMap<&str, Vec<&Chunk>> =
        std::collections::BTreeMap::new();
    for c in chunks {
        by_path.entry(c.path.as_str()).or_default().push(c);
    }

    let mut outcome = GraphOutcome::default();
    let mut mem_chunks: Vec<Value> = Vec::new();
    let mut calls: Vec<EntityPair> = Vec::new();
    let mut imports: Vec<EntityPair> = Vec::new();
    let mut inherits: Vec<EntityPair> = Vec::new();

    for (path, file_chunks) in by_path {
        match extractor.extract(path).await {
            Ok(reply) => {
                let file = build_file_payloads(workspace_id, &reply, &file_chunks);
                outcome.files_parsed += 1;
                mem_chunks.extend(file.mem_chunks);
                calls.extend(file.calls);
                imports.extend(file.imports);
                inherits.extend(file.inherits);
            }
            Err(e) if is_transport_error(&e.code) => {
                // The sidecar is unreachable. Don't pay a connect timeout
                // for every remaining file — record the outage and stop;
                // we still upsert anything already built below.
                tracing::debug!(
                    "workspaces.rag.graph: tree-sitter sidecar unavailable ({}: {}); \
                     stopping graph-write",
                    e.code,
                    e.message
                );
                outcome.error = Some(format!(
                    "tree-sitter sidecar unavailable ({}): {}",
                    e.code, e.message
                ));
                break;
            }
            Err(e) => {
                // Unsupported language (the common case: prose files) or a
                // per-file parse error — skip just this file.
                tracing::debug!(
                    "workspaces.rag.graph: skip {path} ({}: {})",
                    e.code,
                    e.message
                );
                outcome.files_skipped += 1;
            }
        }
    }

    // relate MERGEs each edge, so deduping is purely an efficiency pass
    // (smaller Cypher UNWIND lists).
    dedup_pairs(&mut calls);
    dedup_pairs(&mut imports);
    dedup_pairs(&mut inherits);

    outcome.chunk_nodes = mem_chunks.len() as u32;
    outcome.calls = calls.len() as u32;
    outcome.imports = imports.len() as u32;
    outcome.inherits = inherits.len() as u32;

    if mem_chunks.is_empty() {
        // Nothing parseable (e.g. an all-prose vault) or the sidecar was
        // down for every file. No graph writes; carry any recorded error.
        return outcome;
    }

    let up = sink.upsert(mem_chunks).await;
    if !up.ok {
        outcome.error.get_or_insert_with(|| reply_err(&up, "upsert"));
        return outcome;
    }

    for (rel_type, pairs) in [
        (REL_CALLS, calls),
        (REL_IMPORTS, imports),
        (REL_INHERITS, inherits),
    ] {
        if pairs.is_empty() {
            continue;
        }
        let r = sink.relate(rel_type, pairs).await;
        if !r.ok {
            // continueOnFail: record the first relate failure but still
            // attempt the remaining edge types.
            let msg = reply_err(&r, rel_type);
            tracing::warn!("workspaces.rag.graph: {msg}");
            outcome.error.get_or_insert(msg);
        }
    }

    outcome
}

/// The graph payloads built from one file's chunks + its entity reply.
struct FilePayloads {
    mem_chunks: Vec<Value>,
    calls: Vec<EntityPair>,
    imports: Vec<EntityPair>,
    inherits: Vec<EntityPair>,
}

/// Attach a file's structural entities to its chunks by line range and
/// build the typed-edge pairs. Pure — the unit-tested heart of the port,
/// mirroring the retired N8N "Build Graph + Attach Entities" node.
fn build_file_payloads(
    workspace_id: &str,
    reply: &Value,
    file_chunks: &[&Chunk],
) -> FilePayloads {
    let module = reply.get("module").and_then(Value::as_str).unwrap_or("");
    let language = reply.get("language").and_then(Value::as_str).unwrap_or("");
    let line_entities = flatten_line_entities(reply);

    let mem_chunks = file_chunks
        .iter()
        .map(|c| {
            let mut names: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            // The file's module identity anchors IMPORTS + module-level
            // calls — attach it to every chunk so those Entity nodes exist.
            dedup_push(&mut names, &mut seen, module);
            for (name, line) in &line_entities {
                if *line >= c.start_line && *line <= c.end_line {
                    dedup_push(&mut names, &mut seen, name);
                }
            }
            json!({
                "id": chunk_id(&c.path, c.chunk_idx, c.mtime),
                "path": c.path,
                "workspace": workspace_id,
                "language": language,
                "symbol": Value::Null,
                "entities": names,
            })
        })
        .collect();

    FilePayloads {
        mem_chunks,
        calls: call_pairs(reply),
        imports: import_pairs(reply, module),
        inherits: inherit_pairs(reply),
    }
}

/// Flat `(name, line)` list of every nameable entity in a file, so each
/// can be matched to the chunk whose line range covers it. Methods inherit
/// their class's definition line (the class spans them), mirroring N8N.
fn flatten_line_entities(reply: &Value) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for f in arr(reply, "functions") {
        if let (Some(name), Some(line)) = (str_at(f, "name"), u32_at(f, "line")) {
            out.push((name, line));
        }
    }
    for c in arr(reply, "classes") {
        let line = u32_at(c, "line").unwrap_or(1);
        if let Some(name) = str_at(c, "name") {
            out.push((name, line));
        }
        for m in arr(c, "methods") {
            if let Some(m) = m.as_str() {
                out.push((m.to_owned(), line));
            }
        }
    }
    for i in arr(reply, "imports") {
        if let (Some(m), Some(line)) = (str_at(i, "module"), u32_at(i, "line")) {
            out.push((m, line));
        }
    }
    for cl in arr(reply, "calls") {
        if let (Some(callee), Some(line)) = (str_at(cl, "callee"), u32_at(cl, "line")) {
            out.push((callee, line));
        }
    }
    out
}

/// `CALLS`: caller → callee.
fn call_pairs(reply: &Value) -> Vec<EntityPair> {
    arr(reply, "calls")
        .filter_map(|c| {
            let s = str_at(c, "caller").filter(|s| !s.is_empty())?;
            let t = str_at(c, "callee").filter(|s| !s.is_empty())?;
            Some(EntityPair::new(s, t))
        })
        .collect()
}

/// `IMPORTS`: this file's module → each imported module. Empty when the
/// file has no module identity (then the edge has no stable source).
fn import_pairs(reply: &Value, module: &str) -> Vec<EntityPair> {
    if module.is_empty() {
        return Vec::new();
    }
    arr(reply, "imports")
        .filter_map(|i| {
            let m = str_at(i, "module").filter(|s| !s.is_empty())?;
            Some(EntityPair::new(module, m))
        })
        .collect()
}

/// `INHERITS`: class → each base/parent type.
fn inherit_pairs(reply: &Value) -> Vec<EntityPair> {
    let mut out = Vec::new();
    for c in arr(reply, "classes") {
        let Some(name) = str_at(c, "name").filter(|s| !s.is_empty()) else {
            continue;
        };
        for b in arr(c, "bases") {
            if let Some(b) = b.as_str() {
                if !b.is_empty() {
                    out.push(EntityPair::new(name.clone(), b));
                }
            }
        }
    }
    out
}

// ── small helpers ────────────────────────────────────────────────────

fn dedup_push(names: &mut Vec<String>, seen: &mut HashSet<String>, name: &str) {
    if !name.is_empty() && seen.insert(name.to_owned()) {
        names.push(name.to_owned());
    }
}

fn dedup_pairs(pairs: &mut Vec<EntityPair>) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    pairs.retain(|p| seen.insert((p.source.clone(), p.target.clone())));
}

fn arr<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u32_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).map(|n| n as u32)
}

/// A transport/connectivity failure (sidecar down) vs a semantic per-file
/// error. `pipe_*` codes come from the IPC client; the rest are the
/// dispatch-disabled / no-backend guards.
fn is_transport_error(code: &str) -> bool {
    code.starts_with("pipe_") || matches!(code, "ipc_disabled" | "no_http_backend" | "timeout")
}

fn reply_err(reply: &Reply, what: &str) -> String {
    match &reply.error {
        Some(e) => format!("graph {what} failed ({}): {}", e.code, e.message),
        None => format!("graph {what} failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn chunk(path: &str, idx: u32, start: u32, end: u32) -> Chunk {
        Chunk {
            path: path.to_owned(),
            chunk_idx: idx,
            content: "x".to_owned(),
            mtime: 1.0,
            start_line: start,
            end_line: end,
        }
    }

    /// A representative `extract_entities` reply for a Rust-ish file with
    /// a free fn, an impl-with-base carrying a method, an import, and two
    /// calls — enough to exercise every edge type.
    fn sample_reply() -> Value {
        json!({
            "path": "C:/ws/widget.rs",
            "language": "rust",
            "module": "widget",
            "functions": [{"name": "free", "line": 3}],
            "classes": [{
                "name": "Widget",
                "line": 10,
                "methods": ["render"],
                "bases": ["Render"]
            }],
            "imports": [{"module": "std::collections", "line": 1}],
            "calls": [
                {"caller": "free", "callee": "helper", "line": 4},
                {"caller": "render", "callee": "draw", "line": 11}
            ],
            "counts": {"functions": 1, "classes": 1, "imports": 1, "calls": 2}
        })
    }

    // ── pure builder ────────────────────────────────────────────────

    #[test]
    fn build_file_payloads_attaches_entities_by_line_range() {
        // One chunk covering lines 1..6 (the import, free fn, helper call)
        // and one covering 8..14 (the impl, render, draw call).
        let chunks = [
            chunk("C:/ws/widget.rs", 0, 1, 6),
            chunk("C:/ws/widget.rs", 1, 8, 14),
        ];
        let refs: Vec<&Chunk> = chunks.iter().collect();
        let p = build_file_payloads("ws-1", &sample_reply(), &refs);

        assert_eq!(p.mem_chunks.len(), 2);

        // Every chunk carries the module identity + workspace + language.
        for mc in &p.mem_chunks {
            assert_eq!(mc["workspace"], "ws-1");
            assert_eq!(mc["language"], "rust");
            let ents: Vec<&str> = mc["entities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect();
            assert!(ents.contains(&"widget"), "module on every chunk: {ents:?}");
        }

        let ents0: Vec<&str> = p.mem_chunks[0]["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        // Chunk 0 (lines 1..6): import (1), free (3), helper call (4).
        assert!(ents0.contains(&"std::collections"));
        assert!(ents0.contains(&"free"));
        assert!(ents0.contains(&"helper"));
        // ...but NOT the impl-scoped names from lines 10/11.
        assert!(!ents0.contains(&"Widget"));
        assert!(!ents0.contains(&"draw"));

        let ents1: Vec<&str> = p.mem_chunks[1]["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        // Chunk 1 (lines 8..14): Widget (10), render method (class line 10),
        // draw call (11).
        assert!(ents1.contains(&"Widget"));
        assert!(ents1.contains(&"render"));
        assert!(ents1.contains(&"draw"));
    }

    #[test]
    fn build_file_payloads_id_matches_vector_store() {
        let chunks = [chunk("C:/ws/a.rs", 2, 1, 5)];
        let refs: Vec<&Chunk> = chunks.iter().collect();
        let p = build_file_payloads("ws-1", &sample_reply(), &refs);
        // Chunk node id is the SAME stable id the vector store assigns, so
        // the two stores can be joined on it.
        assert_eq!(p.mem_chunks[0]["id"], json!(chunk_id("C:/ws/a.rs", 2, 1.0)));
    }

    #[test]
    fn typed_edge_pairs_have_expected_shape() {
        let p = build_file_payloads("ws-1", &sample_reply(), &[]);
        let calls: Vec<(&str, &str)> = p
            .calls
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        assert!(calls.contains(&("free", "helper")));
        assert!(calls.contains(&("render", "draw")));

        // IMPORTS source is the file's module identity.
        assert_eq!(p.imports.len(), 1);
        assert_eq!(p.imports[0].source, "widget");
        assert_eq!(p.imports[0].target, "std::collections");

        // INHERITS: class → base.
        assert_eq!(p.inherits.len(), 1);
        assert_eq!(p.inherits[0].source, "Widget");
        assert_eq!(p.inherits[0].target, "Render");
    }

    #[test]
    fn import_pairs_empty_without_module() {
        let reply = json!({ "imports": [{"module": "os", "line": 1}] });
        assert!(import_pairs(&reply, "").is_empty());
    }

    #[test]
    fn dedup_pairs_collapses_duplicates() {
        let mut pairs = vec![
            EntityPair::new("a", "b"),
            EntityPair::new("a", "b"),
            EntityPair::new("a", "c"),
        ];
        dedup_pairs(&mut pairs);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn is_transport_error_classifies_codes() {
        assert!(is_transport_error("pipe_connect"));
        assert!(is_transport_error("pipe_timeout"));
        assert!(is_transport_error("ipc_disabled"));
        assert!(!is_transport_error("unsupported_language"));
        assert!(!is_transport_error("parse_failed"));
    }

    // ── ingest_graph orchestration, fully mocked ────────────────────

    /// Mock extractor: a path→reply table, with codes for the miss cases.
    struct MockExtractor {
        replies: std::collections::HashMap<String, Result<Value, IpcError>>,
    }
    impl EntityExtractor for MockExtractor {
        fn extract(
            &self,
            path: &str,
        ) -> impl std::future::Future<Output = Result<Value, IpcError>> + Send {
            let r = match self.replies.get(path) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(IpcError::new("unsupported_language", "no grammar")),
            };
            async move { r }
        }
    }

    #[derive(Default)]
    struct Recorded {
        upserts: Vec<Vec<Value>>,
        relates: Vec<(String, Vec<EntityPair>)>,
    }

    struct MockSink {
        rec: Mutex<Recorded>,
        ok: bool,
    }
    impl GraphSink for MockSink {
        fn upsert(&self, chunks: Vec<Value>) -> impl std::future::Future<Output = Reply> + Send {
            self.rec.lock().unwrap().upserts.push(chunks.clone());
            let ok = self.ok;
            async move {
                if ok {
                    Reply::ok(json!({"ok": true, "count": chunks.len()}))
                } else {
                    Reply::err_msg("bolt_connect", "no neo4j")
                }
            }
        }
        fn relate(
            &self,
            rel_type: &str,
            pairs: Vec<EntityPair>,
        ) -> impl std::future::Future<Output = Reply> + Send {
            self.rec
                .lock()
                .unwrap()
                .relates
                .push((rel_type.to_owned(), pairs.clone()));
            async move { Reply::ok(json!({"ok": true, "written": pairs.len()})) }
        }
    }

    fn extractor_with(path: &str, reply: Value) -> MockExtractor {
        let mut replies = std::collections::HashMap::new();
        replies.insert(path.to_owned(), Ok(reply));
        MockExtractor { replies }
    }

    #[tokio::test]
    async fn ingest_graph_upserts_chunks_and_relates_each_edge_type() {
        let chunks = vec![chunk("C:/ws/widget.rs", 0, 1, 20)];
        let extractor = extractor_with("C:/ws/widget.rs", sample_reply());
        let sink = MockSink {
            rec: Mutex::new(Recorded::default()),
            ok: true,
        };

        let out = ingest_graph("ws-1", &chunks, &extractor, &sink).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.files_parsed, 1);
        assert_eq!(out.chunk_nodes, 1);
        assert_eq!(out.calls, 2);
        assert_eq!(out.imports, 1);
        assert_eq!(out.inherits, 1);

        let rec = sink.rec.lock().unwrap();
        // One upsert carrying the single chunk, tagged with the workspace.
        assert_eq!(rec.upserts.len(), 1);
        assert_eq!(rec.upserts[0].len(), 1);
        assert_eq!(rec.upserts[0][0]["workspace"], "ws-1");
        // All three edge types relate'd, with the right rel_type labels.
        let labels: Vec<&str> = rec.relates.iter().map(|(t, _)| t.as_str()).collect();
        assert!(labels.contains(&"CALLS"));
        assert!(labels.contains(&"IMPORTS"));
        assert!(labels.contains(&"INHERITS"));
    }

    #[tokio::test]
    async fn ingest_graph_skips_unsupported_files_and_keeps_going() {
        // Two files: one parseable, one prose (default mock miss →
        // unsupported_language).
        let chunks = vec![
            chunk("C:/ws/widget.rs", 0, 1, 20),
            chunk("C:/ws/notes.md", 0, 1, 3),
        ];
        let extractor = extractor_with("C:/ws/widget.rs", sample_reply());
        let sink = MockSink {
            rec: Mutex::new(Recorded::default()),
            ok: true,
        };
        let out = ingest_graph("ws-1", &chunks, &extractor, &sink).await;
        assert_eq!(out.files_parsed, 1);
        assert_eq!(out.files_skipped, 1);
        assert!(out.error.is_none());
        assert_eq!(out.chunk_nodes, 1, "only the parseable file's chunk");
    }

    #[tokio::test]
    async fn ingest_graph_bails_on_sidecar_down_without_calling_sink() {
        let chunks = vec![chunk("C:/ws/a.rs", 0, 1, 5)];
        let mut replies = std::collections::HashMap::new();
        replies.insert(
            "C:/ws/a.rs".to_owned(),
            Err(IpcError::new("pipe_connect", "no sidecar")),
        );
        let extractor = MockExtractor { replies };
        let sink = MockSink {
            rec: Mutex::new(Recorded::default()),
            ok: true,
        };
        let out = ingest_graph("ws-1", &chunks, &extractor, &sink).await;
        assert!(out.error.as_deref().unwrap().contains("sidecar unavailable"));
        assert_eq!(out.chunk_nodes, 0);
        assert!(sink.rec.lock().unwrap().upserts.is_empty(), "no sink calls");
    }

    // ── integration: REAL tree-sitter parse of a 3-file .rs corpus ──
    //
    // Drives the genuine `wylde_treesitter::entities::extract_entities`
    // over real fixture files (no sidecar process, no Neo4j) and feeds the
    // results through the real walk → attach → build path, recording the
    // exact upsert/relate payloads. Asserts every edge type lands with the
    // expected endpoint names. The live Bolt round-trip is the manual
    // end-to-end step (and `bolt.rs` unit-tests the upsert/relate Cypher).

    #[tokio::test]
    async fn ingest_graph_over_real_rust_corpus_records_expected_edges() {
        use super::super::walk;

        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        // a.rs — an import + a free fn that calls another.
        std::fs::write(
            root.join("a.rs"),
            "use std::fmt;\n\nfn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
        )
        .unwrap();
        // b.rs — a trait + a struct + an impl-of-trait whose method calls out.
        std::fs::write(
            root.join("b.rs"),
            "trait Greet {\n    fn hello(&self);\n}\n\nstruct Robot;\n\n\
             impl Greet for Robot {\n    fn hello(&self) {\n        wave();\n    }\n}\n",
        )
        .unwrap();
        // c.rs — another import + a fn calling into a.rs's symbol.
        std::fs::write(
            root.join("c.rs"),
            "use std::collections::HashMap;\n\nfn gamma() {\n    alpha();\n}\n",
        )
        .unwrap();

        let chunks = walk::walk_and_chunk(&root.to_string_lossy());
        assert!(!chunks.is_empty(), "fixtures should chunk");

        // Build a path→reply table with REAL extraction (the same shape the
        // sidecar pipe would return).
        let mut replies: std::collections::HashMap<String, Result<Value, IpcError>> =
            std::collections::HashMap::new();
        for c in &chunks {
            replies.entry(c.path.clone()).or_insert_with(|| {
                Ok(wylde_treesitter::entities::extract_entities(&c.path, None)
                    .expect("real extraction"))
            });
        }
        let extractor = MockExtractor { replies };
        let sink = MockSink {
            rec: Mutex::new(Recorded::default()),
            ok: true,
        };

        let out = ingest_graph("ws-int", &chunks, &extractor, &sink).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.files_parsed, 3, "all three .rs files parsed");
        assert!(out.chunk_nodes >= 3);
        assert!(out.calls >= 3, "alpha→beta, hello→wave, gamma→alpha");
        assert!(out.imports >= 2, "a→std::fmt, c→std::collections");
        assert!(out.inherits >= 1, "Robot→Greet");

        // Inspect the actual relate payloads the graph would receive.
        let rec = sink.rec.lock().unwrap();
        let edges = |label: &str| -> Vec<(String, String)> {
            rec.relates
                .iter()
                .filter(|(t, _)| t == label)
                .flat_map(|(_, ps)| ps.iter().map(|p| (p.source.clone(), p.target.clone())))
                .collect()
        };
        let calls = edges("CALLS");
        assert!(calls.contains(&("alpha".into(), "beta".into())), "{calls:?}");
        assert!(calls.contains(&("hello".into(), "wave".into())), "{calls:?}");
        assert!(calls.contains(&("gamma".into(), "alpha".into())), "{calls:?}");

        let imports = edges("IMPORTS");
        // The Rust import strategy records the module *path prefix*:
        // `use std::fmt;` → `std`, `use std::collections::HashMap;` →
        // `std::collections`.
        assert!(imports.contains(&("a".into(), "std".into())), "{imports:?}");
        assert!(
            imports.contains(&("c".into(), "std::collections".into())),
            "{imports:?}"
        );

        let inherits = edges("INHERITS");
        assert!(
            inherits.contains(&("Robot".into(), "Greet".into())),
            "{inherits:?}"
        );

        // Every upserted chunk node is tagged with the workspace id.
        for batch in &rec.upserts {
            for ch in batch {
                assert_eq!(ch["workspace"], "ws-int");
            }
        }
    }

    #[tokio::test]
    async fn ingest_graph_records_upsert_failure_softly() {
        let chunks = vec![chunk("C:/ws/widget.rs", 0, 1, 20)];
        let extractor = extractor_with("C:/ws/widget.rs", sample_reply());
        let sink = MockSink {
            rec: Mutex::new(Recorded::default()),
            ok: false, // upsert returns !ok
        };
        let out = ingest_graph("ws-1", &chunks, &extractor, &sink).await;
        assert!(out.error.as_deref().unwrap().contains("upsert failed"));
        // No relate calls once the upsert failed.
        assert!(sink.rec.lock().unwrap().relates.is_empty());
    }
}
