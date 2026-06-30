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
    /// Concept-routing candidate set (concept-routing plan R1) — `Some` only
    /// when routing was requested (`route == true`) and the service routed.
    /// Logged by [`gather_with`]; carries the menu data for `preview`.
    pub route_candidates: Option<wylde_concept_routing::CandidateSet>,
    /// Concept-routing **R2 Augment injection** (plan §6.3) — the boundary blurb
    /// and member snippets for the user-curated concepts. Populates
    /// [`ChatContext::concept_context`]. Empty unless a non-empty curated set was
    /// forwarded (which only happens with the master toggle ON).
    pub concept_context: Vec<String>,
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
    /// Workspace memory **records** selected for this turn (memory plan
    /// M2, option B): top-k from the harness workspace store
    /// (`memory/workspace/`) — the tier reflection consolidates into for
    /// workspace-bound conversations, with importance + supersession
    /// semantics the flat notes tier lacks. Scored-search hits against
    /// the user message topped up with the importance-ranked head.
    /// Rendered lines, best-first. OI-8 tier ~2.7 — evictable
    /// (least-relevant line first), after `long_term`, before the
    /// user-curated `workspace_notes`.
    pub workspace_memory: Vec<String>,
    /// Anchors the prompt referenced — the never-drop vocabulary block (tier 7).
    pub vocabulary_anchors: Vec<AnchorBlock>,
    /// Workspace RAG snippets (B6 split). OI-8 tier **1** — the generic
    /// retrieval fallback, first to go; sheds lowest-ranked snippet first.
    pub workspace_rag: Vec<String>,
    /// Concept-routing **R2 Augment injection** (concept-routing plan §3, §6.3):
    /// the boundary blurb + member snippets for the user-curated concepts,
    /// rendered as the `### Concepts` slot. **Additive** — rides alongside
    /// `workspace_rag`, never replacing it (Augment). OI-8 tier **~1.5** —
    /// *above* generic RAG (coherent concept context outlives scattered chunks,
    /// per thesis §3.3) but below every other evictable tier and never the
    /// never-drop tier. Element `[0]` is the boundary blurb (protected: the slot
    /// sheds member snippets from the tail first). Empty unless routing is ON and
    /// the user curated a non-empty set — so OFF / no-curation renders nothing.
    pub concept_context: Vec<String>,
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
    /// True when the M3 tier-7 degrade pass had to shrink never-drop
    /// content (working-memory window, profile rules, vocabulary) to fit
    /// a small model's window. The shrunk slots carry their own visible
    /// markers; this flag is for logging/UI annotation (the B8 Settings
    /// surface, when it lands).
    pub tier7_degraded: bool,
    /// Honest, ordered log of what the gather actually did (chat-processing-
    /// indicator, full visibility): retrieval / routing / injection / memory
    /// steps with real counts + names. The driver replays these as
    /// [`TurnEvent::Step`](crate::events::TurnEvent::Step) so the GUI's
    /// activity dropdown shows the pipeline, not just "thinking…". Empty on a
    /// plain unbound turn that gathered nothing.
    pub steps: Vec<GatherStep>,
}

/// One line in the gather activity log (chat-processing-indicator). A
/// human-readable `summary` (e.g. "Retrieved 8 workspace snippets") plus an
/// optional `detail` (concept names, a degraded reason). Built from the real
/// gathered data, not stubbed.
pub(crate) struct GatherStep {
    pub stage: crate::events::StepStage,
    pub summary: String,
    pub detail: Option<String>,
}

impl GatherStep {
    fn new(
        stage: crate::events::StepStage,
        summary: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            stage,
            summary: summary.into(),
            detail,
        }
    }
}

/// Join up to `n` names into a "a, b, c (+k more)" detail string, or `None`
/// when the list is empty. Keeps the activity detail readable when a turn
/// routes to many concepts.
fn name_detail(names: &[String], n: usize) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let shown: Vec<&str> = names.iter().take(n).map(String::as_str).collect();
    let mut s = shown.join(", ");
    if names.len() > n {
        s.push_str(&format!(" (+{} more)", names.len() - n));
    }
    Some(s)
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
    /// parts (B6). `route` is the concept-routing master toggle (concept-routing
    /// plan R0/R1): `false` ⇒ the exact pre-routing path + no candidate set;
    /// `true` ⇒ the service also routes (reusing the RAG embed) and the block
    /// carries `route_candidates` for the caller to **log** (R1; no injection).
    /// `curated_concepts` is the concept-routing R2 curated set (plan §4):
    /// `Some` ⇒ Augment-inject those concepts (the block's `concept_context`);
    /// `None` ⇒ no injection.
    fn gather_prompt(
        &self,
        ws: &str,
        user_message: &str,
        route: bool,
        curated_concepts: Option<&[String]>,
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
        route: bool,
        curated_concepts: Option<&[String]>,
    ) -> SourceResult<Option<WorkspaceBlock>> {
        // Reuse the established Slice-0d workspace-prompt fetch + degrade
        // semantics (its own client + NoRetry policy) rather than re-deriving
        // them — keeps one definition of "is the workspace reachable".
        let prompt =
            crate::turn::workspace_context::gather(Some(ws), user_message, route, curated_concepts)
                .await;
        if prompt.degraded {
            Err(SourceStatus::Unavailable)
        } else if prompt.is_empty()
            && prompt.route_candidates.is_none()
            && prompt.concept_context.is_empty()
        {
            // Nothing to inject AND nothing to log → no block. (When routing
            // surfaced a candidate set, or R2 injected concept context, but
            // every other slot was empty, we still return a block so it reaches
            // the caller.)
            Ok(None)
        } else {
            Ok(Some(WorkspaceBlock {
                persona: prompt.persona,
                notes: prompt.notes,
                rag: prompt.rag,
                route_candidates: prompt.route_candidates,
                concept_context: prompt.concept_context,
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

/// Per-message GUI signals carried on the turn payload. The composer's token
/// choices (Slices F + M): `excluded` tokens never gather this message (the ✕
/// exclude); `reactivated` tokens gather even when an ignore tier covers them
/// (the ↺). Plus the Workspaces signal `active_file` (2.5): the workspace-
/// relative path of the file open in the editor when the turn was sent, used to
/// bias RAG toward the user's current focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TokenOverrides {
    pub excluded: Vec<String>,
    pub reactivated: Vec<String>,
    /// 2.5 (active-file boost): the file open in the Workspaces editor at send
    /// time (workspace-relative path), or `None`. Folded into the retrieval
    /// query behind the `[active_file: …]` marker so the workspace search layer
    /// can lexically boost chunks from that file / its directory.
    pub active_file: Option<String>,
    /// Concept-routing **R2** (plan §4): the user-curated concept ids carried by
    /// `chat.run_turn` after the curate-before-inject menu. `Some` (even empty)
    /// ⇒ the menu ran and these are the concepts to Augment-inject (empty ⇒
    /// inject nothing — Aaron's lock); `None` ⇒ no curation this turn ⇒ no
    /// injection (R1 behaviour). Only honoured when the master toggle is ON, so
    /// a stale list can never inject while routing is OFF.
    pub curated_concepts: Option<Vec<String>>,
}

impl TokenOverrides {
    /// Parse `excluded_tokens` / `reactivated_tokens` arrays and the
    /// `active_file` string off a `chat.run_turn` / `chat.start_turn` payload
    /// (absent → empty / `None`).
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
            active_file: payload
                .get("active_file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            // R2: distinguish "field absent" (None ⇒ no curation, no injection)
            // from "explicitly curated to nothing" (Some([]) ⇒ inject nothing
            // without re-routing). A present-but-non-array value reads as an
            // empty curated set, never as "absent".
            curated_concepts: payload.get("curated_concepts").map(|v| {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            }),
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
    // A conversation is *bound* iff it carries a non-blank workspace_id
    // (scope spec [D2]); resolve it once so the long-term read-gate and the
    // workspace reads below agree on "is there an active workspace".
    let active_ws = workspace_id.map(str::trim).filter(|s| !s.is_empty());

    // Always-on, in-process slots — never depend on the workspaces service.
    let mut ctx = ChatContext {
        user_profile: crate::user_profile::store::read().profile.to_prompt_block(),
        conversation_short_term: read_short_term(conversation_id),
        // B2: the auto-summary pipeline (chat/search/summary, regenerated
        // every 5 messages) finally feeds the tier-2 slot it was built for.
        conversation_summary: crate::chat::search::summary::auto_summary_for(conversation_id),
        // B3 + C2b-read [D2]: the long-term store reaches the prompt for
        // *unbound* (workspace-free) conversations only. A bound (workspace)
        // conversation excludes long-term entirely — global user identity/
        // prefs stay confined to the global Chat surface (manual copy-in is
        // the opt-in; the write-side complement ships in C8). Gating here
        // skips the embed round-trip too, so a bound turn pays nothing for the
        // slot it won't get.
        long_term: if active_ws.is_none() {
            gather_long_term(user_message).await
        } else {
            Vec::new()
        },
        // B1: the model can finally see the previous turns.
        history: load_history(conversation_id, user_message, slot_budget),
        ..ChatContext::default()
    };
    let mut degraded = false;
    // Captured for the activity log (chat-processing-indicator): the routed
    // concept set + the curated names actually injected this turn.
    let mut routed: Option<(usize, Vec<String>)> = None;
    let mut curated_names: Vec<String> = Vec::new();

    if let Some(ws) = active_ws {
        // Bare candidate tokens from the live message, then the Slice M / F
        // ignore-tier filter. Computed *before* the RAG query (2.4 reorder):
        // resolving anchors first lets their identifiers bias retrieval. This
        // is the literal-message token set — the symbol/anchor lookups below
        // keep using it verbatim; only the RAG/notes embed query is augmented.
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

        // Anchors + the symbol ids their code-symbol targets point at. Resolved
        // up front (deterministic, high-precision) so 2.4 can fold the resolved
        // anchor identifiers into the RAG query below.
        let (anchors, anchor_symbol_ids) = gather_anchors(source, ws, &tokens).await;
        let anchor_terms: Vec<String> = anchors.iter().map(|a| a.identifier.clone()).collect();
        ctx.vocabulary_anchors = anchors;

        // 2.3 (conversation-aware query construction) + 2.4 (anchor-biased
        // retrieval): embed an *augmented* retrieval query — the live message
        // plus a bounded keyword tail (recent turns / auto-summary / working
        // memory) plus the turn's resolved anchor identifiers — instead of just
        // the last message, so a terse follow-up ("why?") still retrieves the
        // thread's topic, and a question about a known symbol biases toward its
        // defining file. The live message leads and stays dominant (both tails
        // are capped) to avoid drifting retrieval off the current question.
        let mut retrieval_query = compose_retrieval_query(
            user_message,
            ctx.conversation_summary.as_deref(),
            &ctx.conversation_short_term,
            &ctx.history,
            &anchor_terms,
        );

        // 2.5 (active-file boost): append the editor's open file behind its own
        // marker so the workspace search layer can lexically boost chunks from
        // that file / its directory ("focus on this service" without
        // partitioning RAG). Mirrors the `[anchors: …]` cross-crate protocol
        // (`wylde-workspaces/.../rag/indexer/search.rs::extract_active_file` —
        // keep the marker strings in sync). Appended after the query is composed
        // so it rides even a plain turn that contributed no other terms.
        if let Some(active_file) = overrides
            .active_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            retrieval_query.push_str(&format!("\n\n[active_file: {active_file}]"));
        }

        // ── Concept-routing master toggle (concept-routing plan R0/R1) ──
        // This is THE single integration site (plan §3). The toggle is
        // harness-owned (`RoutingConfig`, the privacy-prefs store shape),
        // read in-process here on the hot path (cheap copy out of a cache).
        //
        //   * OFF (the default) ⇒ `route = false` ⇒ the service runs the EXACT
        //     pre-routing path and returns no candidate set, so the rest of
        //     this turn — and the rendered prompt — is byte-identical to
        //     pre-routing. The routing crate is never reached.
        //   * ON ⇒ `route = true` ⇒ the service routes server-side, reusing the
        //     RAG query embed (no extra embed, no extra round-trip — plan §6.1),
        //     and returns the candidate set, which we **log** below as the
        //     threshold-calibration data the plan calls for. **R1 injects
        //     NOTHING**: the candidate set never touches a `ChatContext` slot,
        //     so retrieval output is identical whether routing is on or off.
        let route = wylde_concept_routing::RoutingConfig::current().enabled;

        // R2 (concept-routing plan §4): the user-curated concept set carried by
        // `chat.run_turn` after the curate-before-inject menu. **Only honoured
        // when the master toggle is ON** — so with routing OFF a stale curated
        // list can never inject, keeping OFF byte-identical to today. `Some([])`
        // (curated to nothing) injects nothing; `None` (no menu this turn) keeps
        // R1 behaviour. Aaron's lock: never inject silently — injection requires
        // an explicit curated set from the menu.
        let curated = if route {
            overrides.curated_concepts.as_deref()
        } else {
            None
        };
        curated_names = curated.map(<[String]>::to_vec).unwrap_or_default();

        // The workspace prompt parts (persona / notes / RAG — B6 split) plus,
        // when routing is on, the routed candidate set (logged) and the R2
        // curated Augment injection. An unreachable service here is the degrade
        // signal (Slice 0d semantics).
        match source
            .gather_prompt(ws, &retrieval_query, route, curated)
            .await
        {
            Ok(Some(block)) => {
                // R1: log the routed candidate set (calibration data) + capture
                // it for the activity log.
                if let Some(set) = &block.route_candidates {
                    tracing::info!(target: "concept_routing", "[harness] {}", set.log_line());
                    let names: Vec<String> =
                        set.activated().map(|c| c.label.clone()).collect();
                    routed = Some((set.activated_count, names));
                }
                ctx.workspace_persona = block.persona;
                ctx.workspace_notes = block.notes;
                ctx.workspace_rag = block.rag;
                // R2 Augment: the concept slot rides ALONGSIDE the RAG slot
                // above (additive — never replacing it). Empty unless a non-empty
                // curated set was injected, so OFF / no-curation is unchanged.
                ctx.concept_context = block.concept_context;
            }
            Ok(None) => {}
            Err(SourceStatus::Unavailable) => degraded = true,
            Err(SourceStatus::Empty) => {}
        }

        // M2 (option B): the workspace memory records tier — in-process
        // like short-term/long-term (the store is harness-local), so it
        // doesn't ride the WorkspaceSource trait and never degrades the
        // turn.
        ctx.workspace_memory = gather_workspace_memory(ws, user_message);

        // Unambiguous symbol references from the prompt's bare tokens, plus the
        // anchor-target symbols, deduped and capped.
        let mut symbol_ids = anchor_symbol_ids;
        symbol_ids.extend(gather_unambiguous_symbols(source, ws, &tokens).await);
        dedupe_preserving_order(&mut symbol_ids);
        symbol_ids.truncate(MAX_SYMBOL_CONTEXTS);

        ctx.symbol_contexts = gather_symbol_contexts(source, ws, &symbol_ids).await;
    }

    // Build the honest activity log from what was actually gathered (pre-
    // eviction counts — they reflect the retrieval work, not the trim). Each
    // entry is gated on having something, so a plain turn that gathered
    // nothing emits no steps (no broken/empty sections).
    use crate::events::StepStage;
    let mut steps: Vec<GatherStep> = Vec::new();
    if !ctx.history.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Memory,
            format!("Loaded {} prior turn(s)", ctx.history.len()),
            None,
        ));
    }
    if !ctx.long_term.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Memory,
            format!("Recalled {} long-term memory line(s)", ctx.long_term.len()),
            None,
        ));
    }
    if !ctx.conversation_short_term.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Memory,
            format!("Working memory: {} entr(ies)", ctx.conversation_short_term.len()),
            None,
        ));
    }
    if !ctx.vocabulary_anchors.is_empty() {
        let names: Vec<String> = ctx
            .vocabulary_anchors
            .iter()
            .map(|a| a.identifier.clone())
            .collect();
        steps.push(GatherStep::new(
            StepStage::Symbol,
            format!("Resolved {} anchor(s)", names.len()),
            name_detail(&names, 6),
        ));
    }
    let snippet_count = ctx.workspace_rag.len() + ctx.workspace_notes.len();
    if snippet_count > 0 || ctx.workspace_persona.is_some() {
        let detail = ctx
            .workspace_persona
            .as_ref()
            .map(|_| "incl. workspace persona".to_owned());
        steps.push(GatherStep::new(
            StepStage::Retrieval,
            format!("Retrieved {snippet_count} workspace snippet(s)"),
            detail,
        ));
    }
    if let Some((count, names)) = &routed {
        steps.push(GatherStep::new(
            StepStage::Routing,
            format!("Routed to {count} concept(s)"),
            name_detail(names, 8),
        ));
    }
    if !ctx.concept_context.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Injection,
            format!("Injected {} concept definition(s)", curated_names.len().max(1)),
            name_detail(&curated_names, 8),
        ));
    }
    if !ctx.workspace_memory.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Memory,
            format!("Workspace memory: {} record(s)", ctx.workspace_memory.len()),
            None,
        ));
    }
    if !ctx.symbol_contexts.is_empty() {
        steps.push(GatherStep::new(
            StepStage::Symbol,
            format!("Loaded {} code-symbol context(s)", ctx.symbol_contexts.len()),
            None,
        ));
    }
    if degraded {
        steps.push(GatherStep::new(
            StepStage::Notice,
            "Workspace unreachable — answering from base context",
            None,
        ));
    }

    // Trim to the model's budget (OI-8), then render the named slots.
    // When the never-drop tier alone still exceeds the budget, evict's
    // M3 degrade pass shrinks tier-7 content rather than shipping an
    // over-window prompt Ollama would front-truncate.
    let tier7_degraded = token_budget::evict(&mut ctx, slot_budget);
    if tier7_degraded {
        steps.push(GatherStep::new(
            StepStage::Notice,
            "Context trimmed to fit the model's window",
            None,
        ));
    }
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
        tier7_degraded,
        steps,
    }
}

// ── concept-routing R2: the two-phase preview (plan §4) ───────────────────

/// Phase 1 of the curate-before-inject turn: route the turn's query into
/// concept space and return the [`CandidateSet`](wylde_concept_routing::CandidateSet)
/// the GUI menu is built from — **without** injecting or driving the LLM.
///
/// Returns `None` when the master toggle is OFF, there's no active workspace, or
/// the service routed nothing / is unreachable — in every such case the GUI
/// shows no menu and `chat.run_turn` proceeds exactly as today. Routes on the
/// *same* conversation-composed query the live gather builds (summary +
/// short-term + resolved anchors + active-file marker), so the menu reflects
/// what would actually activate. The only routing in the two-phase flow happens
/// here; `chat.run_turn` carries the user-curated ids and skips re-routing.
pub(crate) async fn preview(
    workspace_id: Option<&str>,
    user_message: &str,
    conversation_id: &str,
    overrides: &TokenOverrides,
) -> Option<wylde_concept_routing::CandidateSet> {
    preview_with(
        &LiveSource::for_active(),
        workspace_id,
        user_message,
        conversation_id,
        overrides,
    )
    .await
}

/// Source-injectable core of [`preview`] (tests pass a mock).
pub(crate) async fn preview_with<S: WorkspaceSource + Sync>(
    source: &S,
    workspace_id: Option<&str>,
    user_message: &str,
    conversation_id: &str,
    overrides: &TokenOverrides,
) -> Option<wylde_concept_routing::CandidateSet> {
    // Master toggle gate — OFF ⇒ no menu, no routing (byte-identical to today).
    if !wylde_concept_routing::RoutingConfig::current().enabled {
        return None;
    }
    let ws = workspace_id.map(str::trim).filter(|s| !s.is_empty())?;

    // Re-build the conversation-aware retrieval query exactly as `gather_with`
    // does (minus the windowed history, which the summary already distils), so
    // the previewed routing matches the turn that follows.
    let short_term = read_short_term(conversation_id);
    let summary = crate::chat::search::summary::auto_summary_for(conversation_id);

    let mut tokens = candidate_tokens(user_message);
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

    let (anchors, _) = gather_anchors(source, ws, &tokens).await;
    let anchor_terms: Vec<String> = anchors.iter().map(|a| a.identifier.clone()).collect();

    let mut retrieval_query = compose_retrieval_query(
        user_message,
        summary.as_deref(),
        &short_term,
        &[],
        &anchor_terms,
    );
    if let Some(active_file) = overrides
        .active_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        retrieval_query.push_str(&format!("\n\n[active_file: {active_file}]"));
    }

    // Route only (curated = None ⇒ no injection); return the candidate set.
    match source.gather_prompt(ws, &retrieval_query, true, None).await {
        Ok(Some(block)) => block.route_candidates,
        _ => None,
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
/// pure `core_block` (importance desc).
///
/// Touch damping (memory plan M5): only the **similarity hits** get
/// their `last_used_at` bumped. Pre-M5 every injected record was
/// touched, so the importance-sorted `core_block` fillers re-warmed
/// themselves every turn — injection was self-reinforcing and the
/// recency term could never demote a stale "core" record.
async fn gather_long_term(user_message: &str) -> Vec<String> {
    use crate::memory::long_term as entries;

    // The store-empty fast path also keeps plain turns embed-free.
    let core = entries::core_block(Some(LONG_TERM_LIMIT));
    if core.is_empty() {
        return Vec::new();
    }

    let mut selected = core;
    let mut hit_ids: Vec<String> = Vec::new();
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
                    hit_ids.push(r.id.clone());
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

    entries::touch_all(&hit_ids);
    selected
        .iter()
        .map(|r| format!("- {}", r.body.trim()))
        .collect()
}

/// How many workspace memory records ride each turn (memory plan M2,
/// option B). Matches the workspace search verb's default hit count.
const WORKSPACE_MEMORY_LIMIT: usize = 5;

/// Whether the workspace-memory gather slot is enabled
/// (`WYLDE_WORKSPACE_MEMORY_SLOT=off|0|false` kill switch; on by
/// default — the M2 behavioral-slice switch).
fn workspace_memory_slot_enabled() -> bool {
    match std::env::var("WYLDE_WORKSPACE_MEMORY_SLOT") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        ),
        Err(_) => true,
    }
}

/// Select this turn's workspace memory record lines (memory plan M2,
/// option B — Aaron's call: the harness workspace store, with its
/// importance + supersession semantics, is the canonical middle tier
/// and must reach prompts).
///
/// Selection mirrors [`gather_long_term`]'s shape on the workspace
/// store's own machinery: `search_records` hits against the user
/// message (token-overlap similarity × importance × recency decay —
/// non-superseded records only), topped up with the importance-ranked
/// `list_records` head when the search comes back thin. In-process,
/// synchronous, fail-soft: an empty store yields no lines and a plain
/// turn stays byte-identical.
fn gather_workspace_memory(workspace_id: &str, user_message: &str) -> Vec<String> {
    use crate::memory::workspace::store as ws_store;

    if !workspace_memory_slot_enabled() {
        return Vec::new();
    }
    let hits = ws_store::search_records(workspace_id, user_message, WORKSPACE_MEMORY_LIMIT, None);
    let mut selected: Vec<(String, String)> = hits.into_iter().map(|h| (h.id, h.body)).collect();
    if selected.len() < WORKSPACE_MEMORY_LIMIT {
        // Top up with the importance-ranked head the search missed —
        // high-importance insights ride even with zero token overlap
        // (the same "core block" rule B3 gave the long-term tier).
        for r in ws_store::list_records(workspace_id, false) {
            if selected.len() >= WORKSPACE_MEMORY_LIMIT {
                break;
            }
            if !selected.iter().any(|(id, _)| *id == r.id) {
                selected.push((r.id, r.body));
            }
        }
    }
    selected
        .iter()
        .map(|(_, body)| format!("- {}", body.trim()))
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

/// The M3 tier-7 degrade floor: the newest working-memory entries that
/// survive even a 4096-window squeeze. Below this the slot stops being
/// "memory" at all, and the trade flips back toward overshooting.
pub(crate) const WORKING_MEMORY_DEGRADE_FLOOR: usize = 5;

/// Shed the OLDEST real working-memory line for the M3 tier-7 degrade
/// pass, maintaining the visible omission marker (same format the B8
/// injection cap writes, so the model sees one consistent signal).
/// Returns `false` — nothing removed — once only
/// [`WORKING_MEMORY_DEGRADE_FLOOR`] real entries remain.
pub(crate) fn degrade_short_term_once(lines: &mut Vec<String>) -> bool {
    let omitted = parse_omission_marker(lines.first());
    let has_marker = omitted.is_some();
    let real = lines.len() - usize::from(has_marker);
    if real <= WORKING_MEMORY_DEGRADE_FLOOR {
        return false;
    }
    // Remove the oldest real entry (just after the marker when present).
    lines.remove(usize::from(has_marker));
    let total_omitted = omitted.unwrap_or(0) + 1;
    let marker =
        format!("- [{total_omitted} older working-memory entries omitted (injection cap)]");
    if has_marker {
        lines[0] = marker;
    } else {
        lines.insert(0, marker);
    }
    true
}

/// Parse the omitted-count out of a B8 omission marker line, when `line`
/// is one.
fn parse_omission_marker(line: Option<&String>) -> Option<usize> {
    let l = line?;
    let rest = l.strip_prefix("- [")?;
    let end = rest.find(" older working-memory entries omitted (injection cap)]")?;
    rest[..end].parse().ok()
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

// ── 2.3 conversation-aware retrieval query ───────────────────────────────

/// Max distinct context keywords folded into the RAG/notes embed query
/// (2.3). Bounded so a long thread can't swamp the live message's
/// embedding — the block's "keep the live message dominant to avoid drift".
const RETRIEVAL_CONTEXT_TERMS_MAX: usize = 24;

/// Min length for a context keyword (matches the symbol-token bar): shorter
/// fragments are noise that only dilutes the query embedding.
const RETRIEVAL_KEYWORD_MIN_LEN: usize = MIN_TOKEN_LEN;

/// Cap on resolved anchor identifiers folded into the RAG query (2.4). Anchors
/// are already few (one workspace turn references a handful of symbols); the
/// cap is a guard so a pathological turn can't swamp the embedding or the
/// search layer's boost loop.
const RETRIEVAL_ANCHOR_TERMS_MAX: usize = 12;

/// How many of the most-recent prior turns we mine for context keywords.
/// One exchange (the immediately preceding user+assistant pair) is the
/// thread topic a terse follow-up refers to; older turns are covered by the
/// auto-summary.
const RETRIEVAL_HISTORY_TURNS: usize = 2;

/// High-frequency function words excluded from the keyword tail — they carry
/// no retrieval signal and only dilute the live message's dominance. (Only
/// the *appended tail* is filtered; the live message is always embedded
/// verbatim regardless.)
const RETRIEVAL_STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "you", "your", "are", "was", "were", "have",
    "has", "had", "not", "but", "can", "could", "would", "should", "does", "did", "what", "why",
    "how", "when", "where", "who", "which", "into", "from", "about", "there", "then", "them",
    "they", "its", "our", "out", "get", "got", "one", "all", "any", "more", "than", "also", "like",
    "just", "explain", "please", "tell", "use", "using",
];

/// Compose the workspace RAG / notes embed query for this turn (improvement
/// 2.3, *conversation-aware query construction*).
///
/// The live `user_message` always leads and stays the dominant contributor;
/// a **bounded** tail of distinctive keywords — drawn from the conversation's
/// working memory, the most-recent prior turns, then the running auto-summary
/// (freshest source first, so the cap keeps the most-current terms) — nudges
/// retrieval toward the thread topic. A one-word follow-up ("why?") then
/// pulls that topic's chunks instead of nothing; a substantive question keeps
/// its own terms dominant because the tail is capped at
/// [`RETRIEVAL_CONTEXT_TERMS_MAX`] keywords and excludes anything already in
/// the message.
///
/// When the conversation contributes no fresh keyword (a first / plain turn,
/// or the message already names everything) **and** no anchor resolved, the
/// original message is returned unchanged — that turn embeds byte-identically
/// to before.
///
/// 2.4 (anchor-biased retrieval): `anchor_terms` are the turn's already-
/// resolved anchor identifiers ([`gather_anchors`]). They are appended behind
/// their own `[anchors: …]` marker so (a) the embedding sees the symbol names
/// (query expansion) and (b) the workspace search layer can parse the exact
/// resolved set out of the query and lexically boost chunks in the symbol's
/// defining file (`wylde-workspaces/.../rag/indexer/search.rs::extract_anchor_terms`
/// — the marker string is a shared cross-crate protocol; keep them in sync).
fn compose_retrieval_query(
    user_message: &str,
    summary: Option<&str>,
    short_term: &[String],
    history: &[HistoryMessage],
    anchor_terms: &[String],
) -> String {
    let msg = user_message.trim();

    // Words already in the live message are never repeated in the tail — no
    // point re-embedding a term the message already carries, and it keeps the
    // message the largest coherent contributor.
    let mut in_message: std::collections::HashSet<String> = std::collections::HashSet::new();
    push_lowercased_words(msg, &mut in_message);

    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Working memory — the conversation's most salient current facts (newest
    // entries are last in the slice, so mine newest-first).
    for line in short_term.iter().rev() {
        push_keywords(line, &in_message, &mut seen, &mut terms);
        if terms.len() >= RETRIEVAL_CONTEXT_TERMS_MAX {
            break;
        }
    }
    // The most-recent prior turns (history is chronological → take the tail).
    for m in history.iter().rev().take(RETRIEVAL_HISTORY_TURNS) {
        if terms.len() >= RETRIEVAL_CONTEXT_TERMS_MAX {
            break;
        }
        push_keywords(&m.content, &in_message, &mut seen, &mut terms);
    }
    // The running auto-summary — broadest context, fills any remaining room.
    if let Some(s) = summary {
        push_keywords(s, &in_message, &mut seen, &mut terms);
    }
    terms.truncate(RETRIEVAL_CONTEXT_TERMS_MAX);

    // 2.4: distinct resolved anchor identifiers, capped, first-seen order
    // (case-insensitive dedupe). These bias the embedding *and* are parsed back
    // out by the search layer for the lexical boost — so they ride a dedicated
    // marker, kept verbatim (not stopword/length filtered like conversation
    // keywords): a resolved symbol id is high-signal regardless of its shape.
    let mut anchor_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let anchors: Vec<&str> = anchor_terms
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty() && anchor_seen.insert(a.to_ascii_lowercase()))
        .take(RETRIEVAL_ANCHOR_TERMS_MAX)
        .collect();

    if terms.is_empty() && anchors.is_empty() {
        return user_message.to_owned();
    }

    // Live message leads; each context source trails behind a visible marker so
    // it reads as retrieval scaffolding, not part of the question. The
    // `[anchors: …]` marker is the cross-crate protocol the search layer parses.
    let mut out = msg.to_owned();
    if !terms.is_empty() {
        out.push_str(&format!("\n\n[conversation context: {}]", terms.join(" ")));
    }
    if !anchors.is_empty() {
        out.push_str(&format!("\n\n[anchors: {}]", anchors.join(" ")));
    }
    out
}

/// Collect identifier-ish words (alnum + `_`) of any length, lowercased, into
/// `out` — used to build the "already in the message" exclusion set.
fn push_lowercased_words(text: &str, out: &mut std::collections::HashSet<String>) {
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch.to_ascii_lowercase());
        } else if !cur.is_empty() {
            out.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
}

/// Extract distinctive keywords from `text` (identifier-ish words ≥
/// [`RETRIEVAL_KEYWORD_MIN_LEN`], lowercased, not a stopword, not already in
/// the message, first-seen only) and append them to `out`.
fn push_keywords(
    text: &str,
    in_message: &std::collections::HashSet<String>,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) {
    let mut cur = String::new();
    let consider =
        |cur: &mut String, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>| {
            if cur.len() >= RETRIEVAL_KEYWORD_MIN_LEN
                && !RETRIEVAL_STOPWORDS.contains(&cur.as_str())
                && !in_message.contains(cur.as_str())
                && seen.insert(cur.clone())
            {
                out.push(cur.clone());
            }
            cur.clear();
        };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch.to_ascii_lowercase());
        } else {
            consider(&mut cur, seen, out);
        }
    }
    consider(&mut cur, seen, out);
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
    use serial_test::serial;
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
        /// every `user_message`/query string `gather_prompt` was called with,
        /// in call order — lets a test inspect the exact retrieval query the
        /// gather forwarded to the service (2.3).
        gather_prompt_queries: std::sync::Mutex<Vec<String>>,
        /// every `route` flag `gather_prompt` was called with, in call order —
        /// lets a routing test assert the master toggle drove the request
        /// (concept-routing plan R0/R1).
        gather_prompt_routes: std::sync::Mutex<Vec<bool>>,
        /// every `curated_concepts` arg `gather_prompt` was called with, in call
        /// order — lets an R2 test assert the curated set was forwarded (or NOT,
        /// when the toggle is off).
        gather_prompt_curated: std::sync::Mutex<Vec<Option<Vec<String>>>>,
        /// when set, the returned block's `concept_context` is derived from the
        /// FORWARDED curated arg (a blurb + a snippet per id; empty for
        /// None/`Some([])`) — faithfully modelling the server, which injects iff
        /// a non-empty curated set reaches it. Lets the R2 safety proof assert
        /// that toggle-off (curated=None) truly renders no concept slot.
        echo_curated_injection: bool,
    }

    impl WorkspaceSource for MockSource {
        async fn gather_prompt(
            &self,
            _ws: &str,
            m: &str,
            route: bool,
            curated_concepts: Option<&[String]>,
        ) -> SourceResult<Option<WorkspaceBlock>> {
            self.gather_prompt_queries
                .lock()
                .unwrap()
                .push(m.to_owned());
            self.gather_prompt_routes.lock().unwrap().push(route);
            self.gather_prompt_curated
                .lock()
                .unwrap()
                .push(curated_concepts.map(<[String]>::to_vec));
            if self.prompt_unavailable {
                return Err(SourceStatus::Unavailable);
            }
            let mut block = self.prompt.clone();
            if self.echo_curated_injection {
                // Model the server: inject iff a non-empty curated set reached us.
                let injected: Vec<String> = match curated_concepts {
                    Some(ids) if !ids.is_empty() => {
                        let mut v = vec![format!("BLURB: {}", ids.join(", "))];
                        v.push(format!("`{}.rs` (lines 1-9)\nfn member() {{}}", ids[0]));
                        v
                    }
                    _ => Vec::new(),
                };
                if let Some(b) = block.as_mut() {
                    b.concept_context = injected;
                }
            }
            Ok(block)
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
            ..Default::default()
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

    // ── 2.3: conversation-aware retrieval query construction ────────────

    #[test]
    fn compose_query_folds_context_keeps_message_dominant_and_passes_through() {
        // Working memory + recent turns + summary all establish a topic the
        // terse live message does not name.
        let history = vec![
            HistoryMessage {
                role: "user".into(),
                content: "how does the eviction ladder shed tokens?".into(),
            },
            HistoryMessage {
                role: "assistant".into(),
                content: "the budget evicts the workspace_rag tier first".into(),
            },
        ];
        let short_term = vec!["- focus: tokenizer internals".to_owned()];
        let summary = Some("Discussing the eviction ladder and token budget.");

        let q = compose_retrieval_query("why?", summary, &short_term, &history, &[]);
        // Live message leads (dominant) ...
        assert!(q.starts_with("why?"), "live message must lead: {q}");
        // ... and the thread topic is folded in from each source.
        assert!(
            q.contains("tokenizer"),
            "working-memory keyword folded: {q}"
        );
        assert!(
            q.contains("eviction") && q.contains("ladder"),
            "recent-turn / summary keywords folded: {q}"
        );
        // Stopwords + message-words are excluded from the tail.
        assert!(!q.contains("[conversation context: why"), "no message echo");

        // No conversation context and no anchors → byte-identical passthrough
        // (plain turn).
        assert_eq!(
            compose_retrieval_query("brand new question", None, &[], &[], &[]),
            "brand new question"
        );
    }

    #[test]
    fn compose_query_caps_the_keyword_tail() {
        // 60 distinct long keywords across working memory — the tail is
        // bounded so a long thread can't swamp the live message.
        let wm: Vec<String> = (0..60).map(|i| format!("- keyword{i:03}xyz")).collect();
        let q = compose_retrieval_query("ok", None, &wm, &[], &[]);
        let tail = q
            .split_once("[conversation context: ")
            .and_then(|(_, t)| t.strip_suffix(']'))
            .expect("context tail present");
        assert_eq!(
            tail.split_whitespace().count(),
            RETRIEVAL_CONTEXT_TERMS_MAX,
            "keyword tail is capped"
        );
    }

    #[tokio::test]
    async fn gather_forwards_conversation_aware_query_to_the_workspace_source() {
        // The REAL gather path (gather_with) must hand the *augmented* query
        // to the workspace source — proving the composed query reaches the
        // RAG/notes embed boundary, not just that the helper works.
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut doc = serde_json::Map::new();
        doc.insert("id".into(), json!("conv-2-3"));
        doc.insert(
            "messages".into(),
            json!([
                {"role": "user", "content": "how does the eviction ladder shed tokens?"},
                {"role": "assistant", "content": "the budget evicts the workspace_rag tier first"},
            ]),
        );
        doc.insert(
            "auto_summary".into(),
            json!("Discussing the eviction ladder and the token budget."),
        );
        crate::memory::conversations::store::save_conversation(&doc).unwrap();

        let src = MockSource::default();
        // A one-word follow-up carrying none of the topic itself.
        let _ = gather_with(
            &src,
            Some("ws"),
            "why?",
            "conv-2-3",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        let queries = src.gather_prompt_queries.lock().unwrap().clone();
        let forwarded = queries.first().expect("gather_prompt was called");
        assert!(
            forwarded.starts_with("why?"),
            "the live message leads the forwarded query: {forwarded}"
        );
        assert!(
            forwarded.contains("eviction")
                || forwarded.contains("ladder")
                || forwarded.contains("budget"),
            "the forwarded RAG query carries the thread topic: {forwarded}"
        );
        // Sanity: a query that is *just* the bare message would not contain
        // any of the prior-turn vocabulary.
        assert_ne!(forwarded, "why?", "the query must be augmented, not bare");
    }

    // ── concept-routing master toggle (concept-routing plan R0/R1) ──────

    /// A mock block carrying persona/notes/rag, with an optional candidate set.
    fn routing_mock(route_candidates: Option<wylde_concept_routing::CandidateSet>) -> MockSource {
        MockSource {
            prompt: Some(WorkspaceBlock {
                persona: Some("Be precise.".into()),
                notes: vec!["uses cargo".into()],
                rag: vec!["fn main() {}".into()],
                route_candidates,
                concept_context: Vec::new(),
            }),
            ..MockSource::default()
        }
    }

    fn sample_candidate_set() -> wylde_concept_routing::CandidateSet {
        wylde_concept_routing::CandidateSet {
            query_echo: "auth question".into(),
            concepts: vec![wylde_concept_routing::RoutedConcept {
                id: "a".into(),
                label: "Auth".into(),
                score: 0.71,
                seed_score: 0.71,
                provenance: wylde_concept_routing::Provenance::Seed,
                activated: true,
            }],
            vocabulary: vec![],
            abs_threshold: 0.50,
            chosen_cutoff: 0.50,
            activated_count: 1,
            max_concepts: 3,
        }
    }

    #[tokio::test]
    #[serial]
    async fn routing_toggle_off_forwards_no_route_and_is_unchanged() {
        // Default (toggle OFF): gather must forward `route = false` and the
        // rendered slots are exactly the non-routing path.
        let _env = crate::user_profile::test_support::TestEnv::new();
        wylde_concept_routing::RoutingConfig::reload_from_disk(); // fresh dir ⇒ off
        assert!(
            !wylde_concept_routing::RoutingConfig::current().enabled,
            "precondition: master toggle defaults off"
        );

        let src = routing_mock(None);
        let out = gather_with(
            &src,
            Some("ws"),
            "auth question",
            "conv-routing-off",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        let routes = src.gather_prompt_routes.lock().unwrap().clone();
        assert_eq!(routes, vec![false], "toggle off ⇒ route=false forwarded");
        // The persona/notes/rag still render exactly as before routing existed.
        assert!(out.system_slots.contains("Be precise."));
        assert!(out.system_slots.contains("fn main() {}"));
    }

    #[tokio::test]
    #[serial]
    async fn routing_toggle_on_routes_logs_and_injects_nothing() {
        // Toggle ON: gather must forward `route = true`, accept the returned
        // candidate set (which the branch logs) — and the rendered slots must
        // be BYTE-IDENTICAL to the toggle-off render (zero injection in R1).
        let _env = crate::user_profile::test_support::TestEnv::new();

        // Baseline: what the slots look like with routing off + no candidates.
        wylde_concept_routing::RoutingConfig::reload_from_disk();
        let baseline = gather_with(
            &routing_mock(None),
            Some("ws"),
            "auth question",
            "conv-routing-base",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        // Flip the master toggle on (persists to this test's data dir + cache).
        wylde_concept_routing::RoutingConfig::persist(wylde_concept_routing::RoutingConfig {
            enabled: true,
            ..wylde_concept_routing::RoutingConfig::default()
        })
        .expect("persist toggle on");
        assert!(wylde_concept_routing::RoutingConfig::current().enabled);

        // The service now routes and returns a candidate set.
        let src = routing_mock(Some(sample_candidate_set()));
        let out = gather_with(
            &src,
            Some("ws"),
            "auth question",
            "conv-routing-on",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        // Router was invoked: the master toggle drove `route = true`.
        let routes = src.gather_prompt_routes.lock().unwrap().clone();
        assert_eq!(routes, vec![true], "toggle on ⇒ route=true forwarded");

        // ZERO injection: the candidate set never reaches a slot, so the
        // rendered prompt is identical to the routing-off baseline.
        assert_eq!(
            out.system_slots, baseline.system_slots,
            "R1 injects nothing — slots identical to the non-routing render"
        );

        // Reset the process-global config cache so later tests see default-off.
        wylde_concept_routing::RoutingConfig::persist(
            wylde_concept_routing::RoutingConfig::default(),
        )
        .expect("reset toggle off");
    }

    // ── concept-routing R2: curate-before-inject + Augment injection ────

    /// A mock that models the server's R2 injection: persona/notes/rag plus a
    /// concept slot derived from the forwarded curated set (see
    /// `echo_curated_injection`).
    fn injecting_mock() -> MockSource {
        MockSource {
            prompt: Some(WorkspaceBlock {
                persona: None,
                notes: vec![],
                rag: vec!["fn rag_chunk() {}".into()],
                route_candidates: None,
                concept_context: Vec::new(),
            }),
            echo_curated_injection: true,
            ..MockSource::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn r2_toggle_off_ignores_curated_concepts_and_is_unchanged() {
        // The safety proof: even when the payload carries a curated set, master
        // toggle OFF forwards `curated = None`, so the server injects nothing and
        // the slots are byte-identical to a plain turn. A stale GUI list can't
        // inject while routing is off.
        let _env = crate::user_profile::test_support::TestEnv::new();
        wylde_concept_routing::RoutingConfig::reload_from_disk(); // fresh dir ⇒ off
        assert!(!wylde_concept_routing::RoutingConfig::current().enabled);

        let overrides = TokenOverrides {
            curated_concepts: Some(vec!["nextcloud".into()]),
            ..Default::default()
        };
        let src = injecting_mock();
        let out = gather_with(
            &src,
            Some("ws"),
            "q",
            "conv-r2-off",
            &overrides,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        // Curated was NOT forwarded (toggle gates it), routing stayed off, and
        // — because the (server-modelling) mock injects only from a forwarded
        // set — NO concept slot renders. OFF is byte-identical to today.
        let curated = src.gather_prompt_curated.lock().unwrap().clone();
        assert_eq!(curated, vec![None], "toggle off ⇒ curated=None forwarded");
        let routes = src.gather_prompt_routes.lock().unwrap().clone();
        assert_eq!(routes, vec![false]);
        assert!(
            !out.system_slots.contains("### Concepts"),
            "OFF injects nothing"
        );
    }

    #[tokio::test]
    #[serial]
    async fn r2_augment_injects_concept_slot_alongside_rag() {
        // Toggle ON + a curated set: the concept slot is injected ALONGSIDE the
        // RAG slot (Augment — additive, never replacing). The curated ids are
        // forwarded to the service.
        let _env = crate::user_profile::test_support::TestEnv::new();
        wylde_concept_routing::RoutingConfig::persist(wylde_concept_routing::RoutingConfig {
            enabled: true,
            ..Default::default()
        })
        .expect("toggle on");

        let overrides = TokenOverrides {
            curated_concepts: Some(vec!["nextcloud".into(), "ddns".into()]),
            ..Default::default()
        };
        let src = injecting_mock();
        let out = gather_with(
            &src,
            Some("ws"),
            "how does nextcloud sync",
            "conv-r2-on",
            &overrides,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        // Curated ids forwarded to the service (Some, with both ids).
        let curated = src.gather_prompt_curated.lock().unwrap().clone();
        assert_eq!(
            curated,
            vec![Some(vec!["nextcloud".to_owned(), "ddns".to_owned()])]
        );
        // Augment: BOTH the concept slot and the raw RAG slot render.
        assert!(out.system_slots.contains("### Concepts"));
        assert!(out.system_slots.contains("BLURB: nextcloud, ddns"));
        assert!(
            out.system_slots.contains("### Workspace context"),
            "Augment keeps the RAG slot alongside (never replaces it)"
        );
        assert!(out.system_slots.contains("fn rag_chunk() {}"));

        wylde_concept_routing::RoutingConfig::persist(Default::default()).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn r2_empty_curated_set_injects_nothing() {
        // Aaron's lock: a curated-empty menu injects nothing. The empty set is
        // still forwarded (Some([]) — explicit "curated to nothing"), but the
        // server injects nothing for it, so no slot renders.
        let _env = crate::user_profile::test_support::TestEnv::new();
        wylde_concept_routing::RoutingConfig::persist(wylde_concept_routing::RoutingConfig {
            enabled: true,
            ..Default::default()
        })
        .expect("toggle on");

        let overrides = TokenOverrides {
            curated_concepts: Some(vec![]),
            ..Default::default()
        };
        let src = injecting_mock();
        let out = gather_with(
            &src,
            Some("ws"),
            "q",
            "conv-r2-empty",
            &overrides,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        let curated = src.gather_prompt_curated.lock().unwrap().clone();
        assert_eq!(
            curated,
            vec![Some(Vec::<String>::new())],
            "Some([]) forwarded"
        );
        assert!(
            !out.system_slots.contains("### Concepts"),
            "nothing injected"
        );

        wylde_concept_routing::RoutingConfig::persist(Default::default()).unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn preview_returns_candidates_when_toggle_on_and_none_when_off() {
        let _env = crate::user_profile::test_support::TestEnv::new();

        // OFF ⇒ no preview, no routing (and gather_prompt is never called).
        wylde_concept_routing::RoutingConfig::reload_from_disk();
        let src = routing_mock(Some(sample_candidate_set()));
        let got = preview_with(
            &src,
            Some("ws"),
            "auth question",
            "conv-prev-off",
            &TokenOverrides::default(),
        )
        .await;
        assert!(got.is_none(), "toggle off ⇒ no menu");
        assert!(
            src.gather_prompt_routes.lock().unwrap().is_empty(),
            "toggle off ⇒ preview doesn't even call the service"
        );

        // ON ⇒ routes (curated=None) and returns the candidate set.
        wylde_concept_routing::RoutingConfig::persist(wylde_concept_routing::RoutingConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        let src = routing_mock(Some(sample_candidate_set()));
        let got = preview_with(
            &src,
            Some("ws"),
            "auth question",
            "conv-prev-on",
            &TokenOverrides::default(),
        )
        .await;
        let set = got.expect("toggle on ⇒ candidates");
        assert_eq!(set.concepts[0].id, "a");
        // Preview routes (route=true) but never injects (curated=None).
        let routes = src.gather_prompt_routes.lock().unwrap().clone();
        assert_eq!(routes, vec![true]);
        let curated = src.gather_prompt_curated.lock().unwrap().clone();
        assert_eq!(curated, vec![None], "preview never injects");

        wylde_concept_routing::RoutingConfig::persist(Default::default()).unwrap();
    }

    // ── 2.4: anchor-biased retrieval query construction ─────────────────

    #[test]
    fn compose_query_appends_resolved_anchors_behind_marker() {
        // Anchors ride a dedicated marker, deduped (case-insensitive), capped,
        // kept verbatim; the live message still leads.
        let anchors = vec![
            "compose_retrieval_query".to_owned(),
            "Compose_Retrieval_Query".to_owned(), // dup (case-insensitive)
            "gather_with".to_owned(),
        ];
        let q = compose_retrieval_query("how is the query built?", None, &[], &[], &anchors);
        assert!(
            q.starts_with("how is the query built?"),
            "message leads: {q}"
        );
        let tail = q
            .split_once("[anchors: ")
            .and_then(|(_, t)| t.strip_suffix(']'))
            .expect("anchor marker present");
        assert_eq!(
            tail, "compose_retrieval_query gather_with",
            "anchors deduped, order-preserved, verbatim"
        );

        // Anchors augment even when there is *no* conversation context.
        let q2 = compose_retrieval_query("plain", None, &[], &[], &["run_it".to_owned()]);
        assert_eq!(q2, "plain\n\n[anchors: run_it]");

        // Still byte-identical when neither context nor anchors contribute.
        assert_eq!(compose_retrieval_query("bare", None, &[], &[], &[]), "bare");
    }

    #[test]
    fn compose_query_caps_the_anchor_tail() {
        let many: Vec<String> = (0..40).map(|i| format!("symbol_{i:03}")).collect();
        let q = compose_retrieval_query("q", None, &[], &[], &many);
        let tail = q
            .split_once("[anchors: ")
            .and_then(|(_, t)| t.strip_suffix(']'))
            .expect("anchor tail present");
        assert_eq!(
            tail.split_whitespace().count(),
            RETRIEVAL_ANCHOR_TERMS_MAX,
            "anchor tail is capped"
        );
    }

    #[tokio::test]
    async fn gather_forwards_resolved_anchors_to_the_workspace_source() {
        // The REAL gather path must resolve anchors *before* the RAG query and
        // fold the resolved identifier into it behind the `[anchors: …]` marker
        // the search layer parses — proving the producer half of 2.4 reaches
        // the embed/search boundary.
        let _env = crate::user_profile::test_support::TestEnv::new();
        let mut src = MockSource::default();
        let anchor = Anchor::new(
            "compose_retrieval_query",
            AnchorKind::CodeSymbol,
            AnchorTarget::CodeSymbol {
                symbol_id: "compose_retrieval_query".into(),
            },
            AnchorScope::Workspace {
                workspace_id: "ws".into(),
            },
            "builds the augmented RAG query",
        );
        // The token the user typed resolves to the anchor.
        src.anchors
            .insert("compose_retrieval_query".into(), vec![anchor]);

        let _ = gather_with(
            &src,
            Some("ws"),
            "what does compose_retrieval_query do?",
            "conv-2-4",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        let queries = src.gather_prompt_queries.lock().unwrap().clone();
        let forwarded = queries.first().expect("gather_prompt was called");
        assert!(
            forwarded.contains("[anchors: compose_retrieval_query]"),
            "resolved anchor folded into the forwarded query: {forwarded}"
        );
        assert!(
            forwarded.starts_with("what does compose_retrieval_query do?"),
            "live message still leads: {forwarded}"
        );
    }

    // ── 2.5: active-file boost — GUI signal → forwarded query marker ─────

    #[test]
    fn from_payload_parses_active_file() {
        let o = TokenOverrides::from_payload(&json!({
            "active_file": "  services/x/foo.rs  "
        }));
        assert_eq!(
            o.active_file.as_deref(),
            Some("services/x/foo.rs"),
            "trimmed"
        );
        // Absent / blank → None.
        assert_eq!(TokenOverrides::from_payload(&json!({})).active_file, None);
        assert_eq!(
            TokenOverrides::from_payload(&json!({"active_file": "   "})).active_file,
            None
        );
    }

    #[tokio::test]
    async fn gather_folds_active_file_into_the_forwarded_query() {
        // The REAL gather path must append the editor's open file behind the
        // `[active_file: …]` marker the search layer parses — the producer half
        // of 2.5 reaching the embed/search boundary.
        let _env = crate::user_profile::test_support::TestEnv::new();
        let src = MockSource::default();
        let overrides = TokenOverrides {
            active_file: Some("services/x/foo.rs".to_owned()),
            ..Default::default()
        };
        let _ = gather_with(
            &src,
            Some("ws"),
            "how does the dispatcher work?",
            "conv-2-5",
            &overrides,
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;

        let queries = src.gather_prompt_queries.lock().unwrap().clone();
        let forwarded = queries.first().expect("gather_prompt was called");
        assert!(
            forwarded.contains("[active_file: services/x/foo.rs]"),
            "active file folded into the forwarded query: {forwarded}"
        );
        assert!(
            forwarded.starts_with("how does the dispatcher work?"),
            "live message still leads: {forwarded}"
        );
    }

    #[tokio::test]
    async fn gather_omits_active_file_marker_when_unset() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        let src = MockSource::default();
        let _ = gather_with(
            &src,
            Some("ws"),
            "plain question",
            "conv-2-5b",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        let queries = src.gather_prompt_queries.lock().unwrap().clone();
        let forwarded = queries.first().expect("gather_prompt was called");
        assert!(
            !forwarded.contains("[active_file:"),
            "no marker when no active file: {forwarded}"
        );
    }

    // ── M2 (option B): workspace memory records → the insights slot ────

    #[tokio::test]
    async fn workspace_memory_records_feed_the_insights_slot() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::workspace::store as ws_store;
        let v1 =
            ws_store::save_new("ws", "stale guidance", "reflection", Some(6.0), vec![]).unwrap();
        // Supersede v1 — only the replacement may ride.
        ws_store::update("ws", &v1.id, Some("fresh distilled guidance"), None, None).unwrap();
        ws_store::save_new(
            "ws",
            "unrelated high-value insight",
            "chat",
            Some(9.0),
            vec![],
        )
        .unwrap();

        let src = MockSource::default();
        let out = gather_with(
            &src,
            Some("ws"),
            "anything at all",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            out.system_slots.contains("### Workspace insights"),
            "slot present: {}",
            out.system_slots
        );
        assert!(out.system_slots.contains("- fresh distilled guidance"));
        assert!(out.system_slots.contains("- unrelated high-value insight"));
        assert!(
            !out.system_slots.contains("stale guidance"),
            "superseded record must not ride"
        );

        // No workspace → no records read, no slot.
        let out = gather_with(
            &src,
            None,
            "anything at all",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(!out.system_slots.contains("### Workspace insights"));
    }

    #[tokio::test]
    async fn workspace_memory_search_hits_outrank_importance_fillers() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::workspace::store as ws_store;
        // Six high-importance fillers + one low-importance record that
        // actually matches the user message: the search hit must ride.
        for i in 0..6 {
            ws_store::save_new("ws", &format!("filler insight {i}"), "", Some(9.0), vec![])
                .unwrap();
        }
        ws_store::save_new(
            "ws",
            "the eviction ladder orders the gather slots",
            "",
            Some(2.0),
            vec![],
        )
        .unwrap();

        let out = gather_with(
            &MockSource::default(),
            Some("ws"),
            "how does the eviction ladder order slots?",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        let slots = &out.system_slots;
        let insights_at = slots.find("### Workspace insights").expect("slot");
        // The hit leads the list — search hits come before importance
        // fillers, so the first line after the header is the match.
        let after_header = &slots[insights_at..];
        let first_line = after_header.lines().nth(1).expect("first insight line");
        assert_eq!(first_line, "- the eviction ladder orders the gather slots");
    }

    #[tokio::test]
    async fn workspace_memory_slot_kill_switch() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::workspace::store as ws_store;
        ws_store::save_new("ws", "should not ride", "", Some(8.0), vec![]).unwrap();

        std::env::set_var("WYLDE_WORKSPACE_MEMORY_SLOT", "off");
        let out = gather_with(
            &MockSource::default(),
            Some("ws"),
            "anything",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        std::env::remove_var("WYLDE_WORKSPACE_MEMORY_SLOT");
        assert!(!out.system_slots.contains("### Workspace insights"));
    }

    // ── M5: touch damping — core fillers don't self-reinforce ──────────

    #[tokio::test]
    async fn core_block_fillers_are_not_touched_by_injection() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::long_term as lt;
        // Records WITHOUT embeddings: whatever the embedder does (live
        // box embeds, headless box times out), the similarity search
        // returns no hits, so the injected lines are pure core_block —
        // and post-M5 those must NOT be re-warmed.
        let a = lt::save("core fact one", "t", Some(9.0), vec![], None).unwrap();
        let b = lt::save("core fact two", "t", Some(8.0), vec![], None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let a_before = lt::get(&a.id).unwrap().last_used_at;
        let b_before = lt::get(&b.id).unwrap().last_used_at;

        let out = gather_with(
            &MockSource::default(),
            None,
            "anything at all",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            out.system_slots.contains("- core fact one"),
            "fillers still ride: {}",
            out.system_slots
        );
        assert_eq!(
            lt::get(&a.id).unwrap().last_used_at,
            a_before,
            "filler must not be re-warmed by injection (M5)"
        );
        assert_eq!(lt::get(&b.id).unwrap().last_used_at, b_before);
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

        // M5 touch damping: these records carry no embeddings, so they
        // ride as core_block FILLERS — and fillers are no longer
        // re-warmed by injection (pre-M5 touch-everything made the
        // importance-sorted five self-reinforcing; only similarity
        // hits touch now — see `core_block_fillers_are_not_touched_by_injection`).
        let injected = entries::get(&saved[6].id).unwrap();
        // The stored timestamp round-trips f64 → JSON → f64, which can drift
        // by up to a ULP (serde_json's default parser isn't bit-exact; see
        // the round-trip check in `entries::tests`). Compare with a small
        // tolerance rather than exact identity — a real re-warm would push
        // last_used_at forward by the >=25ms slept above, dwarfing any
        // round-trip noise.
        assert!(
            (injected.last_used_at - saved[6].last_used_at).abs() < 1e-3,
            "core filler must NOT be re-warmed by injection (M5): {} vs {}",
            injected.last_used_at,
            saved[6].last_used_at,
        );
    }

    // ── C2b-read [D2]: long-term confined to unbound conversations ──────

    #[tokio::test]
    async fn long_term_gated_by_binding_bound_excludes_unbound_includes() {
        let _env = crate::user_profile::test_support::TestEnv::new();
        use crate::memory::long_term as entries;
        entries::save("durable identity fact", "test", Some(9.0), Vec::new(), None)
            .expect("save record");

        // Unbound (no workspace) → long-term injected.
        let unbound = gather_with(
            &MockSource::default(),
            None,
            "what do you remember",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            unbound.system_slots.contains("### Long-term memory"),
            "unbound conversation must receive long-term: {}",
            unbound.system_slots
        );
        assert!(unbound.system_slots.contains("durable identity fact"));

        // Bound (workspace set) → long-term slot absent entirely, even though
        // the same populated store would otherwise inject it.
        let bound = gather_with(
            &MockSource::default(),
            Some("ws"),
            "what do you remember",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            !bound.system_slots.contains("### Long-term memory"),
            "bound (workspace) conversation must NOT receive long-term: {}",
            bound.system_slots
        );
        assert!(!bound.system_slots.contains("durable identity fact"));

        // A blank workspace_id is unbound, not bound — long-term rides.
        let blank = gather_with(
            &MockSource::default(),
            Some("   "),
            "what do you remember",
            "c",
            &TokenOverrides::default(),
            token_budget::DEFAULT_TOKEN_BUDGET,
        )
        .await;
        assert!(
            blank.system_slots.contains("### Long-term memory"),
            "a blank workspace_id is unbound and must receive long-term: {}",
            blank.system_slots
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
                route_candidates: None,
                concept_context: Vec::new(),
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
