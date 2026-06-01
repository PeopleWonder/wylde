//! Stage-2 graph-distance expansion for the retrieval pipeline.
//!
//! Rust port of `Core/harness/memory/graph_retrieval.py`. Sits between
//! the vector / BM25 stage and the RRF fusion stage of
//! `Core/harness/memory/retrieval.py`: takes a candidate pool, walks
//! the Memgraph service from those candidates' entities, and returns
//! neighbour chunks ranked by inverse hop distance.
//!
//! The whole stage is best-effort — every error path returns an empty
//! expansion so callers can splice the result into their hit list
//! unconditionally. This matches the Python module's contract.
//!
//! ## What gets called, in order
//!
//! For candidate-derived expansion the Python module tried `multihop`
//! first and fell back to `traverse`. The Python `multihop` client
//! mis-mapped its arguments (sent `start` where the server reads
//! `entities`, sent chunk IDs where the server reads entity NAMES) —
//! the result was that the multihop call always silently returned an
//! empty list and the traverse fallback did all the work. The Rust
//! port fixes both: we extract candidate **entities** up front and
//! hand those to `multihop`, which is the input shape the server
//! actually wants. If multihop comes back empty (or errors), we still
//! fall through to `traverse` with the same entity list, preserving
//! the layered-degradation contract.
//!
//! For the explicit `seed_entities` path the Python module called
//! `traverse` directly — Rust does the same.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::client::TraverseRequest;
use super::transport::MemgraphTraversal;

/// Default expansion hop budget. Mirrors Python's
/// `WYLDE_GRAPH_HOPS` env override, default 1.
pub fn default_hops() -> u32 {
    std::env::var("WYLDE_GRAPH_HOPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// Default max extra neighbours to surface. Mirrors Python's
/// `WYLDE_GRAPH_MAX_EXTRA` env override, default 20.
pub fn default_max_extra() -> u32 {
    std::env::var("WYLDE_GRAPH_MAX_EXTRA")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

/// Compile-time fallback constants. Tests use these to pin the same
/// default values Python's `DEFAULT_HOPS` / `DEFAULT_MAX_EXTRA`
/// constants documented.
pub const DEFAULT_HOPS: u32 = 1;
pub const DEFAULT_MAX_EXTRA: u32 = 20;

/// Tunable knobs for [`expand_by_graph`].
#[derive(Clone, Debug)]
pub struct ExpandOptions {
    /// Filter neighbours to chunks in this workspace. Empty string ⇒
    /// no filter (matches Python's empty-string sentinel).
    pub workspace_id: String,
    /// Graph hop budget. Used by both multihop (`expand_hops`) and
    /// traverse (`max_hops`).
    pub hops: u32,
    /// Cap on the final neighbour list.
    pub max_extra: u32,
    /// Optional "soft addressing" hook: entity names extracted from the
    /// user's query (see Python `rag_entities`). Walked via `traverse`
    /// directly regardless of the candidate-derived expansion.
    pub seed_entities: Vec<String>,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            workspace_id: String::new(),
            hops: default_hops(),
            max_extra: default_max_extra(),
            seed_entities: Vec::new(),
        }
    }
}

/// One chunk surfaced via graph traversal. Matches the JSON shape
/// Python's `GraphHit.to_dict()` produces so consumers further down
/// the retrieval pipeline see identical data.
#[derive(Clone, Debug)]
pub struct GraphHit {
    pub id: String,
    pub path: String,
    pub content: String,
    pub hops: u32,
    pub via_entities: Vec<String>,
    pub similarity: f64,
}

impl GraphHit {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "path": self.path,
            "content": self.content,
            "hops": self.hops,
            "via_entities": self.via_entities,
            "similarity": self.similarity,
        })
    }
}

/// Convert a hop count to a 0..1 similarity for RRF fusion. Mirrors
/// Python's `_hops_to_similarity`: `1 / (1 + hops)`, hops clamped to ≥1.
pub fn hops_to_similarity(hops: u32) -> f64 {
    let h = hops.max(1) as f64;
    1.0 / (1.0 + h)
}

/// Take a candidate pool's entity edges and return up-to-N neighbour
/// chunks ranked by graph distance.
///
/// `candidates` carries the vector-stage hits (each should have an `id`
/// matching a Memgraph chunk node). `opts.workspace_id` filters
/// neighbours to the same workspace if the graph layer tracks that.
///
/// Returns `[]` on any error path so callers can splice the result
/// into their hit list unconditionally.
pub async fn expand_by_graph<T: MemgraphTraversal>(
    client: &T,
    candidates: Vec<Value>,
    opts: ExpandOptions,
) -> Vec<GraphHit> {
    let entity_seeds: Vec<String> = opts
        .seed_entities
        .iter()
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        })
        .collect();

    let seed_ids: Vec<String> = candidates
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();

    if seed_ids.is_empty() && entity_seeds.is_empty() {
        return Vec::new();
    }

    let mut raw: Vec<Value> = Vec::new();

    // ── Candidate-derived expansion ──────────────────────────────────
    if !seed_ids.is_empty() {
        let candidate_entities = collect_candidate_entities(&candidates);
        if !candidate_entities.is_empty() {
            // Try multihop first — the higher-precision primitive.
            let mh_reply = client
                .multihop(candidate_entities.clone(), opts.hops, opts.max_extra)
                .await;
            let mh_chunks = if mh_reply.ok {
                normalize_chunks(&mh_reply.data)
            } else {
                Vec::new()
            };
            if !mh_chunks.is_empty() {
                raw.extend(mh_chunks);
            } else {
                // Fall back to traverse — same entity list, but the
                // typed-edge expansion the route does is the safety
                // net for graphs that don't have the MENTIONED_IN
                // edges multihop needs.
                let trv = client
                    .traverse(TraverseRequest {
                        entities: candidate_entities,
                        max_hops: opts.hops,
                        limit: opts.max_extra,
                        workspace: if opts.workspace_id.is_empty() {
                            None
                        } else {
                            Some(opts.workspace_id.clone())
                        },
                        decay_alpha: None,
                        rel_depths: None,
                    })
                    .await;
                if trv.ok {
                    let mut chunks = normalize_chunks(&trv.data);
                    // Single-hop fallback — traverse doesn't return
                    // per-row hop counts in the same shape, so default
                    // to 1 (matches Python's `_try_traverse`).
                    for entry in chunks.iter_mut() {
                        synthesise_hops(entry, 1);
                    }
                    raw.extend(chunks);
                }
            }
        }
    }

    // ── Entity-seed expansion (soft addressing) ──────────────────────
    if !entity_seeds.is_empty() {
        let trv = client
            .traverse(TraverseRequest {
                entities: entity_seeds,
                max_hops: opts.hops,
                limit: opts.max_extra,
                workspace: if opts.workspace_id.is_empty() {
                    None
                } else {
                    Some(opts.workspace_id.clone())
                },
                decay_alpha: None,
                rel_depths: None,
            })
            .await;
        if trv.ok {
            let mut chunks = normalize_chunks(&trv.data);
            for entry in chunks.iter_mut() {
                synthesise_hops(entry, 1);
            }
            raw.extend(chunks);
        }
    }

    if raw.is_empty() {
        return Vec::new();
    }

    // ── Coerce / dedupe / sort ───────────────────────────────────────
    let seed_set: HashSet<String> = seed_ids.iter().cloned().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<GraphHit> = Vec::new();
    for entry in &raw {
        let chunk_id = entry
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| entry.get("chunk_id").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned();
        if chunk_id.is_empty() || seed_set.contains(&chunk_id) || seen.contains(&chunk_id) {
            continue;
        }
        seen.insert(chunk_id.clone());
        let hop_count = entry
            .get("hops")
            .and_then(Value::as_u64)
            .or_else(|| entry.get("distance").and_then(Value::as_u64))
            .unwrap_or(1) as u32;
        let via_entities: Vec<String> = entry
            .get("via_entities")
            .or_else(|| entry.get("entities"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| entry.get("body").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned();
        out.push(GraphHit {
            id: chunk_id,
            path,
            content,
            hops: hop_count,
            via_entities,
            similarity: hops_to_similarity(hop_count),
        });
        if out.len() >= opts.max_extra as usize {
            break;
        }
    }
    out.sort_by_key(|h| h.hops);
    out
}

/// Pull every `entities` (or `via_entities`) string out of the
/// candidate pool, deduplicated by lowercase. Order is first-seen.
fn collect_candidate_entities(candidates: &[Value]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for c in candidates {
        let lists = [c.get("entities"), c.get("via_entities")];
        for list in lists.into_iter().flatten() {
            if let Some(arr) = list.as_array() {
                for e in arr {
                    if let Some(name) = e.as_str() {
                        let trimmed = name.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let key = trimmed.to_lowercase();
                        if seen.insert(key) {
                            out.push(trimmed.to_owned());
                        }
                    }
                }
            }
        }
    }
    out
}

fn normalize_chunks(data: &Value) -> Vec<Value> {
    if let Value::Object(map) = data {
        if let Some(Value::Array(chunks)) = map.get("chunks") {
            return chunks.clone();
        }
        for key in ["results", "hits", "data"] {
            if let Some(Value::Array(list)) = map.get(key) {
                return list.clone();
            }
        }
        return Vec::new();
    }
    if let Value::Array(list) = data {
        return list.clone();
    }
    Vec::new()
}

fn synthesise_hops(entry: &mut Value, default: u64) {
    if let Value::Object(map) = entry {
        if !map.contains_key("hops") && !map.contains_key("distance") {
            map.insert("hops".to_owned(), json!(default));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::client::mock;
    use serde_json::json;
    use wylde_shared::ipc::Reply;

    fn opts_with(hops: u32, max_extra: u32) -> ExpandOptions {
        ExpandOptions {
            workspace_id: String::new(),
            hops,
            max_extra,
            seed_entities: Vec::new(),
        }
    }

    #[test]
    fn hops_to_similarity_curve() {
        assert!((hops_to_similarity(1) - 0.5).abs() < 1e-9);
        assert!((hops_to_similarity(2) - 1.0 / 3.0).abs() < 1e-9);
        assert!((hops_to_similarity(3) - 0.25).abs() < 1e-9);
        // hops=0 clamps to 1
        assert!((hops_to_similarity(0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn default_hops_falls_back_when_env_unset() {
        let prev = std::env::var("WYLDE_GRAPH_HOPS").ok(); // wylde-check: discard-result-ok
        std::env::remove_var("WYLDE_GRAPH_HOPS");
        assert_eq!(default_hops(), 1);
        if let Some(v) = prev {
            std::env::set_var("WYLDE_GRAPH_HOPS", v);
        }
    }

    #[test]
    fn default_max_extra_falls_back_when_env_unset() {
        let prev = std::env::var("WYLDE_GRAPH_MAX_EXTRA").ok(); // wylde-check: discard-result-ok
        std::env::remove_var("WYLDE_GRAPH_MAX_EXTRA");
        assert_eq!(default_max_extra(), 20);
        if let Some(v) = prev {
            std::env::set_var("WYLDE_GRAPH_MAX_EXTRA", v);
        }
    }

    #[test]
    fn graph_hit_to_value_round_trip() {
        let h = GraphHit {
            id: "c1".into(),
            path: "a.py".into(),
            content: "def foo(): ...".into(),
            hops: 2,
            via_entities: vec!["foo".into()],
            similarity: 0.5,
        };
        let v = h.to_value();
        assert_eq!(v["id"], "c1");
        assert_eq!(v["hops"], 2);
        assert_eq!(v["via_entities"][0], "foo");
    }

    #[test]
    fn collect_candidate_entities_dedupes_case_insensitive() {
        let cands = vec![
            json!({"id": "c1", "entities": ["Foo", "BAR"]}),
            json!({"id": "c2", "entities": ["foo", "baz"]}),
            json!({"id": "c3", "via_entities": ["BAR", "qux"]}),
        ];
        let ents = collect_candidate_entities(&cands);
        // First-seen wins on dedup, so "Foo" beats "foo", "BAR" beats later "BAR".
        assert_eq!(ents, vec!["Foo", "BAR", "baz", "qux"]);
    }

    #[test]
    fn normalize_chunks_handles_every_key() {
        assert_eq!(
            normalize_chunks(&json!({"chunks": [{"id": "c1"}]})).len(),
            1
        );
        assert_eq!(
            normalize_chunks(&json!({"results": [{"id": "c2"}]})).len(),
            1
        );
        assert_eq!(normalize_chunks(&json!({"hits": [{"id": "c3"}]})).len(), 1);
        assert_eq!(normalize_chunks(&json!([{"id": "c4"}])).len(), 1);
        assert!(normalize_chunks(&json!({"weird": "shape"})).is_empty());
    }

    #[tokio::test]
    async fn expand_returns_empty_when_no_seeds_or_candidates() {
        let (client, _) = mock::new_with_static_ok(Value::Null);
        let out = expand_by_graph(&client, Vec::new(), ExpandOptions::default()).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn expand_skips_when_candidates_have_no_entities() {
        // Candidates with ids but no entities — there's nothing to seed
        // multihop / traverse with on the candidate path.
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        let cands = vec![json!({"id": "c1"}), json!({"id": "c2"})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        assert!(out.is_empty());
        assert!(
            handle.calls().is_empty(),
            "client should not be called when candidates carry no entities"
        );
    }

    #[tokio::test]
    async fn expand_uses_multihop_first_then_decodes_chunks() {
        let (client, handle) = mock::new_with_static_ok(json!({
            "chunks": [
                {"id": "n1", "path": "a.py", "hops": 1, "via_entities": ["foo"]},
                {"id": "n2", "path": "b.py", "hops": 2, "via_entities": ["foo", "bar"]},
            ]
        }));
        let cands = vec![json!({"id": "c1", "entities": ["foo", "bar"]})];
        let out = expand_by_graph(&client, cands, opts_with(2, 10)).await;
        assert_eq!(out.len(), 2);
        // Sorted by hops ascending.
        assert_eq!(out[0].id, "n1");
        assert_eq!(out[0].hops, 1);
        assert_eq!(out[1].id, "n2");
        assert_eq!(out[1].hops, 2);
        // Multihop was the first call.
        let calls = handle.calls();
        assert_eq!(calls[0].method, "/multihop");
        // Multihop received the entity names extracted from the candidate.
        let entities = calls[0].payload["entities"].as_array().unwrap();
        assert!(entities.iter().any(|v| v.as_str() == Some("foo")));
        assert!(entities.iter().any(|v| v.as_str() == Some("bar")));
    }

    #[tokio::test]
    async fn expand_falls_back_to_traverse_when_multihop_empty() {
        // Multihop returns empty → traverse is called next.
        let traverse_data = json!({"chunks": [{"id": "n3", "path": "c.py"}]});
        let (client, handle) = mock::new_with_responder(move |call| {
            if call.method == "/multihop" {
                Reply::ok(json!({"chunks": []}))
            } else if call.method == "/traverse" {
                Reply::ok(traverse_data.clone())
            } else {
                Reply::err_msg("no_route", call.method.clone())
            }
        });
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        let calls = handle.calls();
        let methods: Vec<&str> = calls.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["/multihop", "/traverse"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "n3");
        // Synthesised hops=1 → similarity=0.5.
        assert!((out[0].similarity - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn expand_falls_back_to_traverse_when_multihop_errors() {
        let (client, handle) = mock::new_with_responder(|call| {
            if call.method == "/multihop" {
                Reply::err_msg("pipe_io", "boom")
            } else if call.method == "/traverse" {
                Reply::ok(json!({"chunks": [{"id": "n9"}]}))
            } else {
                Reply::err_msg("no_route", call.method.clone())
            }
        });
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        let calls = handle.calls();
        let methods: Vec<&str> = calls.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["/multihop", "/traverse"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "n9");
    }

    #[tokio::test]
    async fn expand_returns_empty_when_all_paths_empty() {
        let (client, _) = mock::new_with_static_ok(json!({"chunks": []}));
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn expand_dedupes_seed_chunks_and_duplicates() {
        // Same chunk surfaces with id "n1" (a seed) → dropped.
        // Same chunk "dup" appears twice → only kept once.
        let (client, _) = mock::new_with_static_ok(json!({
            "chunks": [
                {"id": "n1", "hops": 1},
                {"id": "dup", "hops": 2},
                {"id": "dup", "hops": 3},
                {"id": "fresh", "hops": 2},
            ]
        }));
        let cands = vec![json!({"id": "n1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        let ids: Vec<&str> = out.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["dup", "fresh"]);
    }

    #[tokio::test]
    async fn expand_caps_at_max_extra() {
        let chunks: Vec<Value> = (0..50).map(|i| json!({"id": format!("n{i}"), "hops": 1})).collect();
        let (client, _) = mock::new_with_static_ok(json!({"chunks": chunks}));
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 7)).await;
        assert_eq!(out.len(), 7);
    }

    #[tokio::test]
    async fn expand_passes_workspace_through_to_traverse() {
        let (client, handle) = mock::new_with_responder(|call| {
            if call.method == "/multihop" {
                Reply::ok(json!({"chunks": []}))
            } else {
                Reply::ok(json!({"chunks": [{"id": "n1"}]}))
            }
        });
        let opts = ExpandOptions {
            workspace_id: "ws-99".into(),
            hops: 1,
            max_extra: 5,
            seed_entities: Vec::new(),
        };
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        expand_by_graph(&client, cands, opts).await;
        let trv = handle
            .calls()
            .iter()
            .find(|c| c.method == "/traverse")
            .cloned()
            .expect("traverse called");
        assert_eq!(trv.payload["workspace"], "ws-99");
    }

    #[tokio::test]
    async fn expand_runs_entity_seed_traverse_independently() {
        let (client, handle) = mock::new_with_responder(|call| {
            if call.method == "/multihop" {
                Reply::ok(json!({"chunks": [{"id": "from-mh", "hops": 1, "via_entities": ["cand_ent"]}]}))
            } else if call.method == "/traverse" {
                Reply::ok(json!({"chunks": [{"id": "from-seed"}]}))
            } else {
                Reply::err_msg("no_route", call.method.clone())
            }
        });
        let cands = vec![json!({"id": "c1", "entities": ["cand_ent"]})];
        let opts = ExpandOptions {
            workspace_id: String::new(),
            hops: 1,
            max_extra: 10,
            seed_entities: vec!["query_ent".into()],
        };
        let out = expand_by_graph(&client, cands, opts).await;
        let ids: HashSet<String> = out.iter().map(|h| h.id.clone()).collect();
        assert!(ids.contains("from-mh"), "candidate path");
        assert!(ids.contains("from-seed"), "entity-seed path");
        // Pin that traverse was called with the entity_seeds payload.
        let trv = handle
            .calls()
            .iter()
            .find(|c| c.method == "/traverse")
            .cloned()
            .expect("traverse called");
        assert_eq!(trv.payload["entities"][0], "query_ent");
    }

    #[tokio::test]
    async fn expand_normalises_alternate_field_names() {
        // Server might emit `chunk_id` instead of `id`, `distance` instead of `hops`,
        // `entities` instead of `via_entities`, `body` instead of `content`.
        let (client, _) = mock::new_with_static_ok(json!({
            "chunks": [
                {"chunk_id": "alt-id", "distance": 2, "entities": ["e1"], "body": "code"}
            ]
        }));
        let cands = vec![json!({"id": "c1", "entities": ["foo"]})];
        let out = expand_by_graph(&client, cands, opts_with(1, 10)).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "alt-id");
        assert_eq!(out[0].hops, 2);
        assert_eq!(out[0].via_entities, vec!["e1".to_owned()]);
        assert_eq!(out[0].content, "code");
    }
}
