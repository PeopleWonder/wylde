//! Per-workspace lexical (BM25) inverted index — a sibling of [`store`] and
//! [`search`] (lexical-bm25 plan L1).
//!
//! A pure-Rust tantivy index living **inside the same `index/` bundle** as the
//! vectors, at `<data_dir>/workspaces/<id>/index/lexical/`. It holds the BM25
//! term postings + the chunk join key only — never a second copy of the chunk
//! bodies — so retrieval can add an exact-token recall signal (fused with the
//! dense cosine via RRF, §4 / L4) without a storage-engine swap and without a
//! second plaintext exposure.
//!
//! ## Schema (one doc = one chunk)
//!
//! | field | type | stored? | purpose |
//! |---|---|---|---|
//! | `chunk_id`  | `STRING` | **STORED** + indexed | join key back to the loaded [`IndexedChunk`]; also the **delete term** for incremental sync (L3) |
//! | `path_raw`  | `STRING` | indexed | exact-path delete fallback + exact-path term match |
//! | `path_text` | `TEXT`   | indexed | tokenised path/identifier BM25 (`/src/run_it.rs` → `src run it rs`) |
//! | `content`   | `TEXT`   | indexed, **NOT stored** | the BM25 body field — term postings only, no second plaintext copy |
//!
//! The default tantivy tokenizer (lowercase, split on non-alphanumeric) already
//! splits `snake_case` / `kebab-case` / `dotted.paths`, so a query for
//! `compose_retrieval_query` matches the file that defines it. (camelCase
//! splitting + a dedicated `symbols` field are L6 refinements, not v1.)
//!
//! ## Build, not walk
//!
//! The index is built **from the persisted chunk set** ([`build_from_chunks`]),
//! never from a fresh filesystem walk — so it inherits the `ExclusionMatcher`
//! hygiene and the content-hash manifest for free and can never drift from
//! `chunks.jsonl`. It is structurally incapable of indexing a `target/`
//! artifact the vector index skipped.
//!
//! ## Fail-soft
//!
//! [`search`] **never errors** — any tantivy failure (a missing index, a torn
//! segment, a parse error) yields an empty result, so the retrieval contract
//! (`rag_query` returns `[]`, never an error) holds exactly as the dense path's.

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use super::store::{index_dir, IndexedChunk};
use crate::common::ensure_dir;

/// Sub-directory of `index/` that holds the tantivy segments + term dict. Kept
/// inside the per-workspace bundle so the workspace-delete path removes it and
/// tantivy's temp/merge files inherit the same OS ACLs (never a global temp).
const LEXICAL_SUBDIR: &str = "lexical";

/// IndexWriter heap budget (bytes). tantivy requires ≥ ~15 MB; 50 MB gives the
/// merge policy comfortable headroom for the low-thousands-of-chunks corpora a
/// single workspace holds, while staying modest for the watcher's per-file path.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Relative BM25 boost on a path/identifier token match over a body match — the
/// defining file (whose path carries the symbol) should outrank a file that
/// merely mentions it, reproducing the dense path's `ANCHOR_PATH_BOOST` intent
/// as a principled field boost rather than a magic additive.
const PATH_FIELD_BOOST: f32 = 2.0;

/// BM25 field boost on the **content** match of a resolved anchor token (L5).
/// Higher than a plain body term (`1.0`) so a chunk that literally mentions a
/// referenced symbol outranks one that's merely topically near — but the IDF in
/// BM25 already does the heavy lifting (a rare identifier scores far above a
/// common word), so this stays modest rather than a flat additive.
const ANCHOR_CONTENT_BOOST: f32 = 3.0;

/// BM25 field boost on the **path/identifier** match of a resolved anchor token
/// (L5) — the strongest "the symbol's defining file ranks top" signal,
/// reproducing the old additive `ANCHOR_PATH_BOOST` as a principled, IDF-weighted
/// field boost with exact token boundaries (so `add` no longer matches inside
/// `address` — the `ANCHOR_TERM_MIN_LEN` substring hack is retired on this path).
const ANCHOR_PATH_FIELD_BOOST: f32 = 6.0;

/// The four schema fields, resolved once so callers don't re-`get_field`.
#[derive(Clone, Copy)]
pub struct LexicalFields {
    pub chunk_id: Field,
    pub path_raw: Field,
    pub path_text: Field,
    pub content: Field,
}

/// Build the lexical schema + field handles. One doc per chunk; `content` is
/// indexed but **NOT** `STORED` (no second body copy), `chunk_id` is `STORED`
/// so a hit can be joined back to the in-memory [`IndexedChunk`].
fn build_schema() -> (Schema, LexicalFields) {
    let mut b = Schema::builder();
    let chunk_id = b.add_text_field("chunk_id", STRING | STORED);
    let path_raw = b.add_text_field("path_raw", STRING);
    let path_text = b.add_text_field("path_text", TEXT);
    let content = b.add_text_field("content", TEXT);
    let schema = b.build();
    (
        schema,
        LexicalFields {
            chunk_id,
            path_raw,
            path_text,
            content,
        },
    )
}

/// `<data_dir>/workspaces/<id>/index/lexical/`.
fn lexical_dir(workspace_id: &str) -> std::path::PathBuf {
    index_dir(workspace_id).join(LEXICAL_SUBDIR)
}

/// True when a tantivy index already exists for this workspace (its `meta.json`
/// is present). Decides the one-time backfill (L2.5): toggle ON + no lexical
/// index ⇒ build it once from the existing chunks (no embedder).
pub fn has_lexical_index(workspace_id: &str) -> bool {
    lexical_dir(workspace_id).join("meta.json").exists()
}

/// Open the workspace's lexical index, creating an empty one (schema-stamped) if
/// absent. The directory is created if needed; tantivy's segments + merge temps
/// land inside it.
fn open_or_create(workspace_id: &str) -> tantivy::Result<(Index, LexicalFields)> {
    let dir = lexical_dir(workspace_id);
    ensure_dir(&dir).map_err(|e| tantivy::TantivyError::IoError(std::sync::Arc::new(e)))?;
    let (schema, fields) = build_schema();
    let mmap = tantivy::directory::MmapDirectory::open(&dir)?;
    let index = Index::open_or_create(mmap, schema)?;
    Ok((index, fields))
}

/// One tantivy document for a chunk — the join key, the raw + tokenised path,
/// and the (not-stored) body for BM25 term postings.
fn chunk_doc(f: &LexicalFields, c: &IndexedChunk) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(f.chunk_id, &c.id);
    doc.add_text(f.path_raw, &c.path);
    doc.add_text(f.path_text, &c.path);
    doc.add_text(f.content, &c.content);
    doc
}

/// **Full (re)build** of the lexical index from a chunk set: open, clear every
/// existing doc, add one doc per chunk, commit. Idempotent — running it twice
/// over the same chunks converges to the same index. Called by the full-reindex
/// path and the one-time backfill (L2). No embedder, no Ollama: BM25 is local.
pub fn build_from_chunks(workspace_id: &str, chunks: &[IndexedChunk]) -> tantivy::Result<()> {
    let (index, fields) = open_or_create(workspace_id)?;
    let mut writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
    writer.delete_all_documents()?;
    for c in chunks {
        writer.add_document(chunk_doc(&fields, c))?;
    }
    writer.commit()?;
    Ok(())
}

/// **Incremental per-file upsert** (watcher hot path, L3): delete every doc
/// whose `path_raw` equals `canonical` (exact-term, so it clears the file's old
/// chunks regardless of how many or what ids they had), add the file's fresh
/// chunks, and commit. No full rebuild — a single-file save stays cheap. The
/// caller must already have written the matching vectors (§2.6 invariant:
/// lexical never holds a chunk the vector store lacks).
pub fn sync_upsert_file(
    workspace_id: &str,
    canonical: &str,
    chunks: &[IndexedChunk],
) -> tantivy::Result<()> {
    let (index, fields) = open_or_create(workspace_id)?;
    let mut writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
    writer.delete_term(Term::from_field_text(fields.path_raw, canonical));
    for c in chunks {
        writer.add_document(chunk_doc(&fields, c))?;
    }
    writer.commit()?;
    Ok(())
}

/// **Incremental remove** (watcher hot path, L3): drop a deleted file or a
/// deleted directory's whole subtree. Deletes the exact file by `path_raw`
/// **and** every `chunk_id` in `subtree_chunk_ids` (the manifest-recorded ids
/// under a removed directory, which an exact path term can't reach). Belt-and-
/// braces: even if the manifest lacked some ids, the `chunk_id` join at search
/// time silently drops any orphaned lexical hit (§2.6), so a miss here can never
/// surface wrong content — it only leaves a harmless tombstone until the next
/// full rebuild.
pub fn sync_remove_file(
    workspace_id: &str,
    canonical: &str,
    subtree_chunk_ids: &[String],
) -> tantivy::Result<()> {
    // Nothing to do if the index was never built.
    if !has_lexical_index(workspace_id) {
        return Ok(());
    }
    let (index, fields) = open_or_create(workspace_id)?;
    let mut writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
    writer.delete_term(Term::from_field_text(fields.path_raw, canonical));
    for id in subtree_chunk_ids {
        writer.delete_term(Term::from_field_text(fields.chunk_id, id));
    }
    writer.commit()?;
    Ok(())
}

/// Tokenise `text` with the index's default analyzer (the same one the `content`
/// / `path_text` fields are tokenised with), so a query token matches a posting.
/// Lowercases + splits on non-alphanumeric, dropping empties.
fn tokenize(index: &Index, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(mut analyzer) = index.tokenizers().get("default") else {
        return out;
    };
    let mut stream = analyzer.token_stream(text);
    while stream.advance() {
        let t = &stream.token().text;
        if !t.is_empty() {
            out.push(t.clone());
        }
    }
    out
}

/// Append `Should` BM25 term-clauses for each token to `clauses`: one over
/// `content` (boost `content_boost`) and one over `path_text` (boost
/// `path_boost`). Built from terms — never the `QueryParser` — so code
/// identifiers with `:`/`-`/`(` can't trip query syntax. A boost of `1.0` is the
/// BM25 baseline.
fn push_term_clauses(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    f: &LexicalFields,
    terms: &[String],
    content_boost: f32,
    path_boost: f32,
) {
    for tok in terms {
        let content_q = TermQuery::new(Term::from_field_text(f.content, tok), IndexRecordOption::WithFreqs);
        clauses.push((
            Occur::Should,
            Box::new(BoostQuery::new(Box::new(content_q), content_boost)),
        ));
        let path_q = TermQuery::new(Term::from_field_text(f.path_text, tok), IndexRecordOption::WithFreqs);
        clauses.push((
            Occur::Should,
            Box::new(BoostQuery::new(Box::new(path_q), path_boost)),
        ));
    }
}

/// Build a robust BM25 query from already-tokenised body terms — an OR over
/// `content` (boost 1.0) + `path_text` (boost [`PATH_FIELD_BOOST`]) per token.
/// An empty term list yields an empty query (no matches, scored 0).
fn term_or_query(f: &LexicalFields, terms: &[String]) -> BooleanQuery {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() * 2);
    push_term_clauses(&mut clauses, f, terms, 1.0, PATH_FIELD_BOOST);
    BooleanQuery::new(clauses)
}

/// BM25-search the workspace's lexical index for `query_text`, returning up to
/// `limit` `(chunk_id, bm25_score)` pairs, highest score first.
///
/// **Never errors** — a missing index, a blank query, or any tantivy failure all
/// yield an empty vec, so the retrieval contract holds. Returns *only* the join
/// key + score; the body is fetched by joining `chunk_id` back to the loaded
/// [`IndexedChunk`] set the dense arm already holds (no text carried here).
pub fn search(workspace_id: &str, query_text: &str, limit: usize) -> Vec<(String, f64)> {
    if query_text.trim().is_empty() || limit == 0 || !has_lexical_index(workspace_id) {
        return Vec::new();
    }
    match search_inner(workspace_id, query_text, limit) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!("workspaces.rag.lexical: search failed for {workspace_id}: {e}");
            Vec::new()
        }
    }
}

/// The fallible body of [`search`], factored out so the public entry can map any
/// error to the fail-soft empty vec.
fn search_inner(
    workspace_id: &str,
    query_text: &str,
    limit: usize,
) -> tantivy::Result<Vec<(String, f64)>> {
    let (index, fields) = open_or_create(workspace_id)?;
    let terms = tokenize(&index, query_text);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let query = term_or_query(&fields, &terms);
    let reader = index.reader()?;
    let searcher = reader.searcher();
    collect_top(&searcher, &fields, &query, limit)
}

/// BM25-search with the resolved **anchor terms** folded in as a boosted
/// sub-query (L5 / §3) — the rework of the legacy substring anchor-bias. The
/// cleaned user `text` contributes baseline body terms; each anchor token is
/// added at [`ANCHOR_CONTENT_BOOST`] on `content` and [`ANCHOR_PATH_FIELD_BOOST`]
/// on `path_text`, so the symbol's defining file ranks top — with **IDF
/// weighting and exact token boundaries for free** (a rare identifier outscores
/// a common word; `add` no longer matches inside `address`). Multiple anchors
/// compose via normal BM25 term accumulation. Same fail-soft contract as
/// [`search`]. With no anchors this is identical to [`search`].
pub fn search_boosted(
    workspace_id: &str,
    text: &str,
    anchors: &[String],
    limit: usize,
) -> Vec<(String, f64)> {
    if (text.trim().is_empty() && anchors.is_empty())
        || limit == 0
        || !has_lexical_index(workspace_id)
    {
        return Vec::new();
    }
    match search_boosted_inner(workspace_id, text, anchors, limit) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!("workspaces.rag.lexical: boosted search failed for {workspace_id}: {e}");
            Vec::new()
        }
    }
}

fn search_boosted_inner(
    workspace_id: &str,
    text: &str,
    anchors: &[String],
    limit: usize,
) -> tantivy::Result<Vec<(String, f64)>> {
    let (index, fields) = open_or_create(workspace_id)?;
    let base_terms = tokenize(&index, text);
    // Tokenise each anchor through the same analyzer (so `compose_retrieval_query`
    // splits into its sub-tokens, matching the indexed postings), deduped.
    let mut anchor_toks: Vec<String> = Vec::new();
    for a in anchors {
        for t in tokenize(&index, a) {
            if !anchor_toks.contains(&t) {
                anchor_toks.push(t);
            }
        }
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    push_term_clauses(&mut clauses, &fields, &base_terms, 1.0, PATH_FIELD_BOOST);
    push_term_clauses(
        &mut clauses,
        &fields,
        &anchor_toks,
        ANCHOR_CONTENT_BOOST,
        ANCHOR_PATH_FIELD_BOOST,
    );
    if clauses.is_empty() {
        return Ok(Vec::new());
    }
    let query = BooleanQuery::new(clauses);
    let reader = index.reader()?;
    let searcher = reader.searcher();
    collect_top(&searcher, &fields, &query, limit)
}

/// Run a query and collect up to `limit` `(chunk_id, bm25)` pairs, highest first.
fn collect_top(
    searcher: &tantivy::Searcher,
    fields: &LexicalFields,
    query: &dyn Query,
    limit: usize,
) -> tantivy::Result<Vec<(String, f64)>> {
    let top = searcher.search(query, &TopDocs::with_limit(limit))?;
    let mut out = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        if let Some(id) = doc.get_first(fields.chunk_id).and_then(|v| v.as_str()) {
            out.push((id.to_owned(), score as f64));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn chunk(id: &str, path: &str, content: &str) -> IndexedChunk {
        IndexedChunk {
            id: id.to_owned(),
            path: path.to_owned(),
            chunk_idx: 0,
            content: content.to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 4,
            vector: vec![0.1, 0.2],
        }
    }

    #[test]
    fn build_then_search_finds_an_exact_token() {
        let _env = TestEnv::new();
        let ws = "lx-build";
        assert!(!has_lexical_index(ws), "no index before build");
        let chunks = vec![
            chunk("c0", "/src/search.rs", "const ANCHOR_BOOST_CAP: f64 = 0.30;"),
            chunk("c1", "/src/notes.md", "some loosely related prose about boosting"),
            chunk("c2", "/src/other.rs", "fn unrelated() {}"),
        ];
        build_from_chunks(ws, &chunks).unwrap();
        assert!(has_lexical_index(ws), "index exists after build");

        // A rare exact identifier ranks its defining chunk first.
        let hits = search(ws, "ANCHOR_BOOST_CAP", 5);
        assert!(!hits.is_empty(), "exact token is found");
        assert_eq!(hits[0].0, "c0", "defining chunk ranks top");
        assert!(hits[0].1 > 0.0, "positive BM25 score");
    }

    #[test]
    fn search_matches_path_tokens() {
        let _env = TestEnv::new();
        let ws = "lx-path";
        let chunks = vec![
            chunk("c0", "/src/run_it_handler.rs", "fn handler() {}"),
            chunk("c1", "/src/other.rs", "fn other() {}"),
        ];
        build_from_chunks(ws, &chunks).unwrap();
        // The query tokens hit the path field of the defining file.
        let hits = search(ws, "run_it_handler", 5);
        assert_eq!(hits[0].0, "c0", "path-token match ranks the defining file");
    }

    #[test]
    fn search_is_empty_without_an_index() {
        let _env = TestEnv::new();
        // No build → fail-soft empty, never an error.
        assert!(search("no-lexical-000000", "anything", 5).is_empty());
    }

    #[test]
    fn search_empty_query_and_zero_limit_are_empty() {
        let _env = TestEnv::new();
        let ws = "lx-empty";
        build_from_chunks(ws, &[chunk("c0", "/a.rs", "hello world")]).unwrap();
        assert!(search(ws, "   ", 5).is_empty(), "blank query");
        assert!(search(ws, "hello", 0).is_empty(), "zero limit");
    }

    #[test]
    fn rebuild_clears_stale_docs() {
        let _env = TestEnv::new();
        let ws = "lx-rebuild";
        build_from_chunks(ws, &[chunk("c0", "/a.rs", "alpha unique_token_xyz")]).unwrap();
        assert_eq!(search(ws, "unique_token_xyz", 5).len(), 1);
        // Rebuild with a different chunk set: the stale token must be gone.
        build_from_chunks(ws, &[chunk("c1", "/b.rs", "beta different")]).unwrap();
        assert!(
            search(ws, "unique_token_xyz", 5).is_empty(),
            "full rebuild cleared the old docs"
        );
        assert_eq!(search(ws, "different", 5)[0].0, "c1");
    }

    #[test]
    fn query_with_special_chars_does_not_panic() {
        let _env = TestEnv::new();
        let ws = "lx-special";
        build_from_chunks(ws, &[chunk("c0", "/a.rs", "os error 32 sharing violation")]).unwrap();
        // Query syntax characters that would trip a QueryParser are tokenised
        // away harmlessly here (we build the query from terms, not the parser).
        let hits = search(ws, "os error 32 [](){}:-+", 5);
        assert_eq!(hits[0].0, "c0");
    }

    // ── L3: incremental delta primitives + delta == full convergence ────────

    #[test]
    fn upsert_file_replaces_a_files_docs_by_path() {
        let _env = TestEnv::new();
        let ws = "lx-upsert";
        build_from_chunks(ws, &[chunk("c0", "/a.rs", "alphaword zeta")]).unwrap();
        assert_eq!(search(ws, "alphaword", 5).len(), 1);
        // Re-save the same file with disjoint-token content: the exact-path
        // delete clears the stale doc, the new one replaces it. (Tokens are
        // chosen not to overlap, since the default tokenizer splits on
        // non-alphanumerics — a shared sub-token would match either doc.)
        sync_upsert_file(ws, "/a.rs", &[chunk("c1", "/a.rs", "betaword omega")]).unwrap();
        assert!(
            search(ws, "alphaword", 5).is_empty(),
            "stale doc cleared by path delete"
        );
        assert_eq!(search(ws, "betaword", 5)[0].0, "c1");
    }

    #[test]
    fn remove_file_drops_by_path_and_subtree_ids() {
        let _env = TestEnv::new();
        let ws = "lx-remove";
        build_from_chunks(
            ws,
            &[
                chunk("c0", "/dir/a.rs", "alphaword"),
                chunk("c1", "/dir/sub/b.rs", "betaword"),
                chunk("c2", "/keep.rs", "gammaword"),
            ],
        )
        .unwrap();
        // Remove the /dir directory: exact path doesn't match (it's a dir), but
        // the subtree ids drop both files under it; the sibling survives.
        sync_remove_file(ws, "/dir", &["c0".into(), "c1".into()]).unwrap();
        assert!(search(ws, "alphaword", 5).is_empty());
        assert!(search(ws, "betaword", 5).is_empty());
        assert_eq!(search(ws, "gammaword", 5)[0].0, "c2", "sibling kept");
    }

    /// The convergence guarantee (L3): a lexical index built **incrementally**
    /// (full build of a seed, then per-file upserts/removes) returns the same
    /// **live document set** for a query as one built in a single full pass over
    /// the same final chunk set. Mirrors the "delta == full" property the vector
    /// + graph halves already hold.
    ///
    /// Convergence is asserted on the matched-id *set*, not exact BM25 ordering:
    /// tantivy keeps a removed doc's term stats until a segment merge, so an
    /// incrementally-built index can score two near-tied live docs in a slightly
    /// different order than a clean build — but the set of *live matches* is
    /// identical (deleted docs are filtered), and RRF fuses on rank, robust to
    /// the minor drift. Each query below targets a token unique to one chunk, so
    /// the live set is unambiguous.
    #[test]
    fn delta_equals_full_after_incremental_ops() {
        let _env = TestEnv::new();
        let a0 = chunk("a0", "/a.rs", "fn compose_retrieval_query body");
        let a1 = chunk("a1", "/a.rs", "additional helper paragraph");
        let b0 = chunk("b0", "/b.rs", "const removed_marker_token usize");
        let c0 = chunk("c0", "/c.rs", "ReadDirectoryChangesW watcher notes");

        // Reference: one full build over the final set {a0, a1, c0} (b removed).
        let full = "lx-conv-full";
        build_from_chunks(full, &[a0.clone(), a1.clone(), c0.clone()]).unwrap();

        // Incremental: seed with a, upsert b, upsert c, then remove b.
        let incr = "lx-conv-incr";
        build_from_chunks(incr, &[a0.clone(), a1.clone()]).unwrap();
        sync_upsert_file(incr, "/b.rs", std::slice::from_ref(&b0)).unwrap();
        sync_upsert_file(incr, "/c.rs", std::slice::from_ref(&c0)).unwrap();
        sync_remove_file(incr, "/b.rs", &["b0".into()]).unwrap();

        // Same live matched-id set for each unique-token query.
        let id_set = |ws: &str, q: &str| {
            let mut ids: Vec<String> = search(ws, q, 10).into_iter().map(|(id, _)| id).collect();
            ids.sort();
            ids
        };
        for q in [
            "compose_retrieval_query",
            "additional",
            "ReadDirectoryChangesW",
        ] {
            assert_eq!(
                id_set(full, q),
                id_set(incr, q),
                "delta and full return the same live set for {q:?}"
            );
        }
        // The removed file's unique token is absent from both.
        assert!(search(full, "removed_marker_token", 5).is_empty());
        assert!(
            search(incr, "removed_marker_token", 5).is_empty(),
            "removed file's token absent in the incrementally-built index too"
        );
    }

    // ── L5: anchors as a boosted BM25 sub-query ─────────────────────────────

    #[test]
    fn anchor_boost_promotes_the_defining_file() {
        let _env = TestEnv::new();
        let ws = "lx-anchor";
        build_from_chunks(
            ws,
            &[
                // Defining file: the symbol is in its PATH (highest-boosted field).
                chunk("c0", "/src/run_it_handler.rs", "fn run_it_handler() { work() }"),
                // A file that merely mentions the symbol in its body.
                chunk("c1", "/src/caller.rs", "calls run_it_handler from here somewhere"),
                // Unrelated.
                chunk("c2", "/src/other.rs", "totally different content"),
            ],
        )
        .unwrap();
        // With the anchor folded in as a boosted sub-query, the defining file
        // (path match, highest boost) ranks above the mere mention.
        let hits = search_boosted(ws, "where is it", &["run_it_handler".to_owned()], 5);
        assert_eq!(hits[0].0, "c0", "anchor path-boost ranks the defining file top");
        assert!(hits.iter().any(|(id, _)| id == "c1"), "the mention still matches");
    }

    #[test]
    fn anchor_exact_token_boundary_add_not_in_address() {
        let _env = TestEnv::new();
        let ws = "lx-boundary";
        build_from_chunks(
            ws,
            &[
                chunk("c0", "/net/dns.rs", "the address record resolver"),
                chunk("c1", "/ops/add.rs", "fn add(a, b) { a + b }"),
            ],
        )
        .unwrap();
        // BM25 matches whole tokens, so the anchor `add` hits the `add` token in
        // c1 but NOT inside `address` in c0 — the old ≥4-char substring hack is
        // unnecessary on this path.
        let hits = search_boosted(ws, "", &["add".to_owned()], 5);
        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"c1"), "exact `add` token matched");
        assert!(!ids.contains(&"c0"), "`add` did NOT match inside `address`");
    }

    #[test]
    fn search_boosted_without_anchors_matches_plain_search_set() {
        let _env = TestEnv::new();
        let ws = "lx-boosted-plain";
        build_from_chunks(
            ws,
            &[chunk("c0", "/a.rs", "alphaword body"), chunk("c1", "/b.rs", "betaword body")],
        )
        .unwrap();
        // No anchors ⇒ same matched set as the plain body search.
        let boosted: Vec<String> = search_boosted(ws, "alphaword", &[], 5)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(boosted, vec!["c0".to_owned()]);
        // Empty text + empty anchors ⇒ nothing.
        assert!(search_boosted(ws, "  ", &[], 5).is_empty());
    }
}
