//! The `workspaces.concepts.*` verb handlers (TBS concept-system Phase 0).
//!
//! Read/write/curate the per-workspace concept [`store`]:
//!
//!   * `list` / `get` / `update` / `delete` — CRUD over `concepts.json`.
//!   * `build` — the Phase-0 cheap-concept pass: read the workspace code graph
//!     ([`crate::graph::api::graph`]), label its directory clusters
//!     ([`crate::concepts::cheap`]), and replace the concept set. Idempotent.
//!   * `reverse_lookup` — from a symbol/file → the concepts (and vocabulary
//!     anchors) it belongs to (thesis §4.2). A pure store query; no Neo4j.
//!
//! Every concept-bearing reply uses the [`Concept`] serde shape directly.

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use super::cheap;
use super::concept::Concept;
use super::store::{self, ConceptPatch, CreateOutcome, UpdateOutcome};
use crate::anchors::store as anchor_store;

fn require_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn opt_str_array(payload: &Value, key: &str) -> Option<Vec<String>> {
    payload.get(key).and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn concepts_reply(workspace_id: &str, extra: &[(&str, Value)], concepts: &[Concept]) -> Reply {
    let mut obj = json!({
        "workspace_id": workspace_id,
        "count": concepts.len(),
        "concepts": concepts,
    });
    for (k, v) in extra {
        obj[*k] = v.clone();
    }
    Reply::ok(obj)
}

/// `workspaces.concepts.list` — every concept for a workspace.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    concepts_reply(&ws, &[], &store::load(&ws))
}

/// `workspaces.concepts.get` — one concept by id (with members + files).
pub async fn handle_get(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::get(&ws, &id) {
        Some(c) => Reply::ok(json!(c)),
        None => Reply::err_msg("not_found", format!("no concept {id:?} in this workspace")),
    }
}

/// `workspaces.concepts.build` — build the concept set, **preferring semantic
/// clustering** when the workspace has an embedding index, else falling back to
/// the Phase-0 directory-cluster stand-ins (thesis §7: Phase 2 upgrades the
/// concept *source* without changing the browse UI — the same Build button
/// "instantly gets better"). Idempotent (deterministic clustering / labeling).
/// Manually-authored concepts are preserved across a rebuild.
///
/// Returns `{workspace_id, built, projected, source}` where `source` is
/// `embedding` (semantic) or `directory_cluster` (fallback).
pub async fn handle_build(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };

    let force = payload
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Prefer semantic clustering when an embedding index exists.
    let chunks = crate::rag::indexer::store::load_chunks(&ws);
    let have_vectors = chunks.iter().filter(|c| !c.vector.is_empty()).count();
    if have_vectors >= 2 {
        return build_semantic_stable(
            &ws,
            &chunks,
            super::semantic::SemanticParams::default(),
            force,
        )
        .await;
    }

    // #137 — the index cannot support a semantic build. Falling through to the
    // directory fallback REPLACES the whole auto-generated set (`finish_build`
    // keeps only `Manual` concepts), so every `sem:` concept would be dropped
    // unrecoverably. Refuse when that would spend authored relations (shared
    // guard — the explicit `build_semantic` verb gates on it too).
    if let Some(refusal) =
        refuse_empty_index_rebuild(&ws, have_vectors, semantic_concept_count(&ws), force)
    {
        return refusal;
    }

    // Fallback: label the directory clusters from the live code graph.
    let graph = match crate::graph::api::graph(&ws).await {
        Ok(g) => g,
        Err(e) => return e.to_reply(),
    };
    let concepts = cheap::build_concepts(&graph);
    finish_build(&ws, concepts, "directory_cluster").await
}

/// How many `Embedding`-sourced concepts the workspace currently stores — the
/// set a directory-fallback build would drop (#137).
fn semantic_concept_count(ws: &str) -> usize {
    store::load(ws)
        .iter()
        .filter(|c| c.source == super::concept::ConceptSource::Embedding)
        .count()
}

/// #137/#209 — refuse a rebuild that an empty or torn chunk index would turn
/// into an unrecoverable drop of the existing semantic concepts.
///
/// A build with fewer than two usable vectors produces no `sem:` concepts, so
/// `finish_build` keeps only `Manual` ones and every `sem:` concept is dropped.
/// Their ordinals are never recycled, so a later build over a restored index
/// mints NEW ids that can never re-match the authored relations anchored on the
/// old ones — the edges survive on disk but are permanently inert. An empty or
/// torn index is exactly the transient condition (a purge, an interrupted
/// reindex, a `data_dir` resolved against the wrong cwd) that must NOT be
/// allowed to spend the user's hand-authored work.
///
/// Returns the refusal reply when there ARE semantic concepts to lose and the
/// caller has not forced it; `None` when it is safe to proceed — enough vectors
/// to cluster, nothing to lose, or an explicit `force`. Both the auto verb
/// ([`handle_build`]) and the explicit `build_semantic` verb
/// ([`build_semantic_stable`]) gate on this, so neither entry point can orphan
/// authored relations on an empty index.
fn refuse_empty_index_rebuild(
    ws: &str,
    have_vectors: usize,
    prior_semantic: usize,
    force: bool,
) -> Option<Reply> {
    if have_vectors >= 2 || prior_semantic == 0 || force {
        return None;
    }
    tracing::error!(
        "workspaces.concepts.build: refusing to rebuild {ws} — the chunk index has \
         {have_vectors} usable vectors but {prior_semantic} semantic concepts exist \
         and would be dropped unrecoverably (#137/#209)"
    );
    Some(Reply::err_msg(
        "index_unavailable",
        format!(
            "The chunk index has no usable embeddings, so this rebuild would drop all \
             {prior_semantic} semantic concepts. Semantic ids are never reused, so any \
             relations you authored on them could not be reattached by a later rebuild. \
             Reindex the workspace first, or pass force=true to accept the loss."
        ),
    ))
}

/// A carry-over pool that cannot possibly match the incoming vectors (#137).
#[derive(Debug)]
struct WidthMismatch {
    /// Distinct centroid widths found in the prior concepts.
    prior_widths: Vec<usize>,
    /// Width of the vectors the new build will cluster.
    incoming_width: usize,
}

/// Detect an embedding-width change that would silently void the whole
/// carry-over pool.
///
/// Returns `Some` only when there IS a pool with usable centroids and **none**
/// of its widths match the incoming vectors — i.e. `assign_stable_ids` is
/// guaranteed to produce zero candidate pairs. A pool that merely drifted below
/// the cosine threshold is a different (legitimate) situation and is not
/// flagged here; so is a first build, where there is nothing to carry.
fn carry_over_width_mismatch(
    prior_emb: &[Concept],
    chunks: &[crate::rag::indexer::store::IndexedChunk],
) -> Option<WidthMismatch> {
    let mut prior_widths: Vec<usize> = prior_emb
        .iter()
        .filter_map(|c| c.centroid.as_ref())
        .map(Vec::len)
        .collect();
    prior_widths.sort_unstable();
    prior_widths.dedup();
    if prior_widths.is_empty() {
        return None; // nothing to carry over — first build, or no centroids
    }
    let incoming_width = chunks.iter().find(|c| !c.vector.is_empty())?.vector.len();
    if prior_widths.contains(&incoming_width) {
        return None; // at least some of the pool can still match
    }
    Some(WidthMismatch {
        prior_widths,
        incoming_width,
    })
}

/// `workspaces.concepts.build_semantic` — force the embedding-clustering build
/// (thesis S2.1/S2.2), regardless of the auto-choice. Payload may carry
/// `{k?, overlap_margin?, seed?}` overrides. Reply mirrors `build`. Returns
/// `built: 0, source: "embedding"` when the index has too few vectors.
pub async fn handle_build_semantic(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let mut params = super::semantic::SemanticParams::default();
    if let Some(k) = payload.get("k").and_then(Value::as_u64) {
        params.k = Some(k as usize);
    }
    if let Some(m) = payload.get("overlap_margin").and_then(Value::as_f64) {
        params.overlap_margin = m as f32;
    }
    if let Some(s) = payload.get("seed").and_then(Value::as_u64) {
        params.seed = s;
    }
    let force = payload
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let chunks = crate::rag::indexer::store::load_chunks(&ws);
    build_semantic_stable(&ws, &chunks, params, force).await
}

/// Build semantic concepts with **stable ids** (Phase-B §4.1): feed the prior
/// `Embedding` concepts + the persisted never-reused ordinal allocator into the
/// clustering so a recompute carries ids over to drifted themes and mints fresh
/// ids for new ones, then persist the advanced allocator. Delegates the store
/// swap + projection + dangling sweep to [`finish_build`].
async fn build_semantic_stable(
    ws: &str,
    chunks: &[crate::rag::indexer::store::IndexedChunk],
    params: super::semantic::SemanticParams,
    force: bool,
) -> Reply {
    let prior_emb: Vec<Concept> = store::load(ws)
        .into_iter()
        .filter(|c| c.source == super::concept::ConceptSource::Embedding)
        .collect();

    // #137/#209 — an empty or torn index yields zero `sem:` concepts, so the
    // store swap in `finish_build` would drop every existing one and orphan the
    // authored relations anchored on them. `handle_build` guards this for the
    // auto path, but the explicit `build_semantic` verb reaches here directly,
    // so gate it here too (no-op for the auto path, which only calls in with
    // have_vectors >= 2).
    let have_vectors = chunks.iter().filter(|c| !c.vector.is_empty()).count();
    if let Some(refusal) = refuse_empty_index_rebuild(ws, have_vectors, prior_emb.len(), force) {
        return refusal;
    }

    // #137 — carry-over matches prior centroids to new drafts by cosine, and
    // only ever pairs vectors of EQUAL length (`semantic::assign_stable_ids`).
    // So if the embedding width changed, the candidate-pair list comes out
    // empty, every draft mints a fresh ordinal, and every authored relation
    // goes dangling in a single build — with nothing but a nonzero
    // `dangling_count` in the reply to say so.
    //
    // Detect the condition up front: a non-empty carry-over pool whose
    // centroids are ALL a different width than the incoming vectors means
    // carry-over is arithmetically impossible, not merely unlikely.
    if let Some(mismatch) = carry_over_width_mismatch(&prior_emb, chunks) {
        if !force {
            tracing::error!(
                "workspaces.concepts.build: refusing to rebuild {ws} — embedding width \
                 changed from {:?} to {} so no prior concept id can carry over (#137)",
                mismatch.prior_widths,
                mismatch.incoming_width,
            );
            return Reply::err_msg(
                "embedding_width_changed",
                format!(
                    "The embedding width changed (stored concepts are {:?}-dimensional, \
                     the index is now {}-dimensional), so no concept id can be carried \
                     over and all {} semantic concepts would be reminted — dangling every \
                     relation authored on them. Rebuild the index under the previous \
                     embedder, or pass force=true to accept the loss.",
                    mismatch.prior_widths,
                    mismatch.incoming_width,
                    prior_emb.len(),
                ),
            );
        }
        tracing::warn!(
            "workspaces.concepts.build: forced rebuild of {ws} across an embedding-width \
             change — {} concepts will be reminted and their authored relations will dangle",
            prior_emb.len(),
        );
    }

    let mut ident = super::identity::load(ws);
    let out = super::semantic::build_semantic_concepts_stable(
        chunks,
        &params,
        &prior_emb,
        ident.next_sem_ordinal,
    );
    ident.next_sem_ordinal = out.next_ordinal;
    if let Err(e) = super::identity::save(ws, &ident) {
        tracing::warn!("workspaces.concepts: persist id allocator failed for {ws}: {e}");
    }
    finish_build(ws, out.concepts, "embedding").await
}

/// Shared tail of the build verbs: preserve manually-authored concepts, replace
/// the rest with `built`, additively project into the graph (fail-soft), then
/// re-validate authored relations against the new concept set (Phase-B §4.2:
/// flag edges to dropped concepts `dangling`, never delete them).
async fn finish_build(ws: &str, built: Vec<Concept>, source: &str) -> Reply {
    // Preserve curated (Manual) concepts; replace the auto-generated set.
    let mut concepts: Vec<Concept> = store::load(ws)
        .into_iter()
        .filter(|c| c.source == super::concept::ConceptSource::Manual)
        .collect();
    concepts.extend(built);

    // Build the graph-projection rows before the store swap moves `concepts`.
    let rows: Vec<Value> = concepts
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "description": c.description,
                "source": c.source.as_str(),
                "members": c.members,
                "parents": c.parent_concepts,
            })
        })
        .collect();

    let built = match store::replace_all(ws, concepts) {
        Ok(n) => n,
        Err(e) => return Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    };

    // Additively project into the graph so the panel can render concept nodes.
    // Best-effort: the JSON store is authoritative, so a projection failure
    // (Neo4j hiccup) is logged, never fatal to the build.
    let projected = match crate::graph::BoltClient::new()
        .project_concepts(ws, rows)
        .await
    {
        reply if reply.ok => reply.data.get("projected").cloned().unwrap_or(json!(0)),
        reply => {
            tracing::warn!(
                workspace = %ws,
                error = ?reply.error,
                "concept graph projection failed (non-fatal; JSON store is authoritative)"
            );
            json!(0)
        }
    };

    // Re-validate authored relations against the new concept set: an edge whose
    // concept id the recompute dropped is flagged `dangling` (surfaced, excluded
    // from routing) — never deleted (Phase-B §4.2).
    let dangling_count = super::relations_bridge::sweep_dangling(ws);

    Reply::ok(json!({
        "workspace_id": ws,
        "built": built,
        "projected": projected,
        "source": source,
        "dangling_count": dangling_count,
    }))
}

/// `workspaces.concepts.update` — patch a concept's
/// label/description/members/parents/described_by. `not_found` for an unknown id.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let patch = ConceptPatch {
        label: require_str(&payload, "label"),
        description: payload
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        members: opt_str_array(&payload, "members"),
        member_files: opt_str_array(&payload, "member_files"),
        parent_concepts: opt_str_array(&payload, "parent_concepts"),
        described_by: opt_str_array(&payload, "described_by"),
    };
    match store::update(&ws, &id, patch) {
        Ok(UpdateOutcome::Updated(c)) => Reply::ok(json!(c)),
        Ok(UpdateOutcome::NotFound) => {
            Reply::err_msg("not_found", format!("no concept {id:?} in this workspace"))
        }
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.create` — hand-author one concept (curation).
/// `already_exists` on a duplicate id.
pub async fn handle_create(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let label = require_str(&payload, "label").unwrap_or_else(|| id.clone());
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut concept = Concept::new(
        id,
        label,
        description,
        super::concept::ConceptSource::Manual,
    );
    if let Some(m) = opt_str_array(&payload, "members") {
        concept.members = m;
    }
    if let Some(f) = opt_str_array(&payload, "member_files") {
        concept.member_files = f;
    }
    if let Some(p) = opt_str_array(&payload, "parent_concepts") {
        concept.parent_concepts = p;
    }
    match store::create(&ws, concept) {
        Ok(CreateOutcome::Created(c)) => Reply::ok(json!(c)),
        Ok(CreateOutcome::AlreadyExists(c)) => Reply::err_msg(
            "already_exists",
            format!("concept {:?} already exists in this workspace", c.id),
        ),
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.delete` — remove a concept by id.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match store::delete(&ws, &id) {
        Ok(removed) => Reply::ok(json!({ "ok": true, "removed": removed, "id": id })),
        Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
    }
}

/// `workspaces.concepts.list_under` — concepts whose parent set contains
/// `parent_id` (DAG child traversal).
pub async fn handle_list_under(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(parent_id) = require_str(&payload, "parent_id") else {
        return Reply::err_msg("bad_request", "parent_id is required");
    };
    let kids = store::list_under(&ws, &parent_id);
    concepts_reply(&ws, &[("parent_id", json!(parent_id))], &kids)
}

/// `workspaces.concepts.search` — hybrid (fuzzy + semantic) search over a
/// workspace's concepts (thesis §3.2). Payload: {workspace_id, query, limit?}.
/// Reply: {workspace_id, query, results:[{concept, score, fuzzy, semantic}],
/// count}. An empty query returns the full set ordered by label.
pub async fn handle_search(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let query = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(50);
    let results = super::search::search(&ws, &query, limit).await;
    Reply::ok(json!({
        "workspace_id": ws,
        "query": query,
        "count": results.len(),
        "results": results,
    }))
}

/// `workspaces.concepts.reverse_lookup` — from a `symbol_id` (and/or `file`) to
/// the concepts and vocabulary it belongs to (thesis §4.2). Pure store query;
/// no Neo4j. Reply: `{workspace_id, symbol_id?, file?, concepts, vocabulary}`
/// where `vocabulary` is the anchors targeting that symbol (the curated terms).
pub async fn handle_reverse_lookup(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let symbol_id = require_str(&payload, "symbol_id");
    let file = require_str(&payload, "file");
    if symbol_id.is_none() && file.is_none() {
        return Reply::err_msg("bad_request", "one of symbol_id or file is required");
    }

    // Concepts: union of member-match (by symbol) and file-match (by file).
    let mut concepts: Vec<Concept> = Vec::new();
    if let Some(sym) = &symbol_id {
        concepts.extend(store::find_by_member(&ws, sym));
    }
    if let Some(f) = &file {
        for c in store::find_by_file(&ws, f) {
            if !concepts.iter().any(|e| e.id == c.id) {
                concepts.push(c);
            }
        }
    }
    concepts.sort_by(|a, b| a.id.cmp(&b.id));

    // Vocabulary: the anchors targeting that symbol (curated terms naming it).
    let vocabulary: Vec<Value> = symbol_id
        .as_deref()
        .map(|s| {
            anchor_store::find_by_target(&ws, s)
                .iter()
                .map(wylde_shared::anchor::Anchor::to_value)
                .collect()
        })
        .unwrap_or_default();

    Reply::ok(json!({
        "workspace_id": ws,
        "symbol_id": symbol_id,
        "file": file,
        "concepts": concepts,
        "vocabulary": vocabulary,
    }))
}

// ── Concept-driven retrieval (Phase 3; routing deferred) ─────────────────

/// `workspaces.concepts.lens` — a concept seen "within" a scope (thesis §3.1):
/// `lens(concept, scope) = members ∩ region(scope)`. Payload: {workspace_id, id,
/// scope?}. Reply: {concept_id, scope, files, count}. Pure store query.
pub async fn handle_lens(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let scope = require_str(&payload, "scope");
    let Some(concept) = store::get(&ws, &id) else {
        return Reply::err_msg("not_found", format!("no concept {id:?} in this workspace"));
    };
    let files: Vec<&String> = super::lens::lens(&concept.member_files, scope.as_deref());
    Reply::ok(json!({
        "concept_id": id,
        "scope": scope,
        "count": files.len(),
        "files": files,
    }))
}

/// `workspaces.concepts.retrieve` — concept-driven retrieval (thesis §3.3): the
/// concept as the RAG unit. Selects representative member chunks (cosine to the
/// concept centroid, MMR-diversified), optionally scoped by a §3.1 lens.
/// Payload: {workspace_id, id, scope?, k?=5}. Reply: {concept_id, scope,
/// snippets:[{path,start_line,end_line,content,score}], count}.
///
/// This is the retrieval *mechanism*; query→concept *routing* (which concepts
/// to activate per turn) is the explicitly-deferred §3.4 phase.
pub async fn handle_retrieve(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let scope = require_str(&payload, "scope");
    let k = payload
        .get("k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(5);
    let Some(concept) = store::get(&ws, &id) else {
        return Reply::err_msg("not_found", format!("no concept {id:?} in this workspace"));
    };
    let allowed: std::collections::HashSet<String> =
        super::lens::lens(&concept.member_files, scope.as_deref())
            .into_iter()
            .cloned()
            .collect();
    let chunks = crate::rag::indexer::store::load_chunks(&ws);
    let snippets =
        super::retrieve::select_member_chunks(concept.centroid.as_deref(), &chunks, &allowed, k);
    Reply::ok(json!({
        "concept_id": id,
        "scope": scope,
        "count": snippets.len(),
        "snippets": snippets,
    }))
}

/// `workspaces.concepts.freshness` — concept drift detection (thesis S4.3).
/// Payload: {workspace_id, id?}. With `id`, one verdict; without, all concepts.
/// Reply: {workspace_id, freshness:[{id, stale, churned_files, missing_files,
/// built_at, newest_member_mtime}], stale_count}. Pure store + chunk query.
pub async fn handle_freshness(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let chunks = crate::rag::indexer::store::load_chunks(&ws);
    let mtimes = super::freshness::file_mtimes_from_chunks(&chunks);
    let id = require_str(&payload, "id");
    let concepts: Vec<Concept> = match &id {
        Some(one) => match store::get(&ws, one) {
            Some(c) => vec![c],
            None => return Reply::err_msg("not_found", format!("no concept {one:?}")),
        },
        None => store::load(&ws),
    };
    let verdicts: Vec<_> = concepts
        .iter()
        .map(|c| super::freshness::assess(c, &mtimes))
        .collect();
    let stale_count = verdicts.iter().filter(|v| v.stale).count();
    Reply::ok(json!({
        "workspace_id": ws,
        "stale_count": stale_count,
        "freshness": verdicts,
    }))
}

// ── Concept curation loop (S2.3) ─────────────────────────────────────────

/// `workspaces.concepts.propose` — queue an AI-proposed concept for review
/// (NOT persisted to concepts.json; user-accept-always). Payload: {workspace_id,
/// id, label?, description?, members?, confidence?, rationale?}. Reply:
/// {queued|already_pending|suppressed}.
pub async fn handle_propose(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let label = require_str(&payload, "label").unwrap_or_else(|| id.clone());
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut concept = Concept::new(
        id,
        label,
        description,
        super::concept::ConceptSource::Manual,
    );
    if let Some(m) = opt_str_array(&payload, "members") {
        concept.members = m;
    }
    let proposal = super::proposals::PendingConceptProposal {
        concept,
        confidence: payload
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32,
        rationale: payload
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        proposed_at: wylde_shared::anchor::epoch_now(),
    };
    use super::proposals::QueueOutcome;
    match super::proposals::queue(&ws, proposal, wylde_shared::anchor::epoch_now()) {
        Ok(outcome) => {
            let s = match outcome {
                QueueOutcome::Queued => "queued",
                QueueOutcome::AlreadyPending => "already_pending",
                QueueOutcome::Suppressed => "suppressed",
            };
            Reply::ok(json!({ "outcome": s }))
        }
        Err(e) => Reply::err_msg("io_error", format!("write concept_proposals.json: {e}")),
    }
}

/// `workspaces.concepts.list_proposals` — pending concept proposals.
pub async fn handle_list_proposals(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let file = super::proposals::load(&ws);
    Reply::ok(json!({
        "workspace_id": ws,
        "count": file.pending.len(),
        "proposals": file.pending,
    }))
}

/// `workspaces.concepts.accept_proposal` — land a pending proposal in the store.
pub async fn handle_accept_proposal(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match super::proposals::take(&ws, &id) {
        Ok(Some(p)) => match store::upsert(&ws, p.concept) {
            Ok(c) => Reply::ok(json!({ "accepted": true, "concept": c })),
            Err(e) => Reply::err_msg("io_error", format!("write concepts.json: {e}")),
        },
        Ok(None) => Reply::err_msg("not_found", format!("no pending proposal {id:?}")),
        Err(e) => Reply::err_msg("io_error", format!("read concept_proposals.json: {e}")),
    }
}

/// `workspaces.concepts.reject_proposal` — dismiss + record suppression.
pub async fn handle_reject_proposal(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    match super::proposals::reject(&ws, &id, wylde_shared::anchor::epoch_now()) {
        Ok(rejected) => Reply::ok(json!({ "ok": true, "rejected": rejected, "id": id })),
        Err(e) => Reply::err_msg("io_error", format!("write concept_proposals.json: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;
    use crate::test_support::TestEnv;

    fn seed(ws: &str) {
        let mut a = Concept::new(
            "dir:src/graph",
            "Graph",
            "the graph",
            ConceptSource::DirectoryCluster,
        );
        a.members = vec!["alpha".into(), "shared".into()];
        a.member_files = vec!["src/graph/api.rs".into()];
        let mut b = Concept::new(
            "dir:src/rag",
            "Rag",
            "retrieval",
            ConceptSource::DirectoryCluster,
        );
        b.members = vec!["shared".into()];
        b.member_files = vec!["src/rag/search.rs".into()];
        b.parent_concepts = vec!["dir:src/graph".into()];
        store::replace_all(ws, vec![a, b]).unwrap();
    }

    #[tokio::test]
    async fn list_requires_workspace_id() {
        let r = handle_list(json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn list_and_get_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-api-con-0000";
        seed(ws);
        let list = handle_list(json!({ "workspace_id": ws })).await;
        assert!(list.ok);
        assert_eq!(list.data["count"], 2);

        let got = handle_get(json!({ "workspace_id": ws, "id": "dir:src/rag" })).await;
        assert!(got.ok);
        assert_eq!(got.data["label"], "Rag");

        let miss = handle_get(json!({ "workspace_id": ws, "id": "nope" })).await;
        assert!(!miss.ok);
        assert_eq!(miss.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn update_and_delete() {
        let _env = TestEnv::new();
        let ws = "ws-api-upd-0000";
        seed(ws);
        let upd = handle_update(json!({
            "workspace_id": ws, "id": "dir:src/graph",
            "label": "Graph Layer", "described_by": ["graph_term"]
        }))
        .await;
        assert!(upd.ok);
        assert_eq!(upd.data["label"], "Graph Layer");
        assert_eq!(upd.data["described_by"][0], "graph_term");

        let del = handle_delete(json!({ "workspace_id": ws, "id": "dir:src/rag" })).await;
        assert!(del.ok);
        assert_eq!(del.data["removed"], true);
        assert_eq!(store::load(ws).len(), 1);
    }

    #[tokio::test]
    async fn list_under_returns_children() {
        let _env = TestEnv::new();
        let ws = "ws-api-under-000";
        seed(ws);
        let r =
            handle_list_under(json!({ "workspace_id": ws, "parent_id": "dir:src/graph" })).await;
        assert!(r.ok);
        assert_eq!(r.data["count"], 1);
        assert_eq!(r.data["concepts"][0]["id"], "dir:src/rag");
    }

    #[tokio::test]
    async fn reverse_lookup_unions_member_and_file() {
        let _env = TestEnv::new();
        let ws = "ws-api-rev-0000";
        seed(ws);
        // "shared" is a member of both concepts.
        let by_sym =
            handle_reverse_lookup(json!({ "workspace_id": ws, "symbol_id": "shared" })).await;
        assert!(by_sym.ok);
        assert_eq!(by_sym.data["concepts"].as_array().unwrap().len(), 2);

        // file match narrows to one.
        let by_file =
            handle_reverse_lookup(json!({ "workspace_id": ws, "file": "src/rag/search.rs" })).await;
        assert!(by_file.ok);
        assert_eq!(by_file.data["concepts"].as_array().unwrap().len(), 1);
        assert_eq!(by_file.data["concepts"][0]["id"], "dir:src/rag");
    }

    #[tokio::test]
    async fn reverse_lookup_requires_a_key() {
        let _env = TestEnv::new();
        let ws = "ws-api-rev2-000";
        let r = handle_reverse_lookup(json!({ "workspace_id": ws })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let _env = TestEnv::new();
        let ws = "ws-api-cr-0000";
        let mk = || json!({ "workspace_id": ws, "id": "manual:x", "label": "X" });
        assert!(handle_create(mk()).await.ok);
        let dup = handle_create(mk()).await;
        assert!(!dup.ok);
        assert_eq!(dup.error.unwrap().code, "already_exists");
    }

    fn idx_chunk(id: &str, path: &str, v: Vec<f32>) -> crate::rag::indexer::store::IndexedChunk {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let v: Vec<f32> = if n > 0.0 {
            v.iter().map(|x| x / n).collect()
        } else {
            v
        };
        crate::rag::indexer::store::IndexedChunk {
            id: id.to_owned(),
            path: path.to_owned(),
            chunk_idx: 0,
            content: String::new(),
            mtime: 0.0,
            start_line: 1,
            end_line: 1,
            vector: v,
        }
    }

    #[tokio::test]
    async fn build_semantic_clusters_chunks_and_preserves_manual() {
        let _env = TestEnv::new();
        let ws = "ws-api-sem-0000";
        // A hand-authored concept must survive a rebuild.
        handle_create(json!({ "workspace_id": ws, "id": "manual:keep", "label": "Keep" })).await;
        // Write a tiny two-theme index.
        let mut chunks = Vec::new();
        for j in 0..4 {
            chunks.push(idx_chunk(
                &format!("a{j}"),
                "src/auth/a.rs",
                vec![1.0, 0.02 * j as f32, 0.0],
            ));
            chunks.push(idx_chunk(
                &format!("g{j}"),
                "src/graph/g.rs",
                vec![0.0, 0.02 * j as f32, 1.0],
            ));
        }
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();

        let r = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data["source"], "embedding");
        let all = store::load(ws);
        // manual concept preserved + semantic concepts added.
        assert!(all.iter().any(|c| c.id == "manual:keep"));
        assert!(all
            .iter()
            .any(|c| c.id.starts_with("sem:") && c.centroid.is_some()));
    }

    #[tokio::test]
    async fn stable_ids_let_authored_relation_survive_recompute() {
        let _env = TestEnv::new();
        let ws = "ws-api-stab-00";
        // A clean two-theme index.
        let mut chunks = Vec::new();
        for j in 0..5 {
            chunks.push(idx_chunk(
                &format!("a{j}"),
                "src/auth/a.rs",
                vec![1.0, 0.02 * j as f32, 0.0],
            ));
            chunks.push(idx_chunk(
                &format!("g{j}"),
                "src/graph/g.rs",
                vec![0.0, 0.02 * j as f32, 1.0],
            ));
        }
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();

        let r1 = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(r1.ok, "{:?}", r1.error);
        let ids_before: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(ids_before.len(), 2, "two semantic themes");

        // Author a relation between the two semantic concepts.
        let add = crate::concepts::relations_bridge::handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id": ids_before[0]},
            "to": {"node":"concept","id": ids_before[1]},
            "kind": "positive",
        }))
        .await;
        assert!(add.ok, "{:?}", add.error);

        // Recompute over the same corpus → ids carried over → relation stays
        // live (not dangling) and the ids are unchanged.
        let r2 = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(r2.ok, "{:?}", r2.error);
        assert_eq!(
            r2.data["dangling_count"], 0,
            "carried-over ids keep the relation live"
        );
        let ids_after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(
            ids_before, ids_after,
            "semantic ids stable across recompute"
        );
    }

    /// Seed a two-theme semantic build and author a relation across it.
    /// Returns `(ws, [id_a, id_b])`.
    async fn seed_two_themes_with_authored_relation(ws: &str, dim_pad: usize) -> Vec<String> {
        let mut chunks = Vec::new();
        for j in 0..5 {
            let mut va = vec![1.0, 0.02 * j as f32, 0.0];
            let mut vg = vec![0.0, 0.02 * j as f32, 1.0];
            va.extend(std::iter::repeat_n(0.0, dim_pad));
            vg.extend(std::iter::repeat_n(0.0, dim_pad));
            chunks.push(idx_chunk(&format!("a{j}"), "src/auth/a.rs", va));
            chunks.push(idx_chunk(&format!("g{j}"), "src/graph/g.rs", vg));
        }
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        let r = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(r.ok, "seed build failed: {:?}", r.error);
        let ids: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(ids.len(), 2, "two semantic themes seeded");
        let add = crate::concepts::relations_bridge::handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id": ids[0]},
            "to": {"node":"concept","id": ids[1]},
            "kind": "positive",
        }))
        .await;
        assert!(add.ok, "seed relation failed: {:?}", add.error);
        ids
    }

    /// #137 criterion 1 — a build against an empty/torn chunk index must NOT
    /// silently fall back to directory clustering and drop every semantic
    /// concept.
    ///
    /// This is the highest-severity path in the issue and had no test at all.
    /// It is unrecoverable: semantic ordinals are never recycled, so a later
    /// build over a restored index mints new ids that can never re-match the
    /// authored relations. The transient condition (a purge, an interrupted
    /// reindex) must not be allowed to spend hand-authored work.
    #[tokio::test]
    async fn empty_index_build_refuses_rather_than_dropping_semantic_concepts() {
        let _env = TestEnv::new();
        let ws = "ws-api-emptyidx";
        let ids_before = seed_two_themes_with_authored_relation(ws, 0).await;

        // The index is purged / torn — no usable vectors remain.
        crate::rag::indexer::store::save_chunks(ws, &[]).unwrap();

        let r = handle_build(json!({ "workspace_id": ws })).await;
        assert!(
            !r.ok,
            "a build that would drop every semantic concept must refuse; got {:?}",
            r.data
        );
        assert_eq!(r.error.as_ref().unwrap().code, "index_unavailable");

        // Nothing was spent: the concepts and the live relation survive.
        let ids_after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(
            ids_before, ids_after,
            "the refused build must not have touched the concept store"
        );
        let g = crate::concepts::relations_bridge::load(ws);
        assert_eq!(g.relations.len(), 1);
        assert!(
            !g.relations[0].dangling,
            "the authored relation must still be live"
        );
    }

    /// #209 — the empty-index guard must ALSO cover the explicit
    /// `workspaces.concepts.build_semantic` verb, not just the auto `build`.
    /// That verb reaches `build_semantic_stable` directly; before this fix the
    /// guard lived only in `handle_build`, so a `build_semantic` over an empty
    /// index silently wiped every `sem:` concept (`finish_build` keeps only
    /// `Manual`) and orphaned authored relations. It is a live path: `rag.purge`
    /// empties the index and the documented follow-up step is exactly
    /// `concepts.build_semantic`. Fails before the fix (the verb returns `ok`
    /// and drops the concepts); passes after (it refuses like the auto verb).
    #[tokio::test]
    async fn build_semantic_verb_on_empty_index_refuses_rather_than_dropping() {
        let _env = TestEnv::new();
        let ws = "ws-api-semempty";
        let ids_before = seed_two_themes_with_authored_relation(ws, 0).await;

        // Purge / tear the index — no usable vectors remain.
        crate::rag::indexer::store::save_chunks(ws, &[]).unwrap();

        // The explicit semantic verb (not the auto build) must refuse too.
        let r = handle_build_semantic(json!({ "workspace_id": ws })).await;
        assert!(
            !r.ok,
            "the build_semantic verb must refuse an empty-index rebuild that would \
             drop every semantic concept; got {:?}",
            r.data
        );
        assert_eq!(r.error.as_ref().unwrap().code, "index_unavailable");

        // Nothing was spent: the concepts and the live relation survive.
        let ids_after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(
            ids_before, ids_after,
            "the refused verb must not have touched the concept store"
        );
        let g = crate::concepts::relations_bridge::load(ws);
        assert_eq!(g.relations.len(), 1);
        assert!(
            !g.relations[0].dangling,
            "the authored relation must still be live"
        );
    }

    /// The refusal is a guard, not a wall: an explicit `force` still performs
    /// the fallback, so a user who genuinely wants directory concepts is not
    /// stuck.
    ///
    /// `#[ignore]`: the forced path runs the directory-cluster build, which
    /// enumerates the live code graph over Bolt (`graph::api::graph`) and so
    /// needs a running Memgraph — the same reason the rest of the live-graph
    /// suite is ignored (cross-ref #121). The data-safety half of this pair —
    /// the *refusal* (criterion 1) — takes no graph path and IS gated in CI by
    /// `empty_index_build_refuses_rather_than_dropping_semantic_concepts`.
    #[tokio::test]
    #[ignore = "requires live Memgraph — the directory-cluster fallback reads the graph over Bolt"]
    async fn empty_index_build_proceeds_when_forced() {
        let _env = TestEnv::new();
        let ws = "ws-api-emptyidx-f";
        seed_two_themes_with_authored_relation(ws, 0).await;
        crate::rag::indexer::store::save_chunks(ws, &[]).unwrap();

        let r = handle_build(json!({ "workspace_id": ws, "force": true })).await;
        assert!(r.ok, "forced build must proceed: {:?}", r.error);
        assert_eq!(r.data["source"], "directory_cluster");
        // ...and the consequence is visible rather than hidden: the authored
        // edge is now flagged dangling, never deleted.
        let g = crate::concepts::relations_bridge::load(ws);
        assert_eq!(g.relations.len(), 1, "the edge is retained, not deleted");
        assert!(
            g.relations[0].dangling,
            "the edge must be surfaced as dangling after a forced drop"
        );
    }

    /// #137 criterion 2 — an embedding-width change makes carry-over
    /// arithmetically impossible (`assign_stable_ids` only pairs equal-length
    /// centroids), so every id would be reminted and every authored relation
    /// would dangle in one build. Detect it instead of absorbing it.
    #[tokio::test]
    async fn embedding_width_change_refuses_rather_than_reminting_every_id() {
        let _env = TestEnv::new();
        let ws = "ws-api-dimchg";
        let ids_before = seed_two_themes_with_authored_relation(ws, 0).await;

        // Re-index the same corpus at a different embedding width.
        let mut wide = Vec::new();
        for j in 0..5 {
            let mut va = vec![1.0, 0.02 * j as f32, 0.0];
            let mut vg = vec![0.0, 0.02 * j as f32, 1.0];
            va.extend(std::iter::repeat_n(0.0, 5)); // 3 -> 8 dims
            vg.extend(std::iter::repeat_n(0.0, 5));
            wide.push(idx_chunk(&format!("a{j}"), "src/auth/a.rs", va));
            wide.push(idx_chunk(&format!("g{j}"), "src/graph/g.rs", vg));
        }
        crate::rag::indexer::store::save_chunks(ws, &wide).unwrap();

        let r = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(
            !r.ok,
            "a width change that voids the whole carry-over pool must refuse; got {:?}",
            r.data
        );
        assert_eq!(r.error.as_ref().unwrap().code, "embedding_width_changed");

        // The prior ids and the live relation are untouched.
        let ids_after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(ids_before, ids_after, "ids must not have been reminted");
        assert!(!crate::concepts::relations_bridge::load(ws).relations[0].dangling);
    }

    /// The width guard must not fire on an ordinary rebuild at the SAME width —
    /// otherwise it would block the normal path. Guards that cry wolf get
    /// forced past reflexively.
    #[tokio::test]
    async fn same_width_rebuild_is_not_blocked_by_the_width_guard() {
        let _env = TestEnv::new();
        let ws = "ws-api-samewidth";
        let ids_before = seed_two_themes_with_authored_relation(ws, 0).await;

        let r = handle_build_semantic(json!({ "workspace_id": ws, "k": 2 })).await;
        assert!(r.ok, "a same-width rebuild must proceed: {:?}", r.error);
        assert_eq!(r.data["dangling_count"], 0);
        let ids_after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert_eq!(ids_before, ids_after);
    }

    /// #137 criterion 3 — the existing survival gate pins `k: 2` on both
    /// builds over a byte-identical corpus. Production uses `k: None`
    /// (`k = sqrt(n)` clamped), so corpus growth re-partitions the space and
    /// moves centroids. That realistic drift case was ungated.
    #[tokio::test]
    async fn ids_survive_a_growth_driven_k_change_with_default_params() {
        let _env = TestEnv::new();
        let ws = "ws-api-kgrowth";

        // Small corpus: sqrt(10) -> k = 3.
        let mut chunks = Vec::new();
        for j in 0..5 {
            chunks.push(idx_chunk(
                &format!("a{j}"),
                "src/auth/a.rs",
                vec![1.0, 0.01 * j as f32, 0.0],
            ));
            chunks.push(idx_chunk(
                &format!("g{j}"),
                "src/graph/g.rs",
                vec![0.0, 0.01 * j as f32, 1.0],
            ));
        }
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        // NOTE: no `k` — the production default.
        let r1 = handle_build(json!({ "workspace_id": ws })).await;
        assert!(r1.ok, "{:?}", r1.error);
        let before: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        assert!(!before.is_empty(), "seeded semantic concepts");

        // Author a relation on the first concept (self-edge avoided by using
        // the two extremes when available).
        let other = before.last().unwrap().clone();
        if other != before[0] {
            let add = crate::concepts::relations_bridge::handle_add(json!({
                "workspace_id": ws,
                "from": {"node":"concept","id": before[0]},
                "to": {"node":"concept","id": other},
                "kind": "positive",
            }))
            .await;
            assert!(add.ok, "{:?}", add.error);
        }

        // Grow the corpus enough to move sqrt(n): 10 -> 40 chunks, k 3 -> 6.
        for j in 5..20 {
            chunks.push(idx_chunk(
                &format!("a{j}"),
                "src/auth/a.rs",
                vec![1.0, 0.01 * j as f32, 0.0],
            ));
            chunks.push(idx_chunk(
                &format!("g{j}"),
                "src/graph/g.rs",
                vec![0.0, 0.01 * j as f32, 1.0],
            ));
        }
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        let r2 = handle_build(json!({ "workspace_id": ws })).await;
        assert!(r2.ok, "{:?}", r2.error);

        // The themes are the same two directions, so the prior ids must be
        // carried over even though k changed and the partition moved.
        let after: Vec<String> = store::load(ws)
            .into_iter()
            .filter(|c| c.id.starts_with("sem:"))
            .map(|c| c.id)
            .collect();
        let carried = before.iter().filter(|id| after.contains(id)).count();
        assert!(
            carried > 0,
            "corpus growth changed k and NO prior id carried over \
             (before={before:?}, after={after:?}) — authored relations would all dangle"
        );
    }

    /// #137 criterion 4 — a `dir:` concept is path-derived (`dir:<path>`) and
    /// has no carry-over pool at all (carry-over filters to `Embedding`/`sem:`
    /// only), so moving or renaming the directory produces a NEW id and the old
    /// one vanishes. A relation authored on it must then be SURFACED as
    /// dangling — retained on disk, excluded from routing — never silently
    /// dropped. This is the state the GUI change in this PR renders.
    ///
    /// Deliberately graph-free: the directory-cluster *build* enumerates the
    /// live code graph over Bolt (`graph::api::graph`), which needs a running
    /// Memgraph and is covered by the `#[ignore]`d live-graph suite. The
    /// property under test — a relation onto a vanished `dir:` concept goes
    /// dangling — is exercised here by seeding the store directly and running
    /// the same `sweep_dangling` that every build runs at its tail, so it gates
    /// in CI without a backend.
    #[tokio::test]
    async fn renaming_a_directory_surfaces_its_relations_as_dangling() {
        use crate::concepts::concept::{Concept, ConceptSource};

        let _env = TestEnv::new();
        let ws = "ws-api-dirrename";

        // A directory-sourced concept set, as a build's fallback would write.
        let dir_a = "dir:src/graph";
        let dir_b = "dir:src/rag";
        store::save(
            ws,
            &[
                Concept::new(dir_a, "graph", "", ConceptSource::DirectoryCluster),
                Concept::new(dir_b, "rag", "", ConceptSource::DirectoryCluster),
            ],
        )
        .unwrap();

        // Author a relation across the two directories; it starts live.
        let add = crate::concepts::relations_bridge::handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id": dir_a},
            "to": {"node":"concept","id": dir_b},
            "kind": "positive",
        }))
        .await;
        assert!(add.ok, "{:?}", add.error);
        assert!(
            !crate::concepts::relations_bridge::load(ws).relations[0].dangling,
            "the relation starts live"
        );

        // Rename `src/graph` → `src/graph_v2`. A rebuild re-derives dir ids from
        // paths and, having no carry-over for `dir:`, mints `dir:src/graph_v2`
        // while `dir:src/graph` simply ceases to exist. Model that by replacing
        // the concept set with the renamed layout.
        store::save(
            ws,
            &[
                Concept::new(
                    "dir:src/graph_v2",
                    "graph_v2",
                    "",
                    ConceptSource::DirectoryCluster,
                ),
                Concept::new(dir_b, "rag", "", ConceptSource::DirectoryCluster),
            ],
        )
        .unwrap();
        assert!(
            store::get(ws, dir_a).is_none(),
            "the renamed directory's old id no longer resolves"
        );

        // The tail-of-build sweep must flag — never delete — the now-orphaned
        // edge, and report it in its count.
        let dangling = crate::concepts::relations_bridge::sweep_dangling(ws);
        assert_eq!(dangling, 1, "the sweep must report the dangling edge");

        let g = crate::concepts::relations_bridge::load(ws);
        assert_eq!(g.relations.len(), 1, "the authored edge is never deleted");
        assert!(
            g.relations[0].dangling,
            "a relation onto a vanished dir: concept must be flagged dangling"
        );
    }

    #[tokio::test]
    async fn build_auto_prefers_semantic_when_index_exists() {
        let _env = TestEnv::new();
        let ws = "ws-api-auto-000";
        let chunks: Vec<_> = (0..6)
            .map(|j| idx_chunk(&format!("c{j}"), "src/m/x.rs", vec![1.0, 0.01 * j as f32]))
            .collect();
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        let r = handle_build(json!({ "workspace_id": ws })).await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data["source"], "embedding", "auto-build prefers semantic");
    }

    #[tokio::test]
    async fn proposal_loop_accept_and_reject() {
        let _env = TestEnv::new();
        let ws = "ws-api-cprop-00";
        // Propose two concepts.
        assert!(
            handle_propose(json!({ "workspace_id": ws, "id": "p1", "label": "P1" }))
                .await
                .ok
        );
        assert!(
            handle_propose(json!({ "workspace_id": ws, "id": "p2", "label": "P2" }))
                .await
                .ok
        );
        let listed = handle_list_proposals(json!({ "workspace_id": ws })).await;
        assert_eq!(listed.data["count"], 2);

        // Accept p1 → lands in the store.
        let acc = handle_accept_proposal(json!({ "workspace_id": ws, "id": "p1" })).await;
        assert!(acc.ok);
        assert!(store::get(ws, "p1").is_some());

        // Reject p2 → gone from pending, suppressed.
        let rej = handle_reject_proposal(json!({ "workspace_id": ws, "id": "p2" })).await;
        assert!(rej.ok);
        assert_eq!(rej.data["rejected"], true);
        assert_eq!(
            handle_list_proposals(json!({ "workspace_id": ws }))
                .await
                .data["count"],
            0
        );
        // Re-proposing p2 inside the window is suppressed.
        let re = handle_propose(json!({ "workspace_id": ws, "id": "p2", "label": "P2" })).await;
        assert_eq!(re.data["outcome"], "suppressed");
    }

    #[tokio::test]
    async fn lens_intersects_members_with_scope() {
        let _env = TestEnv::new();
        let ws = "ws-api-lens-000";
        let mut c = Concept::new(
            "dir:services",
            "Services",
            "d",
            ConceptSource::DirectoryCluster,
        );
        c.member_files = vec!["services/vpn/a.rs".into(), "services/auth/b.rs".into()];
        store::create(ws, c).unwrap();
        let all = handle_lens(json!({ "workspace_id": ws, "id": "dir:services" })).await;
        assert_eq!(all.data["count"], 2);
        let scoped = handle_lens(
            json!({ "workspace_id": ws, "id": "dir:services", "scope": "services/vpn" }),
        )
        .await;
        assert_eq!(scoped.data["count"], 1);
        assert_eq!(scoped.data["files"][0], "services/vpn/a.rs");
    }

    #[tokio::test]
    async fn retrieve_selects_member_chunks_by_centroid() {
        let _env = TestEnv::new();
        let ws = "ws-api-ret-0000";
        let mut c = Concept::new("sem:0000", "Theme", "d", ConceptSource::Embedding);
        c.member_files = vec!["src/x.rs".into()];
        c.centroid = Some(vec![1.0, 0.0]);
        store::create(ws, c).unwrap();
        // Two chunks in the member file; one aligned to the centroid.
        let chunks = vec![
            idx_chunk("x0", "src/x.rs", vec![0.0, 1.0]),
            idx_chunk("x1", "src/x.rs", vec![1.0, 0.0]),
        ];
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        let r = handle_retrieve(json!({ "workspace_id": ws, "id": "sem:0000", "k": 2 })).await;
        assert!(r.ok);
        assert_eq!(r.data["count"], 2);
        // Centroid-aligned chunk ranks first.
        assert_eq!(r.data["snippets"][0]["path"], "src/x.rs");
        let top = r.data["snippets"][0]["score"].as_f64().unwrap();
        let bot = r.data["snippets"][1]["score"].as_f64().unwrap();
        assert!(top >= bot);
    }

    #[tokio::test]
    async fn freshness_reports_per_concept_verdicts() {
        let _env = TestEnv::new();
        let ws = "ws-api-fresh-00";
        let mut c = Concept::new("sem:0000", "Theme", "d", ConceptSource::Embedding);
        c.member_files = vec!["src/x.rs".into(), "src/gone.rs".into()];
        c.updated_at = 100.0;
        store::create(ws, c).unwrap();
        // x.rs present (old mtime); gone.rs absent from the index.
        let chunks = vec![{
            let mut k = idx_chunk("x0", "src/x.rs", vec![1.0]);
            k.mtime = 50.0;
            k
        }];
        crate::rag::indexer::store::save_chunks(ws, &chunks).unwrap();
        let r = handle_freshness(json!({ "workspace_id": ws })).await;
        assert!(r.ok);
        assert_eq!(r.data["stale_count"], 1, "missing member file ⇒ stale");
        assert_eq!(r.data["freshness"][0]["missing_files"][0], "src/gone.rs");
    }

    #[tokio::test]
    async fn retrieve_unknown_concept_is_not_found() {
        let _env = TestEnv::new();
        let ws = "ws-api-ret-nf0";
        let r = handle_retrieve(json!({ "workspace_id": ws, "id": "ghost" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn accept_missing_proposal_is_not_found() {
        let _env = TestEnv::new();
        let ws = "ws-api-cprop-nf";
        let r = handle_accept_proposal(json!({ "workspace_id": ws, "id": "ghost" })).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }
}
