//! Pre-LLM **context gather** — the Thought Bubble System's structural
//! retrieval hook (Slice G, Phase 2).
//!
//! **Conceptual path:** `Core/Harness/chat/turn/context_gather`.
//!
//! This is the "AI gets smarter" moment. Before a chat turn calls the LLM,
//! [`gather`] inspects the user's prompt for **symbol** and **anchor**
//! references, pulls their structural context out of the code graph via the
//! Phase-0/1 read API (`workspaces.symbols.find` / `workspaces.symbol_context`
//! / `workspaces.anchors.find_by_token`), folds in the always-on in-process
//! slots (the user profile + the conversation's short-term working memory) and
//! the workspace's rendered prompt block (`workspaces.gather_prompt`, persona +
//! notes + RAG — already wired in Slice 0d), and assembles them into the
//! system prompt. All the data infrastructure from Slices B / F-data / G-data /
//! N-data / D / E finally has a consumer.
//!
//! ## The flow (Build Order §6 Slice G)
//!
//! 1. Tokenize the prompt into candidate identifier tokens.
//! 2. For each token: resolve anchors (`find_by_token`) and **unambiguous**
//!    symbols (`symbols.find` — a single match; ambiguous tokens are skipped,
//!    Phase-4 composer disambiguation handles those). Anchor targets that are
//!    code symbols become symbol references too.
//! 3. Fetch each symbol's 1-hop [`SymbolContext`](crate) structural context.
//! 4. Gather the always-on slots: user profile (in-process), conversation
//!    short-term (in-process), workspace prompt block (service).
//! 5. Build the layered [`ChatContext`].
//! 6. Apply the OI-8 token budget ([`super::token_budget::evict`]).
//! 7. Render named slots ([`super::prompt_assembly::render`]) and hand the
//!    string to the turn driver, which appends it to the base system prompt.
//!
//! ## Graceful degradation (OI-1 / scope v2 §7.5)
//!
//! Wylde Core must work with workspaces disabled or unreachable. Each
//! `workspaces.*` call is best-effort: an unreachable service (transport
//! failure / open breaker) leaves that slot empty and the gather continues.
//! When the active workspace's prompt block is unreachable the turn is flagged
//! [`GatheredContext::degraded`] so the driver prefixes the established Slice-0d
//! notice ([`super::workspace_context::WORKSPACES_UNAVAILABLE_NOTICE`]). The
//! in-process slots (profile / short-term) never depend on the service, so a
//! fully-down workspace still yields a useful prompt.

use std::future::Future;

use serde_json::Value;
use wylde_shared::anchor::Anchor;
use wylde_workspaces_client::{ClientError, WorkspacesClient};

use crate::turn::workspace_context::workspaces_service;
use crate::turn::{prompt_assembly, token_budget};

/// Minimum length of a bare word to be treated as a candidate symbol/anchor
/// token. Filters out articles / operators / one-or-two-char noise that would
/// only waste `symbols.find` round-trips.
const MIN_TOKEN_LEN: usize = 3;

/// Cap on the number of distinct candidate tokens we run lookups for. Bounds
/// the per-turn IPC fan-out so a long paste can't blow the <2s gather budget;
/// the lookups for the retained tokens run concurrently.
const MAX_LOOKUP_TOKENS: usize = 24;

/// Cap on the number of symbol contexts fetched per turn. `symbol_context` is
/// the heaviest read (per-hop budget); five focal symbols is plenty of
/// structural grounding without risking the gather budget.
const MAX_SYMBOL_CONTEXTS: usize = 5;

/// `symbols.find` limit used for the ambiguity check. We ask for **two** so a
/// single returned match proves the token is unambiguous; two or more means
/// it's ambiguous and we skip it (Phase-4 composer disambiguation owns that).
/// (The brief's pseudocode says `limit=1`, but a limit of 1 can't distinguish
/// "exactly one" from "the first of many" — the very test it then performs;
/// `limit=2` expresses the intended "only a single match" rule faithfully.)
const SYMBOL_FIND_LIMIT: u64 = 2;

// ── the gathered, layered context ────────────────────────────────────────

/// One code-symbol's structural context, gathered for a referenced symbol.
/// Holds the focal (kept until the whole block is evicted) and its neighbour
/// lines tagged by hop distance so the token budget can shed deeper hops first
/// (OI-8 tier 4).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SymbolContextBlock {
    /// The focal symbol id (its graph node key).
    pub symbol_id: String,
    /// Rendered focal header + body. Dropped only when the block is.
    pub focal: String,
    /// Neighbour lines (callers / callees / types / siblings), each tagged with
    /// the hop distance it was reached at. Evicted deepest-hop-first.
    pub neighbors: Vec<NeighborLine>,
}

/// A single rendered neighbour line plus the hop distance it sits at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NeighborLine {
    /// 1 for direct neighbours; 2+ for deeper call-graph reaches.
    pub hop: u32,
    /// The pre-rendered line (e.g. ``  calls `bar` (src/bar.rs)``).
    pub text: String,
}

/// One prior-turn message riding into the model's `messages` array
/// (improvement plan B1). NOT part of the rendered system-prompt block —
/// the turn driver interleaves these between the system message and the
/// current user message — but it competes for the same context window,
/// so the token budget counts and evicts it (oldest pair first).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HistoryMessage {
    /// `"user"` or `"assistant"` (system/tool messages never load).
    pub role: String,
    pub content: String,
}

/// The active workspace's structured prompt contribution (improvement plan
/// B6): persona / notes / RAG arrive as separate parts so each maps onto
/// its own eviction tier — one oversized RAG chunk can no longer evict the
/// persona and notes with it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WorkspaceBlock {
    /// Persona text (already B8-capped by the live gather).
    pub persona: Option<String>,
    /// Workspace note snippets, highest-scoring first.
    pub notes: Vec<String>,
    /// RAG snippets, best-first.
    pub rag: Vec<String>,
}

/// A vocabulary anchor the current prompt referenced. Always in the never-drop
/// tier (OI-8 tier 7 — "vocabulary block for currently-referenced anchors").
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AnchorBlock {
    /// The anchor's `{{identifier}}`.
    pub identifier: String,
    /// The rendered definition line.
    pub text: String,
}

/// The layered context assembled for one turn (Plan v2 §6 / §9.1). Slots are
/// rendered to text as they're gathered; [`super::token_budget`] evicts under
/// pressure and [`super::prompt_assembly`] turns what survives into the
/// system-prompt block.
///
/// Tiers 5/6 (pinned/unpinned **bubbles**, a broad **older-anchors** pool)
/// still have no source — every anchor gathered here is a *currently-
/// referenced* one, so it's never-drop. The eviction ladder documents all
/// tiers for when those land.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ChatContext {
    /// The global user profile block (OI-8 tier 7 — never dropped).
    pub user_profile: String,
    /// Current-conversation short-term working memory (OI-8 tier 7 — never
    /// dropped).
    pub conversation_short_term: Vec<String>,
    /// A running summary of the current conversation, read from the
    /// conversation document's `auto_summary` (the chat/search summariser
    /// regenerates it every 5 messages; joined here by improvement plan B2).
    /// OI-8 tier 2 — first to go under pressure.
    pub conversation_summary: Option<String>,
    /// Long-term memory records selected for this turn (improvement plan
    /// B3): similarity-ranked against the user message when an embedding
    /// arrives in budget, else the importance-ranked `core_block`. Rendered
    /// lines, best-first. OI-8 tier ~2.5 — evictable (least-relevant line
    /// first), above the summary, below the workspace block; deliberately
    /// NOT in never-drop tier 7.
    pub long_term: Vec<String>,
    /// Anchors the prompt referenced — the never-drop vocabulary block (tier 7).
    pub vocabulary_anchors: Vec<AnchorBlock>,
    /// Workspace RAG snippets (B6 split). OI-8 tier **1** — the generic
    /// retrieval fallback, first to go; sheds lowest-ranked snippet first.
    pub workspace_rag: Vec<String>,
    /// Workspace note snippets (B6 split). OI-8 tier **3**; sheds
    /// lowest-ranked snippet first.
    pub workspace_notes: Vec<String>,
    /// The workspace persona (B6 split). High tier (~6) — the workspace's
    /// voice; losing it mid-conversation is jarring, so it outlasts every
    /// retrieved slot and drops just before the history window.
    pub workspace_persona: Option<String>,
    /// Structural code-graph context for referenced symbols. OI-8 tier 4
    /// (drop deeper hops first).
    pub symbol_contexts: Vec<SymbolContextBlock>,
    /// Windowed prior-turn conversation history (B1), chronological.
    /// Rides the `messages` array, not the rendered block; counted by the
    /// token budget and evicted oldest-pair-first at tier ~6 — the most
    /// protected evictable slot, because dialogue continuity is not
    /// re-derivable the way retrieved enrichment is.
    pub history: Vec<HistoryMessage>,
}

/// The result of gathering a turn's context.
pub(crate) struct GatheredContext {
    /// The rendered system-prompt slot block to append to the base prompt.
    /// Empty when nothing was gathered (a plain chat turn stays byte-identical
    /// to before).
    pub system_slots: String,
    /// Budget-surviving prior-turn history as wire messages
    /// (`{"role", "content"}`), chronological — the driver splices these
    /// between the system message and the current user message (B1).
    pub history: Vec<Value>,
    /// True when an active workspace was requested but its prompt block was
    /// unreachable — the driver surfaces the inline degraded notice.
    pub degraded: bool,
}

// ── workspace data source (real + mockable) ──────────────────────────────

/// Why a [`WorkspaceSource`] call didn't return data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceStatus {
    /// Service unreachable (transport failure) or breaker open — degrade.
    Unavailable,
    /// The service answered but had nothing / an application error (unknown
    /// workspace, bad request). Not degradation — just no data for this slot.
    Empty,
}

type SourceResult<T> = Result<T, SourceStatus>;

/// The reads the gather flow needs from the workspaces service, abstracted so
/// the orchestration is unit-testable without a live pipe (mirrors the
/// `NeighborhoodSource` pattern in `wylde-workspaces`). [`LiveSource`] wraps the
/// real [`WorkspacesClient`]; tests supply an in-memory mock.
pub(crate) trait WorkspaceSource {
    /// `workspaces.gather_prompt` — the structured persona / notes / RAG
    /// parts (B6).
    fn gather_prompt(
        &self,
        ws: &str,
        user_message: &str,
    ) -> impl Future<Output = SourceResult<Option<WorkspaceBlock>>> + Send;

    /// `workspaces.anchors.find_by_token` — anchors for one token.
    fn find_anchors(
        &self,
        ws: &str,
        token: &str,
    ) -> impl Future<Output = SourceResult<Vec<Anchor>>> + Send;

    /// `workspaces.symbols.find` — the matched **symbol ids** for one token
    /// (capped at [`SYMBOL_FIND_LIMIT`] so the caller can test for ambiguity by
    /// the returned count).
    fn find_symbols(
        &self,
        ws: &str,
        token: &str,
    ) -> impl Future<Output = SourceResult<Vec<String>>> + Send;

    /// `workspaces.symbol_context` — one symbol's raw structural context JSON
    /// (the serialised `SymbolContext`).
    fn symbol_context(
        &self,
        ws: &str,
        symbol_id: &str,
    ) -> impl Future<Output = SourceResult<Value>> + Send;

    /// `workspaces.ignore.list` — the workspace + conversation ignore tiers
    /// merged into one token list (Slice M). Defaults to empty so existing
    /// mocks keep compiling; an unreachable service degrades to "no service
    /// ignores" (an ignore miss must never block a turn).
    fn ignored_tokens(
        &self,
        _ws: &str,
        _conversation_id: &str,
    ) -> impl Future<Output = SourceResult<Vec<String>>> + Send {
        async { Ok(Vec::new()) }
    }
}

/// The production [`WorkspaceSource`] — talks to the real service through the
/// shared [`WorkspacesClient`]. One client (one breaker + cache) is shared
/// across all of a turn's calls.
pub(crate) struct LiveSource {
    client: WorkspacesClient,
}

impl LiveSource {
    fn for_active() -> Self {
        Self {
            client: WorkspacesClient::for_service(workspaces_service()),
        }
    }
}

/// Map a client error to a [`SourceStatus`]: unreachable/breaker → degrade,
/// everything else → just no data.
fn classify(e: &ClientError) -> SourceStatus {
    if e.transport || e.code == "breaker_open" {
        SourceStatus::Unavailable
    } else {
        SourceStatus::Empty
    }
}

impl WorkspaceSource for LiveSource {
    async fn gather_prompt(
        &self,
        ws: &str,
        user_message: &str,
    ) -> SourceResult<Option<WorkspaceBlock>> {
        // Reuse the established Slice-0d workspace-prompt fetch + degrade
        // semantics (its own client + NoRetry policy) rather than re-deriving
        // them — keeps one definition of "is the workspace reachable".
        let prompt = crate::turn::workspace_context::gather(Some(ws), user_message).await;
        if prompt.degraded {
            Err(SourceStatus::Unavailable)
        } else if prompt.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceBlock {
                persona: prompt.persona,
                notes: prompt.notes,
                rag: prompt.rag,
            }))
        }
    }

    async fn find_anchors(&self, ws: &str, token: &str) -> SourceResult<Vec<Anchor>> {
        match self.client.anchors_find_by_token(ws, token).await {
            Ok(v) => Ok(parse_anchors(&v)),
            Err(e) => Err(classify(&e)),
        }
    }

    async fn find_symbols(&self, ws: &str, token: &str) -> SourceResult<Vec<String>> {
        match self
            .client
            .symbols_find(ws, token, Some(SYMBOL_FIND_LIMIT))
            .await
        {
            Ok(v) => Ok(parse_symbol_ids(&v)),
            Err(e) => Err(classify(&e)),
        }
    }

    async fn symbol_context(&self, ws: &str, symbol_id: &str) -> SourceResult<Value> {
        // include_blame: false (Slice L) — the gather block doesn't render
        // blame, and the per-turn hot path shouldn't pay a git subprocess
        // per symbol for unrendered enrichment. Flip when the prompt block
        // starts using recency.
        match self
            .client
            .symbol_context(ws, symbol_id, Some(1), true, false)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => Err(classify(&e)),
        }
    }

    async fn ignored_tokens(&self, ws: &str, conversation_id: &str) -> SourceResult<Vec<String>> {
        match self.client.ignore_list(ws, Some(conversation_id)).await {
            Ok(v) => Ok(parse_ignored_tokens(&v)),
            Err(e) => Err(classify(&e)),
        }
    }
}

/// Parse a `workspaces.ignore.list` reply — both tiers' tokens, merged.
fn parse_ignored_tokens(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for tier in ["workspace", "conversation"] {
        if let Some(arr) = v.get(tier).and_then(Value::as_array) {
            out.extend(
                arr.iter()
                    .filter_map(|e| e.get("token").and_then(Value::as_str))
                    .map(str::to_owned),
            );
        }
    }
    out
}

/// Parse a `{anchors: [...]}` reply into typed [`Anchor`]s, dropping any that
/// don't deserialise (forward-compatible).
fn parse_anchors(v: &Value) -> Vec<Anchor> {
    v.get("anchors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| serde_json::from_value::<Anchor>(a.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `{matches: [{entry: {id, ...}, score}]}` reply into the matched
/// symbol ids, in rank order.
fn parse_symbol_ids(v: &Value) -> Vec<String> {
    v.get("matches")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("entry")
                        .and_then(|e| e.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── public entry point ────────────────────────────────────────────────────

/// Per-message token overrides from the composer (Slices F + M):
/// `excluded` tokens never gather this message (the ✕ exclude);
/// `reactivated` tokens gather even when an ignore tier covers them (the ↺).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TokenOverrides {
    pub excluded: Vec<String>,
    pub reactivated: Vec<String>,
}

impl TokenOverrides {
    /// Parse `excluded_tokens` / `reactivated_tokens` arrays off a
    /// `chat.run_turn` / `chat.start_turn` payload (absent → empty).
    pub fn from_payload(payload: &Value) -> TokenOverrides {
        let list = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        TokenOverrides {
            excluded: list("excluded_tokens"),
            reactivated: list("reactivated_tokens"),
        }
    }
}

/// Gather a turn's context against the live workspaces service.
///
/// `workspace_id` is the active workspace (absent/blank → no workspace reads;
/// only the in-process profile + short-term slots are gathered, so a plain
/// chat turn with an empty profile stays byte-identical to before).
/// `conversation_id` keys the short-term working-memory read.
/// `slot_budget` is the OI-8 eviction ceiling for the rendered slots — the
/// turn driver derives it from the model's effective `num_ctx`
/// ([`super::chat_options::slot_budget`], improvement plan B5).
pub(crate) async fn gather(
    workspace_id: Option<&str>,
    user_message: &str,
    conversation_id: &str,
    overrides: &TokenOverrides,
    slot_budget: usize,
) -> GatheredContext {
    gather_with(
        &LiveSource::for_active(),
        workspace_id,
        user_message,
        conversation_id,
        overrides,
        slot_budget,
    )
    .await
}

/// The source-injectable core of [`gather`] (the real path passes
/// [`LiveSource`]; tests pass a mock).
pub(crate) async fn gather_with<S: WorkspaceSource + Sync>(
    source: &S,
    workspace_id: Option<&str>,
    user_message: &str,
    conversation_id: &str,
    overrides: &TokenOverrides,
    slot_budget: usize,
) -> GatheredContext {
    // Always-on, in-process slots — never depend on the workspaces service.
    let mut ctx = ChatContext {
        user_profile: crate::user_profile::store::read().profile.to_prompt_block(),
        conversation_short_term: read_short_term(conversation_id),
        // B2: the auto-summary pipeline (chat/search/summary, regenerated
        // every 5 messages) finally feeds the tier-2 slot it was built for.
        conversation_summary: crate::chat::search::summary::auto_summary_for(conversation_id),
        // B3: the long-term store finally reaches the prompt without the
        // model having to think of calling memory.search.
        long_term: gather_long_term(user_message).await,
        // B1: the model can finally see the previous turns.
        history: load_history(conversation_id, user_message, slot_budget),
        ..ChatContext::default()
    };
    let mut degraded = false;

    if let Some(ws) = workspace_id.map(str::trim).filter(|s| !s.is_empty()) {
        // The workspace prompt parts (persona / notes / RAG — B6 split).
        // An unreachable service here is the degrade signal (Slice 0d
        // semantics).
        match source.gather_prompt(ws, user_message).await {
            Ok(Some(block)) => {
                ctx.workspace_persona = block.persona;
                ctx.workspace_notes = block.notes;
                ctx.workspace_rag = block.rag;
            }
            Ok(None) => {}
            Err(SourceStatus::Unavailable) => degraded = true,
            Err(SourceStatus::Empty) => {}
        }

        let mut tokens = candidate_tokens(user_message);

        // Slice M (Plan §5.8): durable ignores (global + workspace +
        // conversation tiers) drop a token unless this message reactivated
        // it (↺); per-message excludes (✕, Slice F) always drop. Tier reads
        // are fail-soft — an unreachable ignore list never blocks a turn.
        let global_ignores: Vec<String> = crate::chat::ignore::store::load()
            .into_iter()
            .map(|e| e.token)
            .collect();
        let service_ignores = source
            .ignored_tokens(ws, conversation_id)
            .await
            .unwrap_or_default();
        tokens.retain(|t| {
            if overrides.excluded.iter().any(|x| x == t) {
                return false;
            }
            let ignored =
                global_ignores.iter().any(|x| x == t) || service_ignores.iter().any(|x| x == t);
            !ignored || overrides.reactivated.iter().any(|x| x == t)
        });

        // Anchors + the symbol ids their code-symbol targets point at.
        let (anchors, anchor_symbol_ids) = gather_anchors(source, ws, &tokens).await;
        ctx.vocabulary_anchors = anchors;

        // Unambiguous symbol references from the prompt's bare tokens, plus the
        // anchor-target symbols, deduped and capped.
        let mut symbol_ids = anchor_symbol_ids;
        symbol_ids.extend(gather_unambiguous_symbols(source, ws, &tokens).await);
        dedupe_preserving_order(&mut symbol_ids);
        symbol_ids.truncate(MAX_SYMBOL_CONTEXTS);

        ctx.symbol_contexts = gather_symbol_contexts(source, ws, &symbol_ids).await;
    }

    // Trim to the model's budget (OI-8), then render the named slots.
    token_budget::evict(&mut ctx, slot_budget);
    let system_slots = prompt_assembly::render(&ctx);
    let history = ctx
        .history
        .iter()
        .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
        .collect();

    GatheredContext {
        system_slots,
        history,
        degraded,
    }
}

// ── gather helpers ──────────────────────────────────────────────────────

/// Cap on the number of prior-turn messages loaded per turn (B1) — 20
/// messages ≈ 10 exchanges of lookback before the auto-summary (B2)
/// takes over for older context.
const HISTORY_MAX_MESSAGES: usize = 20;

/// Absolute token ceiling (estimated) on loaded history. The effective
/// load budget is `min(this, slot_budget / 2)` so history can never
/// swamp a small model's window before eviction even runs.
const HISTORY_MAX_TOKENS: usize = 4_000;

/// Load the windowed conversation history (improvement plan B1):
/// newest-first within the sub-budget, returned chronological.
///
/// * Only `user` / `assistant` messages with non-empty content load —
///   system messages (defensive; the store shouldn't hold any) and tool
///   rows are skipped: tool exchanges are intra-turn scaffolding, and
///   replaying them confuses small models.
/// * If the newest stored message is a `user` message identical to the
///   current one, it is skipped — some callers persist the user message
///   before driving the turn, and the current exchange must never
///   duplicate (the plan's "never the current exchange").
/// * Fail-soft: unknown id / unreadable doc ⇒ no history.
fn load_history(
    conversation_id: &str,
    user_message: &str,
    slot_budget: usize,
) -> Vec<HistoryMessage> {
    if conversation_id.trim().is_empty() {
        return Vec::new();
    }
    let Ok(doc) = crate::memory::conversations::store::read_conversation(conversation_id) else {
        return Vec::new();
    };
    let Some(messages) = doc.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };

    let token_budget_cap = HISTORY_MAX_TOKENS.min(slot_budget / 2);
    let mut picked: Vec<HistoryMessage> = Vec::new();
    let mut tokens = 0usize;
    let mut newest = true;
    for m in messages.iter().rev() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = m
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if content.is_empty() || (role != "user" && role != "assistant") {
            continue;
        }
        if newest {
            newest = false;
            if role == "user" && content == user_message.trim() {
                continue; // the already-persisted current exchange
            }
        }
        let cost = token_budget::estimate_tokens(content) + token_budget::HISTORY_MSG_OVERHEAD;
        if picked.len() >= HISTORY_MAX_MESSAGES || tokens + cost > token_budget_cap {
            break;
        }
        tokens += cost;
        picked.push(HistoryMessage {
            role: role.to_owned(),
            content: content.to_owned(),
        });
    }
    picked.reverse(); // gathered newest-first; the wire wants chronological
    picked
}

/// How many long-term records ride each turn (B3 — the `core_block`
/// default).
const LONG_TERM_LIMIT: usize = 5;

/// Budget for embedding the user message for similarity ranking. Mirrors
/// the workspaces notes-query embed bound: a slow/down embedder degrades
/// the ranking to importance/recency, never delays the turn past this.
const LONG_TERM_EMBED_BUDGET: std::time::Duration = std::time::Duration::from_millis(1200);

/// Select this turn's long-term memory lines (improvement plan B3).
///
/// Selection: when the store is non-empty, embed the user message (bounded
/// by [`LONG_TERM_EMBED_BUDGET`]) and take the combined-score `search`
/// hits (similarity + importance + recency — the scoring formula finally
/// runs for injection, not just the `memory.search` verb), topped up with
/// `core_block` records the search missed. Embedder down/over-budget ⇒
/// pure `core_block` (importance desc). Every injected record's
/// `last_used_at` is bumped so the recency term breathes.
async fn gather_long_term(user_message: &str) -> Vec<String> {
    use crate::memory::long_term as entries;

    // The store-empty fast path also keeps plain turns embed-free.
    let core = entries::core_block(Some(LONG_TERM_LIMIT));
    if core.is_empty() {
        return Vec::new();
    }

    let mut selected = core;
    let embed = tokio::time::timeout(
        LONG_TERM_EMBED_BUDGET,
        crate::memory::embeddings::embed_one(user_message.to_owned()),
    )
    .await;
    if let Ok(Ok(vector)) = embed {
        let hits = entries::search(vector, LONG_TERM_LIMIT, None);
        if !hits.is_empty() {
            let mut merged = Vec::with_capacity(LONG_TERM_LIMIT);
            for h in &hits {
                if let Some(r) = entries::get(&h.id) {
                    merged.push(r);
                }
            }
            for r in selected {
                if merged.len() >= LONG_TERM_LIMIT {
                    break;
                }
                if !merged.iter().any(|m| m.id == r.id) {
                    merged.push(r);
                }
            }
            merged.truncate(LONG_TERM_LIMIT);
            selected = merged;
        }
    }

    let ids: Vec<String> = selected.iter().map(|r| r.id.clone()).collect();
    entries::touch_all(&ids);
    selected
        .iter()
        .map(|r| format!("- {}", r.body.trim()))
        .collect()
}

/// Injection cap on working-memory entries (improvement plan B8). The
/// short-term slot is OI-8 tier 7 (never evicted), and the store grows
/// without bound over a long conversation — uncapped, the never-drop floor
/// can permanently exceed a small model's whole budget, forcing eviction to
/// delete *everything else* every turn while still overshooting. Newest
/// entries win; older ones are omitted from the PROMPT only (the store
/// keeps them, and idle-time consolidation still reads the full list).
const WORKING_MEMORY_MAX_ENTRIES: usize = 40;

/// Token ceiling (estimated) on the same slot — guards against few-but-huge
/// entries the entry cap can't catch.
const WORKING_MEMORY_MAX_TOKENS: usize = 2_000;

/// Read the conversation's short-term working memory as rendered lines,
/// newest-first-capped per B8. In-process and fail-soft: an invalid id /
/// read error yields no lines.
fn read_short_term(conversation_id: &str) -> Vec<String> {
    if conversation_id.trim().is_empty() {
        return Vec::new();
    }
    let lines = crate::memory::short_term::store::get_working_memory(conversation_id)
        .unwrap_or_default()
        .iter()
        .filter_map(render_working_memory_entry)
        .collect();
    cap_short_term(lines)
}

/// Apply the B8 caps: keep the newest [`WORKING_MEMORY_MAX_ENTRIES`] lines
/// within [`WORKING_MEMORY_MAX_TOKENS`] (always at least the newest line),
/// prefixing a visible omission marker when anything was dropped.
fn cap_short_term(lines: Vec<String>) -> Vec<String> {
    let total = lines.len();
    let mut kept = lines;
    if kept.len() > WORKING_MEMORY_MAX_ENTRIES {
        kept = kept.split_off(kept.len() - WORKING_MEMORY_MAX_ENTRIES);
    }
    let token_sum = |ls: &[String]| {
        ls.iter()
            .map(|l| token_budget::estimate_tokens(l))
            .sum::<usize>()
    };
    let mut start = 0usize;
    while start + 1 < kept.len() && token_sum(&kept[start..]) > WORKING_MEMORY_MAX_TOKENS {
        start += 1;
    }
    let mut out: Vec<String> = kept.split_off(start);
    let omitted = total - out.len();
    if omitted > 0 {
        out.insert(
            0,
            format!("- [{omitted} older working-memory entries omitted (injection cap)]"),
        );
    }
    out
}

/// Render one short-term working-memory entry to a compact line. Object entries
/// expose their `data`/`text`/`content` field (the common shapes); anything
/// else falls back to a compact JSON dump. Empty entries are skipped.
fn render_working_memory_entry(entry: &Value) -> Option<String> {
    let text = entry
        .get("data")
        .or_else(|| entry.get("text"))
        .or_else(|| entry.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if entry.is_string() {
                entry.as_str().unwrap_or("").to_owned()
            } else {
                serde_json::to_string(entry).unwrap_or_default()
            }
        });
    let text = text.trim();
    (!text.is_empty()).then(|| format!("- {text}"))
}

/// Tokenize the prompt into distinct candidate identifier tokens: bare words of
/// at least [`MIN_TOKEN_LEN`] identifier bytes plus every `{{anchor}}` token,
/// in source order, deduped, capped at [`MAX_LOOKUP_TOKENS`].
fn candidate_tokens(user_message: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Bare identifier words.
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= MIN_TOKEN_LEN && !out.iter().any(|t| t == cur) {
            out.push(cur.clone());
        }
        cur.clear();
    };
    for ch in user_message.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);

    // Explicit `{{anchor}}` tokens (may be shorter than MIN_TOKEN_LEN; the user
    // typed them deliberately).
    for span in wylde_shared::anchor_tokenizer::parse_anchors(user_message) {
        if !out.contains(&span.identifier) {
            out.push(span.identifier);
        }
    }

    out.truncate(MAX_LOOKUP_TOKENS);
    out
}

/// Resolve anchors for every candidate token concurrently. Returns the deduped
/// anchor blocks (the never-drop vocabulary block) plus the symbol ids their
/// code-symbol targets reference (which become symbol-context lookups too).
async fn gather_anchors<S: WorkspaceSource + Sync>(
    source: &S,
    ws: &str,
    tokens: &[String],
) -> (Vec<AnchorBlock>, Vec<String>) {
    let results =
        futures::future::join_all(tokens.iter().map(|t| source.find_anchors(ws, t))).await;

    let mut blocks: Vec<AnchorBlock> = Vec::new();
    let mut symbol_ids: Vec<String> = Vec::new();
    for anchors in results.into_iter().flatten().flatten() {
        // Dedupe by identifier across tokens.
        if blocks.iter().any(|b| b.identifier == anchors.identifier) {
            continue;
        }
        if let Some(sym) = anchors.target.symbol_id() {
            symbol_ids.push(sym.to_owned());
        }
        blocks.push(AnchorBlock {
            identifier: anchors.identifier.clone(),
            text: render_anchor(&anchors),
        });
    }
    (blocks, symbol_ids)
}

/// Run `symbols.find` for every token concurrently and keep only the
/// **unambiguous** ones (exactly one match). Ambiguous tokens are skipped —
/// Phase-4 composer disambiguation resolves those.
async fn gather_unambiguous_symbols<S: WorkspaceSource + Sync>(
    source: &S,
    ws: &str,
    tokens: &[String],
) -> Vec<String> {
    let results =
        futures::future::join_all(tokens.iter().map(|t| source.find_symbols(ws, t))).await;

    results
        .into_iter()
        .filter_map(Result::ok)
        .filter(|ids| ids.len() == 1)
        .filter_map(|mut ids| ids.pop())
        .collect()
}

/// Fetch each symbol's 1-hop structural context concurrently and render it into
/// a [`SymbolContextBlock`]. Unreachable / empty lookups are simply omitted.
async fn gather_symbol_contexts<S: WorkspaceSource + Sync>(
    source: &S,
    ws: &str,
    symbol_ids: &[String],
) -> Vec<SymbolContextBlock> {
    let results =
        futures::future::join_all(symbol_ids.iter().map(|id| source.symbol_context(ws, id))).await;

    results
        .into_iter()
        .zip(symbol_ids.iter())
        .filter_map(|(res, id)| res.ok().map(|v| block_from_symbol_context(id, &v)))
        .collect()
}

// ── rendering raw replies into context blocks ─────────────────────────────

/// Render one anchor's vocabulary line: ``{{identifier}} — <description>``,
/// noting its code target when present.
fn render_anchor(a: &Anchor) -> String {
    let mut line = format!("{{{{{}}}}} — {}", a.identifier, a.description.trim());
    if let Some(sym) = a.target.symbol_id() {
        line.push_str(&format!(" (code symbol `{sym}`)"));
    }
    line
}

/// Turn a raw `SymbolContext` reply into a [`SymbolContextBlock`]: the focal
/// header + body, then one neighbour line per caller / callee / type / sibling,
/// each carrying its hop distance for OI-8 deeper-hop eviction.
fn block_from_symbol_context(symbol_id: &str, v: &Value) -> SymbolContextBlock {
    let sym = v.get("symbol");
    let name = sym
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(symbol_id);
    let file = sym
        .and_then(|s| s.get("file"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let line = sym
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let body = sym
        .and_then(|s| s.get("body"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();

    let mut focal = if line > 0 {
        format!("Symbol `{name}` — {file}:{line}")
    } else if file.is_empty() {
        format!("Symbol `{name}`")
    } else {
        format!("Symbol `{name}` — {file}")
    };
    if !body.is_empty() {
        focal.push('\n');
        focal.push_str(body);
    }

    let mut neighbors = Vec::new();
    collect_neighbors(v, "callers", "called by", &mut neighbors);
    collect_neighbors(v, "callees", "calls", &mut neighbors);
    collect_neighbors(v, "types_used", "uses type", &mut neighbors);
    collect_neighbors(v, "siblings", "sibling", &mut neighbors);

    SymbolContextBlock {
        symbol_id: symbol_id.to_owned(),
        focal,
        neighbors,
    }
}

/// Append a rendered neighbour line for each entry under `key`, labelled with
/// `verb` and tagged with the entry's `hop_distance` (default 1).
fn collect_neighbors(v: &Value, key: &str, verb: &str, out: &mut Vec<NeighborLine>) {
    let Some(arr) = v.get(key).and_then(Value::as_array) else {
        return;
    };
    for e in arr {
        let Some(name) = e.get("name").and_then(Value::as_str) else {
            continue;
        };
        let hop = e.get("hop_distance").and_then(Value::as_u64).unwrap_or(1) as u32;
        let file = e.get("file").and_then(Value::as_str).unwrap_or("");
        let text = if file.is_empty() {
            format!("  {verb} `{name}`")
        } else {
            format!("  {verb} `{name}` ({file})")
        };
        out.push(NeighborLine { hop, text });
    }
}

/// Drop later duplicates, preserving first-seen order.
fn dedupe_preserving_order(v: &mut Vec<String>) {
    let mut seen: Vec<String> = Vec::new();
    v.retain(|s| {
        if seen.iter().any(|x| x == s) {
            false
        } else {
            seen.push(s.clone());
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use wylde_shared::anchor::{AnchorKind, AnchorScope, AnchorTarget};

    // ── candidate tokenization ──────────────────────────────────────────

    #[test]
    fn tokenizes_bare_words_and_anchors_deduped() {
        let toks = candidate_tokens("explain set_active and {{the_pipe}} vs ab set_active");
        // `ab` (len 2) dropped; `set_active` deduped; anchor `the_pipe` kept.
        assert!(toks.contains(&"set_active".to_owned()));
        assert!(toks.contains(&"the_pipe".to_owned()));
        assert!(toks.contains(&"explain".to_owned()));
        assert!(!toks.contains(&"ab".to_owned()));
        assert_eq!(
            toks.iter().filter(|t| *t == "set_active").count(),
            1,
            "deduped"
        );
    }

    #[test]
    fn token_cap_is_enforced() {
        let many: String = (0..50).map(|i| format!("word{i:03} ")).collect();
        assert_eq!(candidate_tokens(&many).len(), MAX_LOOKUP_TOKENS);
    }

    // ── a mock workspace source ─────────────────────────────────────────

    #[derive(Default)]
    struct MockSource {
        prompt: Option<WorkspaceBlock>,
        prompt_unavailable: bool,
        /// token → anchors
        anchors: HashMap<String, Vec<Anchor>>,
        /// token → matched symbol ids (len drives ambiguity)
        symbols: HashMap<String, Vec<String>>,
        /// symbol_id → raw SymbolContext JSON
        contexts: HashMap<String, Value>,
        /// when set, every symbol_context call reports Unavailable
        contexts_unavailable: bool,
        /// service-side ignored tokens (workspace + conversation tiers,
        /// Slice M)
        ignored: Vec<String>,
    }

    impl WorkspaceSource for MockSource {
        async fn gather_prompt(&self, _ws: &str, _m: &str) -> SourceResult<Option<WorkspaceBlock>> {
            if self.prompt_unavailable {
                Err(SourceStatus::Unavailable)
            } else {
                Ok(self.prompt.clone())
            }
        }
        async fn find_anchors(&self, _ws: &str, token: &str) -> SourceResult<Vec<Anchor>> {
            Ok(self.anchors.get(token).cloned().unwrap_or_default())
        }
        async fn find_symbols(&self, _ws: &str, token: &str) -> SourceResult<Vec<String>> {
            Ok(self.symbols.get(token).cloned().unwrap_or_default())
        }
        async fn symbol_context(&self, _ws: &str, symbol_id: &str) -> SourceResult<Value> {
            if self.contexts_unavailable {
                return Err(SourceStatus::Unavailable);
            }
            self.contexts
                .get(symbol_id)
                .cloned()
                .ok_or(SourceStatus::Empty)
        }
        async fn ignored_tokens(
            &self,
            _ws: &str,
            _conversation_id: &str,
        ) -> SourceResult<Vec<String>> {
            Ok(self.ignored.clone())
        }
    }

    fn ctx_json(name: &str, body: &str, callees: &[(&str, u32)]) -> Value {
        json!({
            "symbol": {"id": name, "name": name, "kind": "Function",
                       "file": format!("src/{name}.rs"), "line": 10, "body": body},
            "callers": [],
            "callees": callees.iter().map(|(n, h)| json!({
                "id": n, "name": n, "kind": "Function",
                "file": format!("src/{n}.rs"), "hop_distance": h, "rel_type": "CALLS"
            })).collect::<Vec<_>>(),
            "types_used": [],
            "siblings": [],
            "hops_traversed": 1,
            "took_ms": 1
        })
    }

    // ── token overrides + ignore tiers (Slices F + M) ───────────────────

    #[test]
    fn token_overrides_parse_from_payload() {
        let o = TokenOverrides::from_payload(&json!({
            "excluded_tokens": ["alpha", "  ", "beta"],
            "reactivated_tokens": ["gamma"]
        }));
        assert_eq!(o.excluded, vec!["alpha", "beta"]);
        assert_eq!(o.reactivated, vec!["gamma"]);
        assert_eq!(
            TokenOverrides::from_payload(&json!({})),
            TokenOverrides::default()
        );
    }

    #[tokio::test]
    async fn ignored_tokens_skip_gather_unless_reactivated() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        src.symbols.insert("foo".into(), vec!["foo".into()]);
        src.contexts
            .insert("foo".into(), ctx_json("foo", "fn foo() {}", &[]));
        src.ignored = vec!["foo".into()];

        // An ignored token gathers nothing (Plan §5.8: default-inactive).
        let out = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.system_slots.contains("Symbol `foo`"));

        // ↺ reactivation brings it back for this message only.
        let re = TokenOverrides {
            reactivated: vec!["foo".into()],
            ..Default::default()
        };
        let out = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &re,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(out.system_slots.contains("Symbol `foo`"));
    }

    #[tokio::test]
    async fn per_message_excludes_always_drop() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        src.symbols.insert("foo".into(), vec!["foo".into()]);
        src.contexts
            .insert("foo".into(), ctx_json("foo", "fn foo() {}", &[]));

        // The ✕ exclude beats everything — even a (nonsensical) simultaneous
        // reactivation.
        let ex = TokenOverrides {
            excluded: vec!["foo".into()],
            reactivated: vec!["foo".into()],
        };
        let out = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &ex,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.system_slots.contains("Symbol `foo`"));
    }

    // ── B2: auto-summary → tier-2 slot ──────────────────────────────────

    #[tokio::test]
    async fn auto_summary_feeds_the_tier2_slot() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut doc = serde_json::Map::new();
        doc.insert("id".into(), json!("conv-b2"));
        doc.insert("messages".into(), json!([]));
        doc.insert(
            "auto_summary".into(),
            json!("They discussed the gather flow."),
        );
        crate::memory::conversations::store::save_conversation(&doc).unwrap();

        let src = MockSource::default();
        let out = gather_with(
            &src,
            None,
            "hello",
            "conv-b2",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            out.system_slots.contains("### Conversation summary"),
            "slot present: {}",
            out.system_slots
        );
        assert!(out.system_slots.contains("They discussed the gather flow."));

        // Unknown conversation → no summary slot (and blank stays blank).
        let out = gather_with(
            &src,
            None,
            "hello",
            "conv-none",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.system_slots.contains("### Conversation summary"));
    }

    // ── B1: windowed conversation history ───────────────────────────────

    #[tokio::test]
    async fn history_loads_windowed_strips_system_and_skips_current_exchange() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut doc = serde_json::Map::new();
        doc.insert("id".into(), json!("conv-b1"));
        doc.insert(
            "messages".into(),
            json!([
                {"role": "system", "content": "should never load"},
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "first answer"},
                {"role": "tool", "content": "{\"result\": 1}"},
                {"role": "user", "content": "second question"},
                {"role": "assistant", "content": "second answer"},
                // The GUI persisted the current user message before the turn.
                {"role": "user", "content": "what about the second one?"},
            ]),
        );
        crate::memory::conversations::store::save_conversation(&doc).unwrap();

        let src = MockSource::default();
        let out = gather_with(
            &src,
            None,
            "what about the second one?",
            "conv-b1",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        let contents: Vec<&str> = out
            .history
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            contents,
            vec![
                "first question",
                "first answer",
                "second question",
                "second answer"
            ],
            "chronological, system/tool stripped, current exchange skipped"
        );
        // History rides the messages array, never the rendered block.
        assert!(!out.system_slots.contains("first question"));
    }

    #[test]
    fn history_load_respects_message_and_token_caps() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        // 30 exchanges — far past the 20-message cap.
        let msgs: Vec<Value> = (0..30)
            .flat_map(|i| {
                vec![
                    json!({"role": "user", "content": format!("q{i:02}")}),
                    json!({"role": "assistant", "content": format!("a{i:02}")}),
                ]
            })
            .collect();
        let mut doc = serde_json::Map::new();
        doc.insert("id".into(), json!("conv-b1-cap"));
        doc.insert("messages".into(), json!(msgs));
        crate::memory::conversations::store::save_conversation(&doc).unwrap();

        let picked = load_history(
            "conv-b1-cap",
            "a new ask",
            token_budget::DEFAULT_TOKEN_BUDGET,
        );
        assert_eq!(picked.len(), HISTORY_MAX_MESSAGES, "newest 20 only");
        assert_eq!(picked.last().unwrap().content, "a29", "newest survives");
        assert_eq!(picked[0].content, "q20", "window starts 10 exchanges back");

        // A tiny slot budget halves into the history cap.
        let picked = load_history("conv-b1-cap", "a new ask", 40);
        let total: usize = picked
            .iter()
            .map(|m| token_budget::estimate_tokens(&m.content) + token_budget::HISTORY_MSG_OVERHEAD)
            .sum();
        assert!(total <= 20, "history fits half the slot budget: {total}");
        assert!(!picked.is_empty(), "the newest message still loads");
    }

    // ── B3: long-term memory injection ──────────────────────────────────

    #[tokio::test]
    async fn long_term_core_block_is_injected_capped_and_touched() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::long_term as entries;

        // 7 records, importance 1..=7 — only the top 5 ride the prompt.
        let mut saved = Vec::new();
        for i in 0..7 {
            let r = entries::save(
                &format!("long-term fact {i}"),
                "test",
                Some((i + 1) as f64),
                Vec::new(),
                None,
            )
            .expect("save record");
            saved.push(r);
        }
        // Let the clock advance so the injection-touch is observable.
        std::thread::sleep(std::time::Duration::from_millis(25));

        let src = MockSource::default();
        let out = gather_with(
            &src,
            None,
            "what do you remember",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            out.system_slots.contains("### Long-term memory"),
            "slot present: {}",
            out.system_slots
        );
        // Top importance (7) in; bottom (1) out.
        assert!(out.system_slots.contains("long-term fact 6"));
        assert!(!out.system_slots.contains("long-term fact 0"));
        assert_eq!(out.system_slots.matches("long-term fact").count(), 5);

        // Injection bumped last_used_at on an injected record (recency
        // term breathes). The selective half (excluded records stay
        // untouched) is pinned synchronously in
        // `entries::tests::touch_all_bumps_only_named_ids` — asserting it
        // here proved flaky: the gather's bounded-embed window (~1.2 s)
        // is long enough for a leaked background task from an earlier
        // test to brush the shared store.
        let injected = entries::get(&saved[6].id).unwrap();
        assert!(
            injected.last_used_at > saved[6].last_used_at,
            "injected record touched"
        );
    }

    #[tokio::test]
    async fn empty_long_term_store_adds_no_slot() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let src = MockSource::default();
        let out = gather_with(
            &src,
            None,
            "hello there",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.system_slots.contains("### Long-term memory"));
    }

    // ── B8 short-term injection caps ─────────────────────────────────────

    #[test]
    fn short_term_cap_keeps_newest_entries_with_marker() {
        let lines: Vec<String> = (0..50).map(|i| format!("- entry {i:02}")).collect();
        let capped = cap_short_term(lines);
        // 40 newest kept + 1 omission marker.
        assert_eq!(capped.len(), WORKING_MEMORY_MAX_ENTRIES + 1);
        assert!(
            capped[0].contains("10 older working-memory entries omitted"),
            "marker: {}",
            capped[0]
        );
        assert_eq!(capped[1], "- entry 10", "oldest survivor");
        assert_eq!(capped.last().unwrap(), "- entry 49", "newest survives");
    }

    #[test]
    fn short_term_token_ceiling_drops_oldest_but_keeps_newest() {
        // Three entries of ~1500 estimated tokens each (6000 chars) — over
        // the 2000-token ceiling, so only the newest survives.
        let lines: Vec<String> = (0..3).map(|i| format!("{i}{}", "x".repeat(6000))).collect();
        let capped = cap_short_term(lines);
        assert_eq!(capped.len(), 2, "marker + newest: {capped:?}");
        assert!(capped[0].contains("2 older working-memory entries omitted"));
        assert!(capped[1].starts_with('2'), "newest entry survives");
    }

    #[test]
    fn short_term_under_caps_is_untouched() {
        let lines: Vec<String> = (0..5).map(|i| format!("- entry {i}")).collect();
        assert_eq!(cap_short_term(lines.clone()), lines);
        assert!(cap_short_term(Vec::new()).is_empty());
    }

    // ── symbol detection ────────────────────────────────────────────────

    #[tokio::test]
    async fn unambiguous_symbol_is_queued_and_injected() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        src.symbols.insert("foo".into(), vec!["foo".into()]); // single match → unambiguous
        src.contexts.insert(
            "foo".into(),
            ctx_json("foo", "fn foo() { bar() }", &[("bar", 1)]),
        );

        let out = gather_with(
            &src,
            Some("ws"),
            "please explain foo",
            "conv-x",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            out.system_slots.contains("Code graph context"),
            "slots: {}",
            out.system_slots
        );
        assert!(out.system_slots.contains("Symbol `foo`"));
        assert!(out.system_slots.contains("fn foo()"));
        assert!(out.system_slots.contains("calls `bar`"));
        assert!(!out.degraded);
    }

    #[tokio::test]
    async fn ambiguous_symbol_is_skipped() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        // 3 matches (capped to 2 by find limit) → ambiguous → skipped.
        src.symbols
            .insert("set_active".into(), vec!["a".into(), "b".into()]);

        let out = gather_with(
            &src,
            Some("ws"),
            "what does set_active do",
            "conv-x",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            !out.system_slots.contains("Code graph context"),
            "ambiguous token must add no symbol context: {}",
            out.system_slots
        );
    }

    #[tokio::test]
    async fn anchor_target_pulls_symbol_context() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        let anchor = Anchor::new(
            "the_fn",
            AnchorKind::CodeSymbol,
            AnchorTarget::CodeSymbol {
                symbol_id: "run_it".into(),
            },
            AnchorScope::Workspace {
                workspace_id: "ws".into(),
            },
            "the entry point",
        );
        src.anchors.insert("the_fn".into(), vec![anchor]);
        src.contexts
            .insert("run_it".into(), ctx_json("run_it", "fn run_it() {}", &[]));

        let out = gather_with(
            &src,
            Some("ws"),
            "look at {{the_fn}}",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        // The anchor shows in the vocabulary block...
        assert!(out.system_slots.contains("{{the_fn}}"));
        // ...and its code target pulled a symbol context.
        assert!(out.system_slots.contains("Symbol `run_it`"));
    }

    // ── graceful degradation ────────────────────────────────────────────

    #[tokio::test]
    async fn symbol_context_unavailable_degrades_to_empty_block() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        src.symbols.insert("foo".into(), vec!["foo".into()]);
        src.contexts_unavailable = true; // symbol_context Broken

        let out = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        // No symbol context block, but the gather still produced (empty) slots
        // and did not panic. The prompt block (gather_prompt) was reachable, so
        // a partial-failure does NOT flag degraded.
        assert!(!out.system_slots.contains("Code graph context"));
        assert!(!out.degraded);
    }

    #[tokio::test]
    async fn full_workspace_down_keeps_profile_and_flags_degraded() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        // Seed a profile so the in-process slot is non-empty.
        crate::user_profile::store::with_store(|s| {
            s.profile.name = Some("Aaron".into());
        })
        .unwrap();

        let src = MockSource {
            prompt_unavailable: true,
            contexts_unavailable: true,
            ..MockSource::default()
        };

        let out = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(out.degraded, "an unreachable workspace prompt must degrade");
        assert!(
            out.system_slots.contains("Aaron"),
            "the in-process profile survives a full workspace outage: {}",
            out.system_slots
        );
        assert!(!out.system_slots.contains("Code graph context"));
    }

    // ── A/B: hook adds the symbol context vs a plain turn ───────────────

    #[tokio::test]
    async fn ab_hook_on_vs_off() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        src.symbols.insert("foo".into(), vec!["foo".into()]);
        src.contexts
            .insert("foo".into(), ctx_json("foo", "fn foo() {}", &[]));

        // ON: active workspace → symbol context present.
        let on = gather_with(
            &src,
            Some("ws"),
            "explain foo",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(on.system_slots.contains("Symbol `foo`"));

        // OFF: no workspace → no symbol context (and an empty profile → empty
        // slots, byte-identical to a plain turn).
        let off = gather_with(
            &src,
            None,
            "explain foo",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!off.system_slots.contains("Symbol `foo`"));
        assert!(off.system_slots.is_empty());
    }

    // ── performance: full gather flow under 2s ──────────────────────────

    #[tokio::test]
    async fn full_gather_flow_under_2s() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource {
            prompt: Some(WorkspaceBlock {
                persona: Some("Be helpful.".into()),
                notes: vec!["be concise".into()],
                rag: Vec::new(),
            }),
            ..MockSource::default()
        };
        // 10 unambiguous symbols, 5 with contexts (cap).
        for i in 0..10 {
            let name = format!("sym{i:02}");
            src.symbols.insert(name.clone(), vec![name.clone()]);
            src.contexts
                .insert(name.clone(), ctx_json(&name, "fn body() {}", &[("dep", 1)]));
        }
        let prompt = "explain sym00 sym01 sym02 sym03 sym04 sym05 sym06 sym07 sym08 sym09";

        let start = std::time::Instant::now();
        let out = gather_with(
            &src,
            Some("ws"),
            prompt,
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "gather took {elapsed:?}"
        );
        // Capped at MAX_SYMBOL_CONTEXTS focal symbols.
        assert_eq!(
            out.system_slots.matches("Symbol `").count(),
            MAX_SYMBOL_CONTEXTS
        );
    }

    // ── reply parsing ───────────────────────────────────────────────────

    #[test]
    fn alias_matched_anchor_renders_canonical_identifier_only() {
        // Slice N-data-aliases / deliverable #5: even when an anchor carries
        // human-friendly aliases, the rendered vocabulary line the LLM sees
        // uses the canonical `identifier` — never an alias. `find_by_token`
        // already returns the canonical Anchor regardless of which alias hit;
        // this pins that the rendering surfaces the canonical name.
        let mut a = Anchor::new(
            "set_active_graph_view",
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: "switch view".into(),
            },
            AnchorScope::Workspace {
                workspace_id: "ws".into(),
            },
            "switches the graph panel view",
        );
        a.aliases = vec!["set active".into(), "graph view".into()];
        let line = render_anchor(&a);
        assert!(
            line.contains("{{set_active_graph_view}}"),
            "canonical identifier rendered: {line}"
        );
        assert!(
            !line.contains("set active") && !line.contains("graph view"),
            "aliases must not leak into the LLM-facing line: {line}"
        );
    }

    #[test]
    fn parse_symbol_ids_reads_entry_id_in_order() {
        let v = json!({"matches": [
            {"entry": {"id": "a", "name": "a"}, "score": 1.0},
            {"entry": {"id": "b", "name": "b"}, "score": 0.5},
        ]});
        assert_eq!(parse_symbol_ids(&v), vec!["a", "b"]);
    }

    #[test]
    fn block_from_symbol_context_renders_focal_and_neighbors() {
        let v = ctx_json("foo", "fn foo() {}", &[("bar", 1), ("deep", 2)]);
        let block = block_from_symbol_context("foo", &v);
        assert_eq!(block.symbol_id, "foo");
        assert!(block.focal.contains("Symbol `foo` — src/foo.rs:10"));
        assert!(block.focal.contains("fn foo()"));
        assert_eq!(block.neighbors.len(), 2);
        assert!(block.neighbors.iter().any(|n| n.hop == 2));
    }
}
