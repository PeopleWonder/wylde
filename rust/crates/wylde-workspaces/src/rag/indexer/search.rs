//! Vector search over a workspace's file index.
//!
//! Embeds the query once via the shared `nomic-embed-text` embedder
//! (`crate::embeddings`), then does a brute-force cosine scan over
//! the persisted chunks — see `store.rs` for why brute-force, not ANN.
//!
//! **Never errors.** A missing index, an empty query, or an unreachable
//! embedder all yield an empty result, so the pointer-only fallback holds:
//! `rag_query` returns `[]`, never an error.

use serde_json::{json, Value};

use super::store::{self, IndexedChunk};
use super::{fuse, lexical};
use crate::rag::{cosine, LexicalConfig};

/// One ranked search hit. Shape mirrors the retired Python verb:
/// `{file_path, line_range, content, score}`.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// Absolute source-file path.
    pub file_path: String,
    /// `[start_line, end_line]`, 1-based inclusive.
    pub line_range: [u32; 2],
    /// The chunk text.
    pub content: String,
    /// Cosine similarity in `[-1, 1]` (higher = closer). **Always the true
    /// cosine** — even under RRF fusion, so the GUI/IPC contract (`score` is the
    /// dense relevance) never changes (lexical-bm25 plan §1.5).
    pub score: f64,
    /// 0-based chunk index within its file (disambiguates same-file hits).
    pub chunk_idx: u32,
    /// The BM25 lexical score, when the lexical arm matched this chunk under RRF
    /// fusion. `None` when fusion is OFF or the lexical arm didn't match — so a
    /// lexical-only hit (low cosine, high BM25) isn't mistaken for a weak one.
    /// Additive provenance; never affects `score`. Omitted from `to_value` when
    /// `None` (so today's JSON is unchanged with fusion OFF).
    pub lexical_score: Option<f64>,
    /// The RRF fused score that drove ordering/cutoff under fusion. `None` when
    /// fusion is OFF (ordering is pure cosine). Additive provenance, omitted from
    /// `to_value` when `None`.
    pub fused_score: Option<f64>,
}

impl SearchHit {
    /// JSON shape handed to the IPC layer / GUI. The two provenance keys are
    /// **additive** — present only under fusion when set — so existing consumers
    /// see a JSON object identical to today's when fusion is OFF.
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "file_path": self.file_path,
            "line_range": [self.line_range[0], self.line_range[1]],
            "content": self.content,
            "score": self.score,
            "chunk_idx": self.chunk_idx,
        });
        if let Some(lex) = self.lexical_score {
            v["lexical_score"] = json!(lex);
        }
        if let Some(fused) = self.fused_score {
            v["fused_score"] = json!(fused);
        }
        v
    }
}

/// Top-`k` chunks for `query` within `workspace_id`, highest score first.
///
/// Returns an empty vec when the workspace has no index, the query is
/// blank, or the embedder is unreachable — the caller treats `[]` as "no
/// snippets", never an error.
pub async fn query(workspace_id: &str, query_text: &str, k: usize) -> Vec<SearchHit> {
    if query_text.trim().is_empty() || k == 0 {
        return Vec::new();
    }
    let Some(query_vec) = embed_query(workspace_id, query_text).await else {
        return Vec::new();
    };
    query_with_vec(workspace_id, &query_vec, query_text, k)
}

/// Embed `query_text` for `workspace_id`, returning `None` (never an error) for
/// a blank query, an empty embedding, or an unreachable embedder — the same
/// fail-soft contract [`query`] has always had.
///
/// Split out so a caller that needs the *vector itself* (concept routing —
/// concept-routing plan §6.1 "no extra round-trip") can embed **once** and feed
/// it to both [`query_with_vec`] and the router, instead of paying a second
/// embed. The [`query`] path is unchanged: same text in, same vector, same
/// hits out.
pub async fn embed_query(workspace_id: &str, query_text: &str) -> Option<Vec<f32>> {
    if query_text.trim().is_empty() {
        return None;
    }
    match crate::embeddings::embed_one(query_text.to_owned()).await {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("workspaces.rag: query embed failed for {workspace_id}: {e}");
            None
        }
    }
}

/// [`query`] with a pre-computed `query_vec` — skips the embed. `query_text` is
/// still passed so the `[anchors: …]` / `[active_file: …]` markers can be
/// extracted (they ride in the text, not the vector). Behaviour is identical to
/// [`query`] for the same `(query_vec, query_text)`; the only difference is who
/// paid for the embed.
pub fn query_with_vec(
    workspace_id: &str,
    query_vec: &[f32],
    query_text: &str,
    k: usize,
) -> Vec<SearchHit> {
    if k == 0 || query_vec.is_empty() {
        return Vec::new();
    }
    let chunks = store::load_chunks(workspace_id);
    if chunks.is_empty() {
        return Vec::new();
    }
    // 2.4 (anchor-biased retrieval): the harness folds the turn's already-
    // resolved anchor/symbol identifiers into the query behind a marker (see
    // [`extract_anchor_terms`]); chunks whose path/body literally contain one
    // get a scoring boost so the deterministic anchor layer and the fuzzy
    // cosine layer agree. The same `query_text` is still embedded whole, so the
    // anchor names also bias the embedding (query expansion).
    let anchors = extract_anchor_terms(query_text);
    // 2.5 (active-file boost): the harness folds the editor's open file behind
    // the `[active_file: …]` marker; chunks from that file (or its directory)
    // get a scoring boost so a generic question while a file is open biases
    // toward the user's current focus, without partitioning the index.
    let active_file = extract_active_file(query_text);

    // Lexical/BM25 + RRF fusion is OFF by default ⇒ today's dense-only path,
    // byte-for-byte (the identity guarantee, lexical-bm25 plan §1.3). Only an
    // explicit, persisted opt-in enters the fused path below.
    let cfg = crate::rag::LexicalConfig::current();
    if !cfg.enabled {
        return rank_with(query_vec, chunks, k, &anchors, active_file.as_deref());
    }
    rank_fused(
        workspace_id,
        query_vec,
        query_text,
        chunks,
        k,
        &anchors,
        active_file.as_deref(),
        &cfg,
    )
}

/// MMR relevance/diversity trade-off (the `λ` in the standard formula):
/// `λ·rel − (1−λ)·max_sim_to_selected`. At `0.7` query-relevance stays
/// dominant while near-duplicate chunks are still penalised out of the top-k.
const MMR_LAMBDA: f64 = 0.7;

/// Over-fetch depth: how many of the strongest cosine hits MMR considers
/// before selecting the final `k`. Larger gives MMR more redundant
/// neighbours to prune; capped to keep the O(pool·k) similarity work cheap
/// on large indexes. (`pool` is always at least `k`.)
const MMR_POOL: usize = 20;

/// Absolute cosine noise floor for the dynamic-k cutoff. A query whose
/// *best* hit scores below this retrieves nothing — the workspace index
/// holds nothing on-topic, so injecting its strongest-but-still-weak chunks
/// only pads the prompt with noise.
///
/// **Empirically calibrated against the live index** (14k chunks,
/// `nomic-embed-text`, no task-prefixing → anisotropic cosines with a high
/// baseline). Measured top-1 scores cleanly separate by relevance: genuinely
/// off-topic queries (pizza recipe, dog training, weather) top out at
/// ~0.49–0.51, while on-topic queries — even a vague "why did that happen?"
/// — sit at ~0.60–0.69. `0.55` lands in that gap, so off-topic queries
/// inject nothing while on-topic queries keep their hits. (A query the model
/// itself can't tell from on-topic — e.g. song lyrics scoring ~0.59 — is a
/// limit of the embedding, not of this cutoff.) Re-measure if the embedding
/// model or task-prefixing changes.
const MIN_ABSOLUTE_SCORE: f64 = 0.55;

/// Relative dominance floor for the dynamic-k cutoff: a hit is only worth a
/// prompt slot if its cosine is at least this fraction of the *top* hit's.
/// When one result dominates (a sharp cliff after rank 1) the weaker tail is
/// trimmed instead of padding the slot; when several hits cluster near the
/// top they all clear it and the full budget is used.
const RELATIVE_FLOOR: f64 = 0.6;

/// Marker the harness wraps the turn's resolved anchor/symbol identifiers in
/// when it appends them to the retrieval query (2.4). It mirrors the format
/// produced by `wylde-harness/.../turn/context_gather.rs::compose_retrieval_query`
/// — **keep the two in sync**; the integration is covered by the live-index
/// real-path check, not by the type system (cross-crate string protocol).
/// Form: `[anchors: term1 term2 ...]`.
const ANCHOR_QUERY_MARKER: &str = "[anchors:";

/// Score lift for a chunk whose **path** contains a resolved anchor term — the
/// strongest signal (the symbol's defining file), so "ask about a known symbol
/// → its defining file ranks top". Additive on the cosine, capped below.
const ANCHOR_PATH_BOOST: f64 = 0.18;

/// Score lift for a chunk whose **body** mentions a resolved anchor term — a
/// weaker signal than a path hit, so it's smaller.
const ANCHOR_BODY_BOOST: f64 = 0.08;

/// Ceiling on the total anchor boost a single chunk can accrue, so the bias
/// re-ranks *within* the relevant pool without ever dwarfing cosine (a chunk
/// can't be dragged from noise to top on lexical hits alone).
const ANCHOR_BOOST_CAP: f64 = 0.30;

/// Minimum length of an anchor term used for the lexical boost. Short
/// fragments make `contains` substring matches too promiscuous (e.g. `add`
/// inside `address`); resolved symbol identifiers clearing this are
/// distinctive enough for a substring hit to be meaningful.
const ANCHOR_TERM_MIN_LEN: usize = 4;

/// Pull the resolved anchor/symbol terms out of a retrieval query the harness
/// augmented (2.4). The harness appends them as `[anchors: term1 term2 ...]`
/// (see [`ANCHOR_QUERY_MARKER`]); we read everything between that marker and
/// the closing `]`, lowercased, deduped, and length-filtered. A query with no
/// marker (a plain turn, or a non-harness caller like `workspaces.rag_query`)
/// yields no terms — the boost is then a no-op and ranking is pure cosine.
fn extract_anchor_terms(query_text: &str) -> Vec<String> {
    let Some(start) = query_text.find(ANCHOR_QUERY_MARKER) else {
        return Vec::new();
    };
    let after = &query_text[start + ANCHOR_QUERY_MARKER.len()..];
    let body = match after.find(']') {
        Some(end) => &after[..end],
        None => after, // tolerate a missing close bracket — take the tail
    };
    let mut terms: Vec<String> = Vec::new();
    for raw in body.split_whitespace() {
        let term = raw.to_ascii_lowercase();
        if term.len() >= ANCHOR_TERM_MIN_LEN && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// Lexical anchor boost for one chunk (2.4): additive lift if its path or body
/// contains a resolved anchor term. A path hit (likely the symbol's defining
/// file) weighs more than a body mention; the total is capped at
/// [`ANCHOR_BOOST_CAP`] so cosine relevance stays dominant. Terms are already
/// lowercased by [`extract_anchor_terms`]; we lowercase the chunk once.
fn anchor_boost(chunk: &IndexedChunk, anchors: &[String]) -> f64 {
    if anchors.is_empty() {
        return 0.0;
    }
    let path = chunk.path.to_ascii_lowercase();
    let body = chunk.content.to_ascii_lowercase();
    let mut boost = 0.0_f64;
    for term in anchors {
        if path.contains(term.as_str()) {
            boost += ANCHOR_PATH_BOOST;
        } else if body.contains(term.as_str()) {
            boost += ANCHOR_BODY_BOOST;
        }
        if boost >= ANCHOR_BOOST_CAP {
            return ANCHOR_BOOST_CAP;
        }
    }
    boost.min(ANCHOR_BOOST_CAP)
}

/// Marker the harness wraps the editor's open file in when it appends it to the
/// retrieval query (2.5). Mirrors the format produced by
/// `wylde-harness/.../turn/context_gather.rs::gather_with` (the `[active_file:
/// …]` append) — **keep the two in sync**; like the anchor marker it is a
/// cross-crate string protocol, covered by the live-index real-path check.
/// Form: `[active_file: workspace/relative/path.rs]`.
const ACTIVE_FILE_QUERY_MARKER: &str = "[active_file:";

/// Score lift for a chunk from the **exact file** open in the Workspaces editor
/// (2.5) — the strongest current-focus signal. Additive on the cosine, smaller
/// than [`ANCHOR_PATH_BOOST`] so an explicitly-referenced symbol still outranks
/// "merely the file I'm looking at".
const ACTIVE_FILE_PATH_BOOST: f64 = 0.15;

/// Score lift for a chunk that merely **shares the directory** of the open file
/// — a weaker "same area / same service" signal, so a generic question while
/// `services/x/foo.rs` is open nudges the rest of `services/x` up too.
const ACTIVE_FILE_DIR_BOOST: f64 = 0.06;

/// Normalise a path for cross-platform comparison: trim, fold `\` to `/`, and
/// lowercase. Both the marker payload and the chunk path run through this so a
/// Windows-relative `services\x\foo.rs` matches an index that stored `/`.
fn normalize_path(p: &str) -> String {
    p.trim().replace('\\', "/").to_ascii_lowercase()
}

/// The directory portion of a `/`-separated, already-[`normalize_path`]d path
/// (everything before the last `/`), or `None` for a root-level file with no
/// directory — a root file must never dir-match every other root file.
fn dir_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// Pull the editor's open file out of a retrieval query the harness augmented
/// (2.5), already [`normalize_path`]d. A query with no marker (a plain turn, or
/// a non-harness caller) yields `None` — the boost is then a no-op.
fn extract_active_file(query_text: &str) -> Option<String> {
    let start = query_text.find(ACTIVE_FILE_QUERY_MARKER)?;
    let after = &query_text[start + ACTIVE_FILE_QUERY_MARKER.len()..];
    let body = match after.find(']') {
        Some(end) => &after[..end],
        None => after, // tolerate a missing close bracket — take the tail
    };
    let path = normalize_path(body);
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Active-file boost for one chunk (2.5): the exact open file gets
/// [`ACTIVE_FILE_PATH_BOOST`], a sibling in the same directory gets the smaller
/// [`ACTIVE_FILE_DIR_BOOST`], everything else nothing. `active_file` is already
/// normalised by [`extract_active_file`]; the chunk path is normalised here.
fn active_file_boost(chunk: &IndexedChunk, active_file: Option<&str>) -> f64 {
    let Some(active) = active_file else {
        return 0.0;
    };
    let path = normalize_path(&chunk.path);
    if path == active {
        return ACTIVE_FILE_PATH_BOOST;
    }
    match (dir_of(active), dir_of(&path)) {
        (Some(ad), Some(pd)) if !ad.is_empty() && ad == pd => ACTIVE_FILE_DIR_BOOST,
        _ => 0.0,
    }
}

/// Ranking core: score every chunk by cosine against `query_vec`, then
/// select `k` with Maximal Marginal Relevance so near-duplicate chunks
/// don't crowd out the prompt's RAG slot. Split out for direct unit testing
/// without a live embedder.
///
/// The pure-cosine scoring is unchanged — MMR only governs *selection*
/// among the top [`MMR_POOL`] candidates, and each returned hit's `score`
/// is still its true cosine relevance.
///
/// `k` is the *budget* (max slots), not a fixed count: a [`dynamic_k`] cutoff
/// trims weak/dominated hits first, so an off-topic query returns few or no
/// chunks instead of padding the slot up to `k`.
///
/// `anchors` are the turn's resolved anchor/symbol terms (2.4): a chunk whose
/// path/body contains one gets an [`anchor_boost`] added to its cosine to form
/// an *effective* score that drives ordering, the dynamic-k cutoff, and MMR
/// selection — so a chunk literally about a referenced symbol survives the
/// cutoff and ranks up. The **reported** [`SearchHit::score`] stays the true
/// cosine relevance; the boost only governs selection. An empty `anchors` is a
/// no-op (effective ≡ cosine), preserving the pre-2.4 behaviour exactly.
pub fn rank(
    query_vec: &[f32],
    chunks: Vec<IndexedChunk>,
    k: usize,
    anchors: &[String],
) -> Vec<SearchHit> {
    rank_with(query_vec, chunks, k, anchors, None)
}

/// [`rank`] plus the 2.5 active-file boost: `active_file` is the editor's open
/// file (workspace-relative, already normalised by [`extract_active_file`]). A
/// chunk from that file (or its directory) gets an [`active_file_boost`] added
/// to its effective score alongside the 2.4 anchor boost, so it survives the
/// dynamic-k cutoff and ranks up — the **reported** [`SearchHit::score`] stays
/// the true cosine. `None` is a no-op (identical to [`rank`]).
pub fn rank_with(
    query_vec: &[f32],
    chunks: Vec<IndexedChunk>,
    k: usize,
    anchors: &[String],
    active_file: Option<&str>,
) -> Vec<SearchHit> {
    if k == 0 {
        return Vec::new();
    }
    // `(effective, cosine, chunk)` — `effective = cosine + anchor_boost +
    // active_file_boost` governs ranking/cutoff/MMR; `cosine` is what each hit
    // reports.
    let mut scored: Vec<(f64, f64, IndexedChunk)> = chunks
        .into_iter()
        .map(|c| {
            let cos = cosine(query_vec, &c.vector);
            let eff = (cos + anchor_boost(&c, anchors) + active_file_boost(&c, active_file)).min(1.0);
            (eff, cos, c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    // Vary how many slots are actually warranted by the score distribution
    // before spending the MMR/diversity budget. Nothing clears the cutoff →
    // inject nothing (the off-topic case).
    let keep = dynamic_k(&scored, k);
    if keep == 0 {
        return Vec::new();
    }
    // Over-fetch a pool of the strongest hits, then MMR-select down to the
    // warranted count. The pool floor of `keep` keeps behaviour intact when
    // `keep` exceeds it.
    scored.truncate(MMR_POOL.max(keep));
    mmr_select(scored, keep, |t| t.0, |t| &t.2.vector)
        .into_iter()
        .map(|(_, cos, c)| SearchHit {
            file_path: c.path,
            line_range: [c.start_line, c.end_line],
            content: c.content,
            score: cos,
            chunk_idx: c.chunk_idx,
            lexical_score: None,
            fused_score: None,
        })
        .collect()
}

/// How many lexical (BM25) hits to fetch for fusion. Beyond this depth the
/// lexical RRF contribution (`w/(rrf_k+rank)`) is negligible vs the dense arm, so
/// fetching deeper can't change the fused top-k; it just over-fetches. Always at
/// least the caller's `k`.
const LEXICAL_FETCH: usize = 50;

/// Ranking core for the **fused** path (toggle ON, lexical-bm25 plan §1.3). Runs
/// the dense (cosine) and lexical (BM25) arms over the same chunk set, fuses them
/// with RRF ([`fuse::fuse`]), then drives the existing dynamic-k / MMR levers off
/// the fused score. The reported [`SearchHit::score`] is **still the true
/// cosine**; the RRF score and any BM25 hit ride in the additive provenance
/// fields so a lexical-only hit isn't mistaken for a weak one.
#[allow(clippy::too_many_arguments)] // the fused retrieval entry: ws + vec + text + chunks + budget + levers + cfg
fn rank_fused(
    workspace_id: &str,
    query_vec: &[f32],
    query_text: &str,
    chunks: Vec<IndexedChunk>,
    k: usize,
    anchors: &[String],
    active_file: Option<&str>,
    cfg: &LexicalConfig,
) -> Vec<SearchHit> {
    if k == 0 || chunks.is_empty() {
        return Vec::new();
    }
    // One-time backfill (§2.5): if the toggle was just flipped on and the lexical
    // index doesn't exist yet, build it from the persisted chunks (no embedder).
    // Best-effort — if it's still absent the lexical arm returns nothing and
    // fusion degrades to dense-only ranking.
    super::ensure_lexical_backfill(workspace_id);

    let n = chunks.len();

    // ── DENSE arm: cosine per chunk + a full descending-cosine ordering. ──
    let cosines: Vec<f64> = chunks.iter().map(|c| cosine(query_vec, &c.vector)).collect();
    let mut dense_order: Vec<usize> = (0..n).collect();
    dense_order.sort_by(|&a, &b| {
        cosines[b]
            .partial_cmp(&cosines[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // ── LEXICAL arm: BM25 over the tantivy index, joined back to chunk indices
    // by chunk_id. The cleaned user text contributes baseline body terms; the
    // resolved anchor terms are folded in as a boosted exact-token sub-query
    // (L5 — IDF-weighted, exact boundaries, retiring the substring hack). A hit
    // with no matching loaded chunk is silently dropped (§2.6 fail-soft) —
    // lexical can never surface a chunk the vector store lacks. ──
    let lex_query = build_lexical_query(query_text);
    let lex_raw = lexical::search_boosted(workspace_id, &lex_query, anchors, LEXICAL_FETCH.max(k));
    let id_to_idx: std::collections::HashMap<&str, usize> = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    let lex_hits: Vec<(usize, f64)> = lex_raw
        .iter()
        .filter_map(|(id, s)| id_to_idx.get(id.as_str()).map(|&i| (i, *s)))
        .collect();

    // ── RRF fuse → (fused, lexical_opt) per chunk index. ──
    let fused = fuse::fuse(n, &dense_order, &lex_hits, cfg);

    // Scored candidates: (effective_fused, cosine, lexical_opt, chunk). The
    // active-file boost is an additive FOCUS lift on the fused score (§3) — at
    // the RRF scale so it nudges the open file up without dwarfing a genuine
    // two-arm hit, kept separate from the lexical relevance arm.
    let mut scored: Vec<(f64, f64, Option<f64>, IndexedChunk)> = chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let focus = active_file_focus_boost(&c, active_file, cfg);
            (fused[i].0 + focus, cosines[i], fused[i].1, c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let keep = dynamic_k_fused(&scored, k, cfg);
    if keep == 0 {
        return Vec::new();
    }
    scored.truncate(MMR_POOL.max(keep));
    mmr_select(scored, keep, |t| t.0, |t| &t.3.vector)
        .into_iter()
        .map(|(fused_eff, cos, lex, c)| SearchHit {
            file_path: c.path,
            line_range: [c.start_line, c.end_line],
            content: c.content,
            score: cos, // STILL true cosine (the §1.5 contract)
            chunk_idx: c.chunk_idx,
            lexical_score: lex,
            fused_score: Some(fused_eff),
        })
        .collect()
}

/// The lexical-arm query text: strip the protocol markers the harness appends
/// (`[anchors: …]` / `[active_file: …]`) so their payloads don't pollute the BM25
/// body query, leaving the user message. The resolved anchor terms are folded
/// back in separately, with a boost, by [`lexical::search_boosted`] (L5).
fn build_lexical_query(query_text: &str) -> String {
    let cut = [ANCHOR_QUERY_MARKER, ACTIVE_FILE_QUERY_MARKER]
        .iter()
        .filter_map(|m| query_text.find(m))
        .min();
    match cut {
        Some(i) => query_text[..i].trim().to_owned(),
        None => query_text.trim().to_owned(),
    }
}

/// Active-file FOCUS boost at the **RRF scale** (fused path, §3): the exact open
/// file gets [`LexicalConfig::active_file_focus_boost`], a sibling in the same
/// directory the smaller dir boost, everything else nothing. Kept additive and
/// post-RRF — a positional/focus signal, deliberately separate from the lexical
/// relevance arm. `active_file` is already normalised by [`extract_active_file`].
fn active_file_focus_boost(
    chunk: &IndexedChunk,
    active_file: Option<&str>,
    cfg: &LexicalConfig,
) -> f64 {
    let Some(active) = active_file else {
        return 0.0;
    };
    let path = normalize_path(&chunk.path);
    if path == active {
        return cfg.active_file_focus_boost;
    }
    match (dir_of(active), dir_of(&path)) {
        (Some(ad), Some(pd)) if !ad.is_empty() && ad == pd => cfg.active_file_dir_focus_boost,
        _ => 0.0,
    }
}

/// Fused dynamic-k cutoff (§1.4) — the one real friction RRF introduces. Returns
/// a count in `0..=k`:
///
/// * `0` — the **top** candidate is on-topic to *neither* signal (dense cosine
///   below [`MIN_ABSOLUTE_SCORE`] **and** no lexical hit clearing
///   [`LexicalConfig::min_bm25`]): inject nothing, exactly as the dense floor
///   does for an off-topic query.
/// * else — the fused-sorted prefix that clears
///   [`LexicalConfig::fused_relative_floor`] · `top_fused`, capped at `k`.
///
/// The crucial difference from [`dynamic_k`]: the absolute *cosine* floor is
/// **not** applied to a fused score (RRF scores are scale-free); its *purpose* —
/// "off-topic injects nothing" — is preserved by the on-topic gate, which a
/// strong exact-token BM25 hit at low cosine **passes** (the approved bypass,
/// confirmed by Aaron). That bypass is the recall win.
fn dynamic_k_fused(
    scored: &[(f64, f64, Option<f64>, IndexedChunk)],
    k: usize,
    cfg: &LexicalConfig,
) -> usize {
    if k == 0 || scored.is_empty() {
        return 0;
    }
    let (top_fused, top_cos, top_lex) = (scored[0].0, scored[0].1, scored[0].2);
    // On-topic gate on the TOP candidate: cleared either floor → on-topic.
    let on_topic =
        top_cos >= MIN_ABSOLUTE_SCORE || top_lex.map(|bm| bm >= cfg.min_bm25).unwrap_or(false);
    if !on_topic || top_fused <= 0.0 {
        return 0;
    }
    let threshold = cfg.fused_relative_floor * top_fused;
    let kept = scored
        .iter()
        .take(k)
        .take_while(|c| c.0 >= threshold)
        .count();
    kept.max(1)
}

/// Decide how many of the top hits are *worth* a prompt slot, given the
/// budget `k` and the descending-cosine `scored` candidates. Returns a count
/// in `0..=k`:
///
/// * `0` — the best hit is below [`MIN_ABSOLUTE_SCORE`]: nothing on-topic, so
///   inject nothing rather than padding with noise.
/// * `1` — one result dominates (the rest fall below [`RELATIVE_FLOOR`]·top):
///   don't dilute it with weak tail hits.
/// * up to `k` — several hits cluster near the top: use the full budget.
///
/// Because `scored` is sorted descending, the kept hits are the contiguous
/// prefix that clears `max(MIN_ABSOLUTE_SCORE, RELATIVE_FLOOR·top)`. Operates
/// on the *effective* score (`.0`, cosine + any 2.4 anchor boost), so an
/// anchor-matched chunk with a modest cosine can still clear the cutoff.
fn dynamic_k(scored: &[(f64, f64, IndexedChunk)], k: usize) -> usize {
    if k == 0 || scored.is_empty() {
        return 0;
    }
    let top = scored[0].0;
    // Best hit is noise → retrieve nothing.
    if top < MIN_ABSOLUTE_SCORE {
        return 0;
    }
    let threshold = MIN_ABSOLUTE_SCORE.max(RELATIVE_FLOOR * top);
    let kept = scored
        .iter()
        .take(k)
        .take_while(|(score, _, _)| *score >= threshold)
        .count();
    // `top` cleared `threshold` by construction, so at least the dominant
    // hit is always kept once we get here.
    kept.max(1)
}

/// Greedy Maximal Marginal Relevance selection over `candidates`
/// (pre-sorted by descending cosine relevance). Picks the highest-relevance
/// chunk first, then repeatedly the chunk maximising
/// `λ·rel − (1−λ)·max cosine to anything already picked`, so a chunk nearly
/// identical to one already chosen is demoted in favour of a fresh-but-still-
/// relevant one. Returns at most `k` items in selection order.
fn mmr_select<T>(
    mut candidates: Vec<T>,
    k: usize,
    rel: impl Fn(&T) -> f64,
    vec_of: impl Fn(&T) -> &[f32],
) -> Vec<T> {
    let target = k.min(candidates.len());
    if target == 0 {
        return Vec::new();
    }
    let mut selected: Vec<T> = Vec::with_capacity(target);
    // Seed with the top hit — candidates is already sorted descending by the
    // effective relevance score (cosine + anchor-boost for the dense path, the
    // fused RRF score for the fusion path).
    selected.push(candidates.remove(0));
    while selected.len() < target && !candidates.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f64::NEG_INFINITY;
        for (i, cand) in candidates.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|s| cosine(vec_of(cand), vec_of(s)))
                .fold(0.0_f64, f64::max);
            let mmr = MMR_LAMBDA * rel(cand) - (1.0 - MMR_LAMBDA) * max_sim;
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = i;
            }
        }
        selected.push(candidates.remove(best_idx));
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(path: &str, vector: Vec<f32>, content: &str) -> IndexedChunk {
        IndexedChunk {
            id: format!("{path}-0"),
            path: path.to_owned(),
            chunk_idx: 0,
            content: content.to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 4,
            vector,
        }
    }

    #[test]
    fn rank_orders_by_cosine_and_truncates_to_k() {
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = vec![
            chunk("/far.md", vec![0.0, 1.0, 0.0], "far"), // orthogonal → 0
            chunk("/near.md", vec![0.9, 0.1, 0.0], "near"), // close → high
            chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"), // middling
        ];
        let hits = rank(&query, chunks, 2, &[]);
        assert_eq!(hits.len(), 2, "truncated to k");
        assert_eq!(hits[0].file_path, "/near.md", "nearest first");
        assert_eq!(hits[1].file_path, "/mid.md");
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[0].line_range, [1, 4]);
    }

    #[test]
    fn rank_empty_chunks_is_empty() {
        assert!(rank(&[1.0, 0.0], Vec::new(), 5, &[]).is_empty());
    }

    #[test]
    fn rank_k_zero_is_empty() {
        let chunks = vec![chunk("/a.md", vec![1.0, 0.0], "a")];
        assert!(rank(&[1.0, 0.0], chunks, 0, &[]).is_empty());
    }

    #[test]
    fn rank_mmr_drops_near_duplicate_for_diverse_chunk() {
        // Query along the first axis. Two chunks are identical (a perfect
        // near-duplicate pair) and a third is *equally* relevant to the query
        // but points in a different residual direction. Pure top-k cosine
        // would return both duplicates; MMR must swap the second duplicate
        // out for the distinct chunk.
        let query = vec![1.0_f32, 0.0, 0.0, 0.0];
        let chunks = vec![
            chunk("/a.md", vec![0.8, 0.6, 0.0, 0.0], "first"),
            chunk("/a-dup.md", vec![0.8, 0.6, 0.0, 0.0], "near-duplicate of first"),
            chunk("/b.md", vec![0.8, 0.0, 0.6, 0.0], "equally relevant but distinct"),
        ];
        let hits = rank(&query, chunks, 2, &[]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "/a.md", "top relevance kept first");
        assert_eq!(
            hits[1].file_path, "/b.md",
            "near-duplicate demoted; diverse chunk selected"
        );
        // Returned score is still the true cosine relevance (all three ≈0.8;
        // tolerance accounts for the f32 vector components).
        assert!((hits[0].score - 0.8).abs() < 1e-6);
    }

    #[test]
    fn rank_mmr_keeps_relevance_dominant() {
        // A clearly irrelevant chunk must never beat a relevant one on
        // diversity alone — λ=0.7 keeps relevance dominant.
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = vec![
            chunk("/near.md", vec![0.9, 0.1, 0.0], "near"),
            chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"),
            chunk("/orthogonal.md", vec![0.0, 1.0, 0.0], "irrelevant"),
        ];
        let hits = rank(&query, chunks, 2, &[]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].file_path, "/near.md");
        assert_eq!(hits[1].file_path, "/mid.md", "relevant mid beats orthogonal");
    }

    /// Build descending `(effective, cosine, chunk)` triples for direct
    /// [`dynamic_k`] boundary tests, without an embedder. With no anchor boost
    /// the effective score equals the cosine, so each input `s` fills both.
    fn scored(scores: &[f64]) -> Vec<(f64, f64, IndexedChunk)> {
        scores
            .iter()
            .enumerate()
            .map(|(i, &s)| (s, s, chunk(&format!("/c{i}.md"), vec![1.0, 0.0], "c")))
            .collect()
    }

    /// The same cutoff threshold `dynamic_k` applies for a given top score —
    /// so these tests pin the *logic*, not the calibrated constants, and stay
    /// green if `MIN_ABSOLUTE_SCORE` / `RELATIVE_FLOOR` are re-tuned.
    fn threshold_for(top: f64) -> f64 {
        MIN_ABSOLUTE_SCORE.max(RELATIVE_FLOOR * top)
    }

    #[test]
    fn dynamic_k_zero_when_best_hit_is_noise() {
        // Top hit below the absolute floor → nothing on-topic, inject none.
        let noise = MIN_ABSOLUTE_SCORE - 0.05;
        assert_eq!(dynamic_k(&scored(&[noise, noise - 0.05, noise - 0.1]), 5), 0);
    }

    #[test]
    fn dynamic_k_one_when_top_dominates_and_tail_is_below_floor() {
        // A strong top hit, with the tail below the cutoff → only the
        // dominant hit is kept (don't dilute it with weak padding).
        let top = 0.9_f64;
        let thr = threshold_for(top);
        assert_eq!(dynamic_k(&scored(&[top, thr - 0.05, thr - 0.1]), 5), 1);
    }

    #[test]
    fn dynamic_k_keeps_cluster_above_the_floor() {
        // Three hits all comfortably above the cutoff → full count kept.
        let top = 0.9_f64;
        let thr = threshold_for(top);
        assert_eq!(dynamic_k(&scored(&[top, thr + 0.05, thr + 0.02]), 5), 3);
    }

    #[test]
    fn dynamic_k_capped_by_budget() {
        let top = 0.9_f64;
        assert_eq!(dynamic_k(&scored(&[top, top, top, top]), 2), 2);
    }

    #[test]
    fn dynamic_k_relative_floor_trims_when_top_is_very_high() {
        // When `RELATIVE_FLOOR·top` exceeds the absolute floor, a tail hit
        // that clears the absolute floor but is dominated by a very strong
        // top is still trimmed — the dominance branch.
        let top = 0.99_f64;
        let rel = RELATIVE_FLOOR * top;
        // Only meaningful while the relative floor is the binding one.
        if rel > MIN_ABSOLUTE_SCORE {
            let tail = (rel + MIN_ABSOLUTE_SCORE) / 2.0; // above absolute, below relative
            assert_eq!(dynamic_k(&scored(&[top, tail]), 5), 1);
        }
    }

    #[test]
    fn dynamic_k_zero_budget_or_empty() {
        assert_eq!(dynamic_k(&scored(&[0.9]), 0), 0);
        assert_eq!(dynamic_k(&[], 5), 0);
    }

    /// Unit vector whose cosine against the `[1, 0]` query equals `c`.
    fn vec_with_cosine(c: f32) -> Vec<f32> {
        vec![c, (1.0 - c * c).max(0.0).sqrt()]
    }

    #[test]
    fn rank_dynamic_k_trims_weak_tail_when_top_dominates() {
        // Budget of 5, but only the strong hit warrants a slot: the two weak
        // tail hits fall below the cutoff and are dropped, not padded in.
        let top = 0.9_f64;
        let weak = (threshold_for(top) - 0.1) as f32;
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/strong.md", vec_with_cosine(top as f32), "strong"),
            chunk("/weak-a.md", vec_with_cosine(weak), "weak a"),
            chunk("/weak-b.md", vec_with_cosine(weak), "weak b"),
        ];
        let hits = rank(&query, chunks, 5, &[]);
        assert_eq!(hits.len(), 1, "weak tail trimmed, not padded to budget");
        assert_eq!(hits[0].file_path, "/strong.md");
    }

    #[test]
    fn rank_dynamic_k_empty_for_off_topic_query() {
        // Every hit is below the absolute floor: an off-topic query injects
        // nothing instead of padding the slot to k.
        let noise = (MIN_ABSOLUTE_SCORE - 0.1) as f32;
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/a.md", vec_with_cosine(noise), "a"),
            chunk("/b.md", vec_with_cosine(noise - 0.05), "b"),
            chunk("/c.md", vec_with_cosine(noise - 0.1), "c"),
        ];
        assert!(rank(&query, chunks, 5, &[]).is_empty());
    }

    #[test]
    fn rank_dynamic_k_uses_budget_when_hits_cluster() {
        // Three hits clustered above the floor all warrant a slot → all three
        // returned (the full available budget), none trimmed.
        let top = 0.9_f64;
        let thr = threshold_for(top);
        let query = vec![1.0_f32, 0.0];
        let chunks = vec![
            chunk("/a.md", vec_with_cosine(top as f32), "a"),
            chunk("/b.md", vec_with_cosine((thr + 0.08) as f32), "b"),
            chunk("/c.md", vec_with_cosine((thr + 0.04) as f32), "c"),
        ];
        let hits = rank(&query, chunks, 5, &[]);
        assert_eq!(hits.len(), 3, "clustered hits all warrant a slot");
    }

    // ── 2.4: anchor-biased retrieval ────────────────────────────────────

    #[test]
    fn extract_anchor_terms_reads_the_marker_section() {
        let q = "why does it fail?\n\n[conversation context: eviction ladder]\n\n\
                 [anchors: compose_retrieval_query GatherWith run_it]";
        let terms = extract_anchor_terms(q);
        // Lowercased, length-filtered, order-preserving, deduped.
        assert_eq!(terms, vec!["compose_retrieval_query", "gatherwith", "run_it"]);
    }

    #[test]
    fn extract_anchor_terms_empty_without_marker() {
        // A plain query (or the rag_query verb path) has no marker → no terms,
        // so the boost is a no-op and ranking stays pure cosine.
        assert!(extract_anchor_terms("just a normal question about search").is_empty());
        // Short fragments inside the marker are dropped (substring-promiscuous).
        assert!(extract_anchor_terms("[anchors: a bc xy]").is_empty());
    }

    #[test]
    fn anchor_boost_weights_path_over_body_and_caps() {
        let path_hit = chunk("/src/compose_retrieval_query.rs", vec![1.0, 0.0], "fn body");
        let body_hit = chunk("/src/other.rs", vec![1.0, 0.0], "calls compose_retrieval_query here");
        let miss = chunk("/src/unrelated.rs", vec![1.0, 0.0], "nothing relevant");
        let anchors = vec!["compose_retrieval_query".to_owned()];
        assert!((anchor_boost(&path_hit, &anchors) - ANCHOR_PATH_BOOST).abs() < 1e-9);
        assert!((anchor_boost(&body_hit, &anchors) - ANCHOR_BODY_BOOST).abs() < 1e-9);
        assert_eq!(anchor_boost(&miss, &anchors), 0.0);
        assert_eq!(anchor_boost(&path_hit, &[]), 0.0, "no anchors → no boost");
        // Many path hits saturate at the cap.
        let many: Vec<String> = (0..10).map(|_| "compose_retrieval_query".to_owned()).collect();
        assert_eq!(anchor_boost(&path_hit, &many), ANCHOR_BOOST_CAP);
    }

    #[test]
    fn rank_anchor_boost_promotes_the_defining_file_over_a_higher_cosine_chunk() {
        // The defining file has a *lower* cosine than a rival chunk, but its
        // path carries the resolved symbol — the anchor boost must lift it to
        // the top. The reported score, however, stays the true cosine.
        let query = vec![1.0_f32, 0.0];
        let defining = {
            let mut c = chunk("/src/run_it_handler.rs", vec_with_cosine(0.60), "def");
            c.content = "fn run_it_handler() {}".into();
            c
        };
        let rival = chunk("/src/notes.md", vec_with_cosine(0.70), "loosely related prose");
        let anchors = vec!["run_it_handler".to_owned()];
        let hits = rank(&query, vec![rival.clone(), defining.clone()], 2, &anchors);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].file_path, "/src/run_it_handler.rs",
            "anchor boost (0.60 + path 0.18 = 0.78) beats the rival's 0.70"
        );
        // Reported score is the true cosine, not the boosted effective score.
        assert!(
            (hits[0].score - 0.60).abs() < 1e-3,
            "reported score stays cosine, got {}",
            hits[0].score
        );
    }

    #[test]
    fn rank_anchor_match_survives_the_dynamic_k_cutoff() {
        // A chunk whose cosine alone sits just below the absolute noise floor
        // (so dynamic-k would drop it) is rescued because its path carries the
        // resolved symbol — the effective score clears the floor.
        let query = vec![1.0_f32, 0.0];
        let just_below = (MIN_ABSOLUTE_SCORE - 0.05) as f32;
        let defining = chunk("/src/run_it_handler.rs", vec_with_cosine(just_below), "x");
        let anchors = vec!["run_it_handler".to_owned()];
        // Without the anchor boost this query injects nothing.
        assert!(rank(&query, vec![defining.clone()], 5, &[]).is_empty());
        // With it, the anchor-matched chunk is retrieved.
        let hits = rank(&query, vec![defining], 5, &anchors);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, "/src/run_it_handler.rs");
    }

    #[test]
    fn rank_empty_anchors_is_identical_to_pre_2_4() {
        // Belt-and-braces: with no anchor terms the effective score equals the
        // cosine, so ordering matches the pure-cosine path.
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = vec![
            chunk("/far.md", vec![0.0, 1.0, 0.0], "far"),
            chunk("/near.md", vec![0.9, 0.1, 0.0], "near"),
            chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"),
        ];
        let hits = rank(&query, chunks, 2, &[]);
        assert_eq!(hits[0].file_path, "/near.md");
        assert_eq!(hits[1].file_path, "/mid.md");
    }

    // ── 2.5: active-file boost ──────────────────────────────────────────

    #[test]
    fn extract_active_file_reads_and_normalises_the_marker() {
        assert_eq!(
            extract_active_file("how does this work?\n\n[active_file: services/X/Foo.rs]")
                .as_deref(),
            Some("services/x/foo.rs"),
            "lowercased; marker payload extracted",
        );
        // Windows separators fold to '/'.
        assert_eq!(
            extract_active_file("[active_file: services\\x\\foo.rs]").as_deref(),
            Some("services/x/foo.rs"),
        );
        // No marker / blank → None (boost is then a no-op).
        assert_eq!(extract_active_file("a plain question"), None);
        assert_eq!(extract_active_file("[active_file:   ]"), None);
    }

    #[test]
    fn active_file_boost_weights_exact_over_directory_and_misses() {
        let exact = chunk("services/x/foo.rs", vec![1.0, 0.0], "body");
        let sibling = chunk("services/x/bar.rs", vec![1.0, 0.0], "body");
        let elsewhere = chunk("services/y/baz.rs", vec![1.0, 0.0], "body");
        let active = Some("services/x/foo.rs");
        assert!((active_file_boost(&exact, active) - ACTIVE_FILE_PATH_BOOST).abs() < 1e-9);
        assert!((active_file_boost(&sibling, active) - ACTIVE_FILE_DIR_BOOST).abs() < 1e-9);
        assert_eq!(active_file_boost(&elsewhere, active), 0.0);
        assert_eq!(active_file_boost(&exact, None), 0.0, "no active file → no boost");
        // A root-level open file never dir-matches every other root file.
        let root_a = chunk("a.rs", vec![1.0, 0.0], "");
        let root_b = chunk("b.rs", vec![1.0, 0.0], "");
        assert_eq!(active_file_boost(&root_b, Some("a.rs")), 0.0);
        assert!((active_file_boost(&root_a, Some("a.rs")) - ACTIVE_FILE_PATH_BOOST).abs() < 1e-9);
    }

    #[test]
    fn rank_with_active_file_promotes_the_open_file_keeping_reported_cosine() {
        // The open file has a lower cosine than a rival, but the active-file
        // boost lifts it to the top; the reported score stays the true cosine.
        let query = vec![1.0_f32, 0.0];
        let open = chunk("services/x/foo.rs", vec_with_cosine(0.60), "the file in the editor");
        let rival = chunk("services/y/other.rs", vec_with_cosine(0.70), "a higher-cosine rival");
        let hits = rank_with(
            &query,
            vec![rival.clone(), open.clone()],
            2,
            &[],
            Some("services/x/foo.rs"),
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].file_path, "services/x/foo.rs",
            "active-file boost (0.60 + 0.15 = 0.75) beats the rival's 0.70",
        );
        assert!(
            (hits[0].score - 0.60).abs() < 1e-3,
            "reported score stays cosine, got {}",
            hits[0].score
        );
    }

    #[test]
    fn rank_with_directory_boost_lifts_siblings_of_the_open_file() {
        // A generic question while services/x/foo.rs is open: a sibling in
        // services/x is nudged above an equally-cosine chunk elsewhere.
        let query = vec![1.0_f32, 0.0];
        let sibling = chunk("services/x/bar.rs", vec_with_cosine(0.60), "sibling");
        let elsewhere = chunk("services/y/baz.rs", vec_with_cosine(0.60), "elsewhere");
        let hits = rank_with(
            &query,
            vec![elsewhere.clone(), sibling.clone()],
            2,
            &[],
            Some("services/x/foo.rs"),
        );
        assert_eq!(
            hits[0].file_path, "services/x/bar.rs",
            "same-directory sibling ranks up",
        );
    }

    #[test]
    fn rank_with_none_active_file_is_identical_to_rank() {
        let query = vec![1.0_f32, 0.0, 0.0];
        let chunks = || {
            vec![
                chunk("/far.md", vec![0.0, 1.0, 0.0], "far"),
                chunk("/near.md", vec![0.9, 0.1, 0.0], "near"),
                chunk("/mid.md", vec![0.6, 0.6, 0.0], "mid"),
            ]
        };
        let via_rank = rank(&query, chunks(), 2, &[]);
        let via_with = rank_with(&query, chunks(), 2, &[], None);
        assert_eq!(via_rank.len(), via_with.len());
        for (a, b) in via_rank.iter().zip(via_with.iter()) {
            assert_eq!(a.file_path, b.file_path);
        }
    }

    #[test]
    fn to_value_has_the_python_shape() {
        let hit = SearchHit {
            file_path: "/a.md".into(),
            line_range: [3, 9],
            content: "body".into(),
            score: 0.42,
            chunk_idx: 2,
            lexical_score: None,
            fused_score: None,
        };
        let v = hit.to_value();
        assert_eq!(v["file_path"], "/a.md");
        assert_eq!(v["line_range"], json!([3, 9]));
        assert_eq!(v["content"], "body");
        assert_eq!(v["score"], 0.42);
        // With no fusion provenance, the two optional keys are ABSENT — today's
        // JSON shape is byte-identical (additive, OFF-safe).
        assert!(v.get("lexical_score").is_none(), "omitted when None");
        assert!(v.get("fused_score").is_none(), "omitted when None");
    }

    #[test]
    fn to_value_includes_provenance_when_set() {
        let hit = SearchHit {
            file_path: "/a.md".into(),
            line_range: [1, 2],
            content: "body".into(),
            score: 0.3,
            chunk_idx: 0,
            lexical_score: Some(7.5),
            fused_score: Some(0.021),
        };
        let v = hit.to_value();
        assert_eq!(v["lexical_score"], 7.5);
        assert_eq!(v["fused_score"], 0.021);
        assert_eq!(v["score"], 0.3, "score is still true cosine");
    }

    // ── L4: RRF-fused dynamic-k cutoff (pure) ───────────────────────────────

    /// Build descending fused candidates `(fused, cosine, lexical_opt, chunk)`
    /// for direct [`dynamic_k_fused`] tests, without an index.
    fn fscored(rows: &[(f64, f64, Option<f64>)]) -> Vec<(f64, f64, Option<f64>, IndexedChunk)> {
        rows.iter()
            .enumerate()
            .map(|(i, &(fused, cos, lex))| {
                (fused, cos, lex, chunk(&format!("/c{i}.rs"), vec![1.0, 0.0], "c"))
            })
            .collect()
    }

    fn fuse_cfg() -> LexicalConfig {
        LexicalConfig {
            enabled: true,
            rrf_k: 60.0,
            w_dense: 1.0,
            w_lex: 1.0,
            min_bm25: 0.5,
            fused_relative_floor: 0.6,
            ..LexicalConfig::default()
        }
    }

    #[test]
    fn fused_dynamic_k_zero_when_top_is_off_topic_to_both() {
        // Top has a sub-floor cosine AND no lexical hit → inject nothing.
        let s = fscored(&[(0.02, 0.40, None), (0.01, 0.30, None)]);
        assert_eq!(dynamic_k_fused(&s, 5, &fuse_cfg()), 0);
    }

    #[test]
    fn fused_dynamic_k_keeps_low_cosine_top_rescued_by_bm25() {
        // The approved bypass: the top has a sub-floor cosine but a strong BM25
        // hit clearing min_bm25 → it is on-topic and kept (the recall win).
        let s = fscored(&[(0.03, 0.30, Some(8.0)), (0.005, 0.20, None)]);
        let keep = dynamic_k_fused(&s, 5, &fuse_cfg());
        assert!(keep >= 1, "lexical-only top is on-topic and kept");
    }

    #[test]
    fn fused_dynamic_k_on_topic_via_cosine_keeps_prefix() {
        // Top clears the cosine floor → on-topic; the relative floor on fused
        // trims the tail below 0.6·top.
        let cfg = fuse_cfg();
        let top = 0.030_f64;
        let s = fscored(&[
            (top, 0.80, None),
            (0.62 * top, 0.50, None),     // above 0.6·top → kept
            (0.40 * top, 0.40, None),     // below 0.6·top → trimmed
        ]);
        assert_eq!(dynamic_k_fused(&s, 5, &cfg), 2);
    }

    #[test]
    fn fused_dynamic_k_capped_by_budget() {
        let cfg = fuse_cfg();
        let s = fscored(&[(0.03, 0.8, None), (0.029, 0.7, None), (0.028, 0.7, None)]);
        assert_eq!(dynamic_k_fused(&s, 2, &cfg), 2);
    }

    // ── L4: build_lexical_query strips the protocol markers ─────────────────

    #[test]
    fn build_lexical_query_strips_markers() {
        let q = "why does compose_retrieval_query fail?\n\n\
                 [anchors: compose_retrieval_query]\n[active_file: src/x.rs]";
        assert_eq!(
            build_lexical_query(q),
            "why does compose_retrieval_query fail?"
        );
        // No markers → trimmed user text verbatim.
        assert_eq!(build_lexical_query("plain question  "), "plain question");
    }

    // ── L4: end-to-end fusion through query_with_vec ────────────────────────

    /// Unit vector whose cosine against the `[1, 0]` query equals `c` (re-stated
    /// in the integration scope; `vec_with_cosine` above is the same shape).
    fn cos_vec(c: f64) -> Vec<f32> {
        vec_with_cosine(c as f32)
    }

    fn enable_fusion() {
        LexicalConfig::persist(LexicalConfig {
            enabled: true,
            rrf_k: 60.0,
            w_dense: 1.0,
            w_lex: 1.0,
            min_bm25: 0.5,
            fused_relative_floor: 0.6,
            ..LexicalConfig::default()
        })
        .unwrap();
    }

    #[test]
    fn fused_off_is_byte_identical_to_dense_only() {
        let _env = crate::test_support::TestEnv::new();
        LexicalConfig::persist(LexicalConfig::default()).unwrap(); // OFF
        let ws = "fuse-off";
        let chunks = vec![
            chunk("/near.rs", cos_vec(0.9), "near content alpha"),
            chunk("/mid.rs", cos_vec(0.6), "mid content beta"),
        ];
        store::save_chunks(ws, &chunks).unwrap();
        let q = vec![1.0_f32, 0.0];
        let hits = query_with_vec(ws, &q, "a question", 5);
        let dense = rank_with(&q, chunks, 5, &[], None);
        assert_eq!(hits.len(), dense.len());
        for (a, b) in hits.iter().zip(dense.iter()) {
            assert_eq!(a.file_path, b.file_path);
            assert_eq!(a.score, b.score);
            assert!(a.fused_score.is_none(), "no fusion provenance when OFF");
            assert!(a.lexical_score.is_none());
        }
        assert!(
            !lexical::has_lexical_index(ws),
            "OFF builds no lexical index (identity with today)"
        );
    }

    #[test]
    fn fused_surfaces_a_lexical_only_hit_below_the_cosine_floor() {
        let _env = crate::test_support::TestEnv::new();
        let ws = "fuse-bypass";
        // Target: a sub-floor cosine but a unique rare exact token. Decoy: even
        // lower cosine, no token match.
        let target = chunk(
            "/defining.rs",
            cos_vec(MIN_ABSOLUTE_SCORE - 0.20),
            "fn zqxjrare_handler() { do_work() }",
        );
        let decoy = chunk("/other.rs", cos_vec(MIN_ABSOLUTE_SCORE - 0.30), "unrelated prose body");
        store::save_chunks(ws, &[target, decoy]).unwrap();
        let q = vec![1.0_f32, 0.0];

        // Dense-only (OFF): both below the absolute floor ⇒ nothing injected.
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
        assert!(
            query_with_vec(ws, &q, "zqxjrare_handler", 5).is_empty(),
            "OFF: below the cosine floor, injects nothing"
        );

        // Fusion ON: the BM25 exact-token hit bypasses the absolute cosine floor
        // (the approved §1.4 bypass) and the low-cosine defining file is rescued.
        enable_fusion();
        let hits = query_with_vec(ws, &q, "zqxjrare_handler", 5);
        assert!(!hits.is_empty(), "ON: lexical hit rescues the low-cosine chunk");
        assert_eq!(hits[0].file_path, "/defining.rs");
        assert!(hits[0].lexical_score.is_some(), "carries BM25 provenance");
        assert!(hits[0].fused_score.is_some());
        assert!(
            (hits[0].score - (MIN_ABSOLUTE_SCORE - 0.20)).abs() < 1e-3,
            "reported score stays the true cosine, got {}",
            hits[0].score
        );
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
    }

    #[test]
    fn fused_off_topic_to_both_injects_nothing() {
        let _env = crate::test_support::TestEnv::new();
        let ws = "fuse-offtopic";
        let target = chunk("/a.rs", cos_vec(MIN_ABSOLUTE_SCORE - 0.20), "fn zqxjrare_handler() {}");
        store::save_chunks(ws, &[target]).unwrap();
        let q = vec![1.0_f32, 0.0];
        enable_fusion();
        // Sub-floor cosine AND a query token that matches nothing lexically →
        // on-topic to neither signal → empty (the off-topic guard holds).
        assert!(
            query_with_vec(ws, &q, "absent_token_qqzzx", 5).is_empty(),
            "off-topic to both arms injects nothing"
        );
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
    }

    #[test]
    fn fused_semantic_guardrail_dense_nailed_query_unaffected() {
        let _env = crate::test_support::TestEnv::new();
        let ws = "fuse-guardrail";
        // A strongly-cosine-relevant chunk the lexical query does NOT match.
        let strong = chunk("/strong.rs", cos_vec(0.85), "alpha bravo charlie delta");
        store::save_chunks(ws, &[strong]).unwrap();
        let q = vec![1.0_f32, 0.0];
        enable_fusion();
        // No lexical overlap, but the high cosine clears the floor → fused ≈
        // dense; the hit the dense path already nailed is still returned.
        let hits = query_with_vec(ws, &q, "echo foxtrot golf", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, "/strong.rs");
        assert!((hits[0].score - 0.85).abs() < 1e-3, "true cosine reported");
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
    }
}
