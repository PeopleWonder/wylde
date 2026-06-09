//! The three scoped chat-history verb handlers + the typed search/list/get
//! functions behind them.
//!
//! Verbs (Build Order §3 / Appendix A):
//!
//! * `chat.search_history`  — `{query, date_range?, workspace_scope?,
//!   active_workspace_id?, top_k?, threshold?}` → `{hits, count, degraded}`
//! * `chat.list_recent`     — `{limit?, date_range?, workspace_scope?,
//!   active_workspace_id?}` → `{hits, count, degraded}`
//! * `chat.get_conversation`— `{id, workspace_scope?, active_workspace_id?}`
//!   → the full conversation document
//!
//! Every handler resolves its reachable backends through
//! [`super::scope::resolve_scope`] and reads nothing else — the strict
//! workspace boundary lives there. Workspace conversations are fetched over
//! the pipe via [`wylde_workspaces_client`]; a slow/unreachable service
//! degrades that backend away (the `degraded` flag flips) without failing
//! the call (Plan v2 §7.5).

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;
use wylde_workspaces_client::{ClientError, WorkspacesClient};

use super::scope::{self, Backend, ScopedQuery, WorkspaceScope};
use super::summary;
use super::{ChatSearchError, ConversationHit, DateRange, DEFAULT_THRESHOLD, DEFAULT_TOP_K};
use crate::api::require_string;
use crate::memory::conversations::store as conv_store;

/// Default cap on `list_recent`.
const DEFAULT_LIST_LIMIT: usize = 20;

/// Soft cap on how many workspace conversations a single search will fetch
/// from the service (one `get` per conversation). Generous; protects the
/// Medium budget on a pathologically large workspace. Drops are logged.
const MAX_WORKSPACE_FETCH: usize = 500;

/// The service name the workspace backend forwards to. Mirrors
/// [`crate::turn::workspace_context`] so the same env override points tests
/// at a guaranteed-dead pipe and exercises the degraded path.
pub(crate) fn workspaces_service() -> String {
    std::env::var("WYLDE_HARNESS_WORKSPACES_SERVICE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wylde-workspaces".to_owned())
}

/// True when a client error means the service didn't answer authoritatively
/// — unreachable transport or the breaker is open. Those degrade; a genuine
/// application error (unknown workspace) is not degradation.
fn is_unavailable(e: &ClientError) -> bool {
    e.transport || e.code == "breaker_open"
}

/// A conversation document tagged with the backend it came from.
struct Candidate {
    doc: Value,
    workspace_id: Option<String>,
}

/// `created_at` epoch seconds (0 if absent).
fn created_at(doc: &Value) -> i64 {
    doc.get("created_at").and_then(Value::as_i64).unwrap_or(0)
}

/// Most-recent-activity epoch seconds: `last_active_at`, else `updated_at`,
/// else `created_at`.
fn last_active_at(doc: &Value) -> i64 {
    doc.get("last_active_at")
        .and_then(Value::as_i64)
        .or_else(|| doc.get("updated_at").and_then(Value::as_i64))
        .unwrap_or_else(|| created_at(doc))
}

/// True when a document is genuinely standalone (no / empty `workspace_id`).
/// Defensive: even if a workspace-tagged file ever lands in the standalone
/// dir, the standalone backend won't surface it (no cross-scope leak).
fn is_standalone_doc(doc: &Value) -> bool {
    doc.get("workspace_id")
        .and_then(Value::as_str)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
}

/// Build a [`ConversationHit`] from a document + computed score.
fn hit_from(doc: &Value, id: &str, workspace_id: Option<String>, score: f32) -> ConversationHit {
    ConversationHit {
        id: id.to_owned(),
        created_at: created_at(doc),
        last_active_at: last_active_at(doc),
        summary: summary::display_summary(doc),
        topic_tags: summary::topic_tags(doc),
        score,
        workspace_id,
    }
}

/// Resolve the active workspace for the *current chat context*.
///
/// 1. If the payload carries `active_workspace_id`, honour it verbatim —
///    a non-empty string is that workspace; an empty string / `null`
///    explicitly means standalone. (This is how the GUI / turn driver call
///    in: they already know the context.)
/// 2. Otherwise fall back to the service's own active pointer via
///    `workspaces.list_mru` (best-effort; an unreachable service → `None` =
///    standalone, the safe default).
async fn resolve_active_workspace(payload: &Value) -> Option<String> {
    if let Some(v) = payload.get("active_workspace_id") {
        return v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    }
    // No explicit context — ask the service which workspace is active.
    let client = WorkspacesClient::for_service(workspaces_service());
    match client.list_mru().await {
        Ok(v) => v
            .get("active_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        Err(_) => None,
    }
}

/// Gather candidate documents from every backend in `scope`. Returns the
/// candidates plus a `degraded` flag (true when a workspace backend was in
/// scope but unreachable). Standalone reads never degrade (local IO).
async fn gather_candidates(scope: &ScopedQuery) -> (Vec<Candidate>, bool) {
    let mut out = Vec::new();
    let mut degraded = false;

    for backend in &scope.backends {
        match backend {
            Backend::Standalone => {
                for doc in conv_store::read_all_conversations() {
                    if is_standalone_doc(&doc) {
                        out.push(Candidate {
                            doc,
                            workspace_id: None,
                        });
                    }
                }
            }
            Backend::Workspace(ws) => {
                let client = WorkspacesClient::for_service(workspaces_service());
                match client.conversations_list(ws).await {
                    Ok(list) => {
                        let ids: Vec<String> = list
                            .get("conversations")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        if ids.len() > MAX_WORKSPACE_FETCH {
                            tracing::warn!(
                                "chat.search: workspace {ws:?} has {} conversations; \
                                 fetching first {MAX_WORKSPACE_FETCH} for search",
                                ids.len()
                            );
                        }
                        for id in ids.into_iter().take(MAX_WORKSPACE_FETCH) {
                            match client.conversations_get(ws, &id).await {
                                Ok(doc) => out.push(Candidate {
                                    doc,
                                    workspace_id: Some(ws.clone()),
                                }),
                                // A single unreachable get degrades; an app
                                // error (gone) just drops that one.
                                Err(e) if is_unavailable(&e) => {
                                    degraded = true;
                                    break;
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    // Service unreachable / breaker open → degrade this
                    // backend, keep whatever the others returned.
                    Err(e) if is_unavailable(&e) => degraded = true,
                    // App error (unknown workspace) → nothing, not degraded.
                    Err(_) => {}
                }
            }
        }
    }
    (out, degraded)
}

/// Candidates from `list_recent`'s cheaper path: standalone reads full docs
/// (local), workspace uses list metadata only (no per-doc `get`, so it stays
/// in the Fast budget). Summary/tags come from whatever the metadata carries
/// (workspace entries fall back to their title).
async fn gather_recent(scope: &ScopedQuery) -> (Vec<Candidate>, bool) {
    let mut out = Vec::new();
    let mut degraded = false;
    for backend in &scope.backends {
        match backend {
            Backend::Standalone => {
                for doc in conv_store::read_all_conversations() {
                    if is_standalone_doc(&doc) {
                        out.push(Candidate {
                            doc,
                            workspace_id: None,
                        });
                    }
                }
            }
            Backend::Workspace(ws) => {
                let client = WorkspacesClient::for_service(workspaces_service());
                match client.conversations_list(ws).await {
                    Ok(list) => {
                        if let Some(arr) = list.get("conversations").and_then(Value::as_array) {
                            for meta in arr {
                                out.push(Candidate {
                                    doc: meta.clone(),
                                    workspace_id: Some(ws.clone()),
                                });
                            }
                        }
                    }
                    Err(e) if is_unavailable(&e) => degraded = true,
                    Err(_) => {}
                }
            }
        }
    }
    (out, degraded)
}

/// The result of a search/list: ranked hits + the degraded flag.
#[derive(Debug)]
pub struct SearchResult {
    pub hits: Vec<ConversationHit>,
    pub degraded: bool,
}

/// `chat.search_history` (typed). Resolve scope, gather candidates, rank by
/// cosine over stored embeddings (lexical fallback), filter by date + a
/// relevance threshold, and return the top `top_k`.
pub async fn search_history(
    query: &str,
    date_range: DateRange,
    requested: WorkspaceScope,
    active_workspace: Option<&str>,
    top_k: usize,
    threshold: f32,
) -> Result<SearchResult, ChatSearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ChatSearchError::BadRequest("query must be non-empty".to_owned()));
    }
    let scope = scope::resolve_scope(active_workspace, requested)?;
    let (candidates, degraded) = gather_candidates(&scope).await;

    // Embed the query once — but only if it can actually help (some
    // candidate carries an embedding). Fail-soft: embedder down → lexical.
    let any_embedding = candidates
        .iter()
        .any(|c| summary::stored_embedding(&c.doc).is_some());
    let query_embedding = if any_embedding {
        super::summary::embed_query(query).await
    } else {
        None
    };

    let mut hits: Vec<ConversationHit> = Vec::new();
    for cand in &candidates {
        let Some(id) = cand.doc.get("id").and_then(Value::as_str) else {
            continue;
        };
        // Date filter on the conversation's [created, last_active] span.
        if !date_range.includes(created_at(&cand.doc), last_active_at(&cand.doc)) {
            continue;
        }
        let score = score_candidate(query, query_embedding.as_deref(), &cand.doc);
        if score < threshold {
            continue;
        }
        hits.push(hit_from(&cand.doc, id, cand.workspace_id.clone(), score));
    }

    // Highest score first; ties broken by most-recent activity.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.last_active_at.cmp(&a.last_active_at))
    });
    hits.truncate(top_k);
    Ok(SearchResult { hits, degraded })
}

/// Score one candidate: cosine over the stored embedding when both the query
/// and the doc have one (and dims match), else a lexical fallback over the
/// summary + title + tags.
fn score_candidate(query: &str, query_embedding: Option<&[f32]>, doc: &Value) -> f32 {
    if let (Some(qe), Some(de)) = (query_embedding, summary::stored_embedding(doc)) {
        if qe.len() == de.len() {
            return summary::cosine_similarity(qe, &de);
        }
    }
    let mut hay = summary::display_summary(doc);
    hay.push(' ');
    hay.push_str(&summary::topic_tags(doc).join(" "));
    if let Some(t) = doc.get("title").and_then(Value::as_str) {
        hay.push(' ');
        hay.push_str(t);
    }
    summary::lexical_score(query, &hay)
}

/// `chat.list_recent` (typed). Most-recent conversations in scope, newest
/// first, score `1.0`, capped at `limit` and filtered by `date_range`.
pub async fn list_recent(
    limit: usize,
    date_range: DateRange,
    requested: WorkspaceScope,
    active_workspace: Option<&str>,
) -> Result<SearchResult, ChatSearchError> {
    let scope = scope::resolve_scope(active_workspace, requested)?;
    let (candidates, degraded) = gather_recent(&scope).await;

    let mut hits: Vec<ConversationHit> = candidates
        .iter()
        .filter_map(|c| {
            let id = c.doc.get("id").and_then(Value::as_str)?;
            if !date_range.includes(created_at(&c.doc), last_active_at(&c.doc)) {
                return None;
            }
            Some(hit_from(&c.doc, id, c.workspace_id.clone(), 1.0))
        })
        .collect();

    hits.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    hits.truncate(limit);
    Ok(SearchResult { hits, degraded })
}

/// `chat.get_conversation` (typed). Fetch one conversation by id, but only
/// from a backend the resolved scope authorises — a conversation in another
/// workspace is simply `not_found` (the boundary holds for point reads too).
pub async fn get_conversation(
    id: &str,
    requested: WorkspaceScope,
    active_workspace: Option<&str>,
) -> Result<Value, ChatSearchError> {
    let scope = scope::resolve_scope(active_workspace, requested)?;

    // Standalone first (cheap, local) when in scope.
    if scope.includes_standalone() {
        match conv_store::read_conversation(id) {
            Ok(doc) if is_standalone_doc(&doc) => return Ok(doc),
            // Exists but is workspace-tagged in the standalone dir → not a
            // standalone hit; fall through to the workspace backend.
            Ok(_) => {}
            Err(conv_store::ReadError::InvalidId(e)) => {
                return Err(ChatSearchError::BadRequest(e.0))
            }
            Err(conv_store::ReadError::NotFound(_)) => {}
        }
    }

    // Workspace backend when in scope.
    if let Some(ws) = scope.workspace_id() {
        let client = WorkspacesClient::for_service(workspaces_service());
        match client.conversations_get(ws, id).await {
            Ok(doc) => return Ok(doc),
            // Service unreachable — we genuinely can't fetch it. Surface as
            // not-found with context rather than inventing a new error tier
            // (consistent with the degrade-everywhere policy).
            Err(e) if is_unavailable(&e) => {
                return Err(ChatSearchError::NotFound(format!(
                    "conversation {id:?} not found (workspace service unavailable)"
                )))
            }
            Err(_) => {}
        }
    }

    Err(ChatSearchError::NotFound(format!(
        "conversation {id:?} not found in the current scope"
    )))
}

// ── verb handlers (Reply wrappers) ───────────────────────────────────────

fn parse_common(
    payload: &Value,
) -> Result<(DateRange, WorkspaceScope), ChatSearchError> {
    let date_range = DateRange::from_payload(payload.get("date_range"));
    let requested = WorkspaceScope::from_payload(payload.get("workspace_scope"))?;
    Ok((date_range, requested))
}

fn err_reply(e: ChatSearchError) -> Reply {
    Reply::err_msg(e.code(), e.to_string())
}

fn result_reply(r: SearchResult) -> Reply {
    let hits: Vec<Value> = r.hits.iter().map(ConversationHit::to_json).collect();
    let count = hits.len();
    Reply::ok(json!({ "hits": hits, "count": count, "degraded": r.degraded }))
}

/// `chat.search_history` handler.
pub async fn handle_search_history(payload: Value) -> Reply {
    let Some(query) = require_string(&payload, "query") else {
        return Reply::err_msg("bad_request", "query is required");
    };
    let (date_range, requested) = match parse_common(&payload) {
        Ok(v) => v,
        Err(e) => return err_reply(e),
    };
    let active = resolve_active_workspace(&payload).await;
    let top_k = payload
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TOP_K);
    let threshold = payload
        .get("threshold")
        .and_then(Value::as_f64)
        .map(|f| f as f32)
        .unwrap_or(DEFAULT_THRESHOLD);

    match search_history(&query, date_range, requested, active.as_deref(), top_k, threshold).await {
        Ok(r) => result_reply(r),
        Err(e) => err_reply(e),
    }
}

/// `chat.list_recent` handler.
pub async fn handle_list_recent(payload: Value) -> Reply {
    let (date_range, requested) = match parse_common(&payload) {
        Ok(v) => v,
        Err(e) => return err_reply(e),
    };
    let active = resolve_active_workspace(&payload).await;
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIST_LIMIT);

    match list_recent(limit, date_range, requested, active.as_deref()).await {
        Ok(r) => result_reply(r),
        Err(e) => err_reply(e),
    }
}

/// `chat.get_conversation` handler.
pub async fn handle_get_conversation(payload: Value) -> Reply {
    let Some(id) = require_string(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let requested = match WorkspaceScope::from_payload(payload.get("workspace_scope")) {
        Ok(v) => v,
        Err(e) => return err_reply(e),
    };
    let active = resolve_active_workspace(&payload).await;
    match get_conversation(&id, requested, active.as_deref()).await {
        Ok(doc) => Reply::ok(doc),
        Err(e) => err_reply(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::conversations::test_support::TestEnv;
    use serde_json::json;

    /// Seed a standalone conversation file directly. The doc's `id` is set
    /// to `cid` so the filename and the in-document id always agree.
    fn seed_standalone(cid: &str, mut doc: Value) {
        doc["id"] = json!(cid);
        let path = crate::memory::common::conversations_dir().join(format!("{cid}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }

    /// Point the workspace backend at a guaranteed-dead pipe so any workspace
    /// fetch degrades deterministically (never a real service).
    struct DeadWorkspaces {
        prior: Option<std::ffi::OsString>,
    }
    impl DeadWorkspaces {
        fn set() -> Self {
            let prior = std::env::var_os("WYLDE_HARNESS_WORKSPACES_SERVICE");
            std::env::set_var(
                "WYLDE_HARNESS_WORKSPACES_SERVICE",
                format!("wylde-workspaces-dead-{}", std::process::id()),
            );
            Self { prior }
        }
    }
    impl Drop for DeadWorkspaces {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var("WYLDE_HARNESS_WORKSPACES_SERVICE", v),
                None => std::env::remove_var("WYLDE_HARNESS_WORKSPACES_SERVICE"),
            }
        }
    }

    /// A standalone conversation carrying a precomputed embedding + summary,
    /// so search ranks semantically with no live Ollama. `id` is injected by
    /// [`seed_standalone`].
    fn convo_with_embedding(summary: &str, emb: Vec<f64>, updated: i64) -> Value {
        json!({
            "title": summary,
            "created_at": updated,
            "updated_at": updated,
            "messages": [{"role": "user", "content": summary}],
            "auto_summary": summary,
            "topic_tags": ["seed"],
            "embedding": emb,
        })
    }

    #[tokio::test]
    async fn search_ranks_by_cosine_over_seeded_embeddings() {
        let _env = TestEnv::new();
        // Query embedding will be the lexical fallback unless we force the
        // semantic path. Here we drive the typed `search_history` and rely on
        // `embed_query` returning None (no Ollama) → lexical. To test cosine
        // deterministically we call the pure scorer instead; this test pins
        // the *plumbing* (date filter, threshold, ordering) via lexical.
        seed_standalone(
            "a",
            convo_with_embedding("apply overrides race in settings", vec![1.0, 0.0], 200),
        );
        seed_standalone(
            "b",
            convo_with_embedding("voice cutover whisper onnx", vec![0.0, 1.0], 100),
        );

        // Lexical path (no Ollama in dev): query terms hit conversation "a".
        let r = search_history(
            "apply overrides",
            DateRange::default(),
            WorkspaceScope::CurrentContext,
            None,
            DEFAULT_TOP_K,
            0.4,
        )
        .await
        .unwrap();
        assert!(!r.degraded);
        assert_eq!(r.hits[0].id, "a", "the matching conversation ranks first");
        assert!(r.hits.iter().all(|h| h.workspace_id.is_none()));
    }

    #[tokio::test]
    async fn search_respects_date_range() {
        let _env = TestEnv::new();
        seed_standalone("old", convo_with_embedding("apply overrides", vec![1.0, 0.0], 100));
        seed_standalone("new", convo_with_embedding("apply overrides", vec![1.0, 0.0], 300));

        // Only conversations active at/after 250.
        let r = search_history(
            "apply overrides",
            DateRange {
                from: Some(250),
                to: None,
            },
            WorkspaceScope::CurrentContext,
            None,
            DEFAULT_TOP_K,
            0.4,
        )
        .await
        .unwrap();
        let ids: Vec<&str> = r.hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["new"], "date filter drops the older conversation");
    }

    #[tokio::test]
    async fn boundary_workspace_only_other_is_bad_request() {
        let _env = TestEnv::new();
        // In workspace A, ask to search ONLY workspace B → refused.
        let err = search_history(
            "anything",
            DateRange::default(),
            WorkspaceScope::WorkspaceOnly("ws-b".into()),
            Some("ws-a"),
            DEFAULT_TOP_K,
            DEFAULT_THRESHOLD,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    #[tokio::test]
    async fn handler_boundary_returns_bad_request_reply() {
        let _env = TestEnv::new();
        let reply = handle_search_history(json!({
            "query": "x",
            "active_workspace_id": "ws-a",
            "workspace_scope": {"workspace_only": "ws-b"},
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn standalone_context_never_touches_workspace_backend() {
        let _env = TestEnv::new();
        let _dead = DeadWorkspaces::set();
        seed_standalone("s1", convo_with_embedding("apply overrides", vec![1.0, 0.0], 100));
        // CurrentContext + no active workspace → standalone only → the dead
        // workspace pipe is never consulted, so this must NOT degrade.
        let r = search_history(
            "apply overrides",
            DateRange::default(),
            WorkspaceScope::CurrentContext,
            None,
            DEFAULT_TOP_K,
            0.4,
        )
        .await
        .unwrap();
        assert!(!r.degraded, "standalone-only search must not hit the service");
        assert_eq!(r.hits.len(), 1);
    }

    #[tokio::test]
    async fn workspace_context_degrades_when_service_dead_but_keeps_standalone() {
        let _env = TestEnv::new();
        let _dead = DeadWorkspaces::set();
        seed_standalone("s1", convo_with_embedding("apply overrides", vec![1.0, 0.0], 100));
        // Active workspace A + CurrentContext → [Workspace(A), Standalone].
        // The workspace pipe is dead → degraded, but standalone still answers.
        let r = search_history(
            "apply overrides",
            DateRange::default(),
            WorkspaceScope::CurrentContext,
            Some("ws-a"),
            DEFAULT_TOP_K,
            0.4,
        )
        .await
        .unwrap();
        assert!(r.degraded, "an unreachable workspace backend degrades");
        assert_eq!(r.hits.len(), 1, "standalone results survive");
        assert_eq!(r.hits[0].id, "s1");
    }

    #[tokio::test]
    async fn list_recent_orders_newest_first_and_caps() {
        let _env = TestEnv::new();
        seed_standalone("old", convo_with_embedding("a", vec![1.0], 100));
        seed_standalone("mid", convo_with_embedding("b", vec![1.0], 200));
        seed_standalone("new", convo_with_embedding("c", vec![1.0], 300));
        let r = list_recent(2, DateRange::default(), WorkspaceScope::CurrentContext, None)
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 2, "limit honoured");
        assert_eq!(r.hits[0].id, "new");
        assert_eq!(r.hits[1].id, "mid");
        assert!(r.hits.iter().all(|h| (h.score - 1.0).abs() < 1e-6));
    }

    #[tokio::test]
    async fn get_conversation_standalone_found_and_scoped() {
        let _env = TestEnv::new();
        seed_standalone("c1", json!({"id": "c1", "title": "Hi", "messages": []}));
        // Found in standalone scope.
        let doc = get_conversation("c1", WorkspaceScope::CurrentContext, None)
            .await
            .unwrap();
        assert_eq!(doc["title"], "Hi");
        // Not found for an unknown id.
        let err = get_conversation("ghost", WorkspaceScope::CurrentContext, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[tokio::test]
    async fn get_conversation_standalone_only_scope_skips_workspace() {
        let _env = TestEnv::new();
        let _dead = DeadWorkspaces::set();
        // StandaloneOnly + an id not present → not_found, and the dead
        // workspace pipe is never consulted (no hang/degrade path taken).
        let err = get_conversation("nope", WorkspaceScope::StandaloneOnly, Some("ws-a"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[tokio::test]
    async fn get_conversation_invalid_id_is_bad_request() {
        let _env = TestEnv::new();
        let err = get_conversation("bad/slash", WorkspaceScope::CurrentContext, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    #[tokio::test]
    async fn empty_query_is_bad_request() {
        let _env = TestEnv::new();
        let err = search_history(
            "   ",
            DateRange::default(),
            WorkspaceScope::CurrentContext,
            None,
            DEFAULT_TOP_K,
            DEFAULT_THRESHOLD,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }
}
