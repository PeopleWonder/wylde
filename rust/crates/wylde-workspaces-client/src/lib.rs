//! `wylde-workspaces-client` — the shared IPC client for the
//! `wylde-workspaces` service.
//!
//! Every consumer (the harness chat-turn driver, the GUI Workspaces panel,
//! the Chat composer) talks to the service through this crate instead of
//! hand-rolling the pipe. It hosts the scope v2 §7 failure-mode policy in
//! one place: per-verb timeout tiers ([`timeouts`]), retry-by-verb-shape
//! ([`retry`]), a per-pipe circuit breaker ([`circuit_breaker`]), a
//! read-through TTL cache ([`cache`]), per-consumer fallbacks ([`fallback`]),
//! and three-tier error classification ([`error`]). The per-verb knobs live
//! in the [`verbs`] table.
//!
//! **Slice 0a (this scaffold)** wires all of that infrastructure but exposes
//! exactly one verb — [`WorkspacesClient::ping`] — which proves the
//! end-to-end round-trip works. Later slices add a method per `workspaces.*`
//! verb; each is a thin wrapper over [`WorkspacesClient::call_verb`] plus a
//! one-line entry in the [`verbs`] table.

pub mod cache;
pub mod circuit_breaker;
pub mod error;
pub mod fallback;
pub mod retry;
pub mod timeouts;
pub mod transport;
pub mod verbs;

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::cache::VerbCache;
use crate::circuit_breaker::{BreakerDecision, CircuitBreaker};
use crate::error::WorkspacesClientError;

pub use crate::error::{ErrorTier, WorkspacesClientError as ClientError};

/// The `ping` reply payload: `{ok, service, version}`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PingResponse {
    pub ok: bool,
    pub service: String,
    pub version: String,
}

/// Shared client for the workspaces service.
///
/// Construct with [`WorkspacesClient::new`] (a pipe path) or
/// [`WorkspacesClient::for_service`] (a service name). One client owns one
/// circuit breaker + cache; share a client across a consumer's call sites so
/// the breaker state is coherent.
#[derive(Debug)]
pub struct WorkspacesClient {
    /// Bare service name handed to the shared transport (it re-applies the
    /// `wylde-` prefix when building the pipe path).
    service: String,
    breaker: CircuitBreaker,
    cache: VerbCache,
}

impl WorkspacesClient {
    /// Build a client pointing at the given pipe path (e.g.
    /// `\\.\pipe\wylde-workspaces`). The service name is derived from the
    /// path's final component.
    pub fn new(pipe_path: PathBuf) -> Self {
        Self::for_service(transport::service_name_from_pipe_path(&pipe_path))
    }

    /// Build a client for a service name (`wylde-workspaces`, or an isolated
    /// test name). Uses the default 5-failure / 30s breaker.
    pub fn for_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            breaker: CircuitBreaker::new(),
            cache: VerbCache::new(),
        }
    }

    /// Build a client with an explicit circuit breaker — for tests that need
    /// to tune the threshold/cooldown or pre-trip the breaker.
    pub fn with_breaker(service: impl Into<String>, breaker: CircuitBreaker) -> Self {
        Self {
            service: service.into(),
            breaker,
            cache: VerbCache::new(),
        }
    }

    /// The bare service name this client targets.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Borrow the circuit breaker (diagnostics / tests).
    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// The Slice 0a verb: a no-op liveness round-trip. Proves transport +
    /// handshake + dispatch all work end-to-end.
    pub async fn ping(&self) -> Result<PingResponse, WorkspacesClientError> {
        let data = self.call_verb("ping", Value::Null, 1).await?;
        serde_json::from_value(data)
            .map_err(|e| WorkspacesClientError::decode(format!("ping reply: {e}")))
    }

    // ── Slice 0b verb wrappers ─────────────────────────────────────────
    //
    // Thin, one-line wrappers over `call_verb`. They return the raw `data`
    // payload (the shapes are documented in `wylde_workspaces::api` /
    // `action_dispatch`); the harness compat-shim proxy forwards the original
    // request payload through `call_verb` directly, so these exist for typed
    // consumers (the GUI panel in later slices) and the client test suite.

    /// `workspaces.list_mru` — MRU-5 workspaces + active id.
    pub async fn list_mru(&self) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.list_mru", Value::Null, 1).await
    }

    /// `workspaces.set_active` — activate a workspace + bump MRU.
    pub async fn set_active(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.set_active",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    /// `workspaces.create` — register (and activate) a folder as a workspace.
    pub async fn create(
        &self,
        folder: &str,
        name: Option<&str>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({ "folder": folder });
        if let Some(n) = name {
            payload["name"] = serde_json::Value::String(n.to_owned());
        }
        self.call_verb("workspaces.create", payload, 1).await
    }

    /// `workspaces.update` — rename / toggle feature flags. Pass the full
    /// `{workspace_id, name?, persona_enabled?, rag_enabled?}` payload.
    pub async fn update(&self, payload: Value) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.update", payload, 1).await
    }

    /// `workspaces.delete` — remove a workspace + its data dir.
    pub async fn delete(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.delete",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    /// `workspaces.set_persona` — write `persona.md`.
    pub async fn set_persona(
        &self,
        workspace_id: &str,
        text: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.set_persona",
            serde_json::json!({ "workspace_id": workspace_id, "text": text }),
            1,
        )
        .await
    }

    /// `workspaces.rag_query` — k-NN search over a workspace's file index.
    pub async fn rag_query(
        &self,
        workspace_id: &str,
        query: &str,
        k: Option<u64>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({ "workspace_id": workspace_id, "query": query });
        if let Some(k) = k {
            payload["k"] = serde_json::Value::from(k);
        }
        self.call_verb("workspaces.rag_query", payload, 1).await
    }

    /// `workspaces.reindex` — force a synchronous full reindex.
    pub async fn reindex(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.reindex",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    // ── Slice B — code graph read API ───────────────────────────────────

    /// `workspaces.graph` — the workspace's code graph (`{nodes, edges,
    /// clusters}`), read live from Neo4j.
    ///
    /// Returns the raw reply payload (the typed `WorkspaceGraph` model lives
    /// in `wylde_workspaces::graph` / the GUI graph panel; this crate stays
    /// decoupled from it, matching the other wrappers). Results are served
    /// from a 5s read-through cache; an unreachable backend surfaces the
    /// underlying `bolt_*` error for the consumer's graph-tab fallback.
    pub async fn graph(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.graph",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    // ── Slice F-data — symbol index read API ────────────────────────────

    /// `workspaces.symbols.find` — resolve `query` to symbols in
    /// `workspace_id` (exact-first, then fuzzy), capped at `limit`
    /// (service default: 20). Returns the raw reply `{query, matches}`; the
    /// typed `SymbolMatch` model lives in `wylde_workspaces::graph`, keeping
    /// this crate decoupled from it (as with `graph`).
    ///
    /// Served from a 60s read-through cache (Plan v2 §7.6) so the composer's
    /// per-keystroke highlighting (Slice F-visual) re-queries cheaply; a
    /// repeated `(workspace_id, query, limit)` within the TTL skips the pipe.
    pub async fn symbols_find(
        &self,
        workspace_id: &str,
        query: &str,
        limit: Option<u64>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({
            "workspace_id": workspace_id,
            "query": query,
        });
        if let Some(l) = limit {
            payload["limit"] = serde_json::Value::from(l);
        }
        self.call_verb("workspaces.symbols.find", payload, 1).await
    }

    // ── Slice G-data — symbol context read API ──────────────────────────

    /// `workspaces.symbol_context` — one symbol's structural context (body +
    /// callers + callees + types used + file siblings), read live from Neo4j.
    ///
    /// `hops` (default 1) walks the call graph that many steps; it is also
    /// what sets this call's timeout — the per-hop budget `200ms + 300ms × N`
    /// (OI-1) — so the client passes it straight through to [`call_verb`].
    /// `include_body` (default true) loads the focal's source body.
    /// `include_blame` (Slice L) folds recent git blame for the focal's body
    /// lines into the reply (tracked files only; fail-soft) — latency-aware
    /// callers on the chat hot path pass `false` to skip the git subprocess.
    ///
    /// Returns the raw `SymbolContext` reply payload (the typed model lives in
    /// `wylde_workspaces::graph::neighborhood`; this crate stays decoupled
    /// from it, like the other wrappers). A `not_found` code means the symbol
    /// isn't in the workspace; `bolt_*` codes feed the consumer's fallback.
    pub async fn symbol_context(
        &self,
        workspace_id: &str,
        symbol_id: &str,
        hops: Option<u32>,
        include_body: bool,
        include_blame: bool,
    ) -> Result<Value, WorkspacesClientError> {
        let hops = hops.unwrap_or(1).max(1);
        self.call_verb(
            "workspaces.symbol_context",
            serde_json::json!({
                "workspace_id": workspace_id,
                "symbol_id": symbol_id,
                "hops": hops,
                "include_body": include_body,
                "include_blame": include_blame,
            }),
            hops,
        )
        .await
    }

    // ── Slice 0d — chat-turn prompt context ─────────────────────────────

    /// `workspaces.gather_prompt` — the rendered system-prompt slot block
    /// for `workspace_id` against `user_message` (persona + notes + RAG).
    ///
    /// Returns the ready-to-append `slots` string (empty when the workspace
    /// contributes nothing or the id is unknown). The chat turn driver
    /// calls this once per turn as best-effort enrichment; on a transport
    /// failure / open breaker it degrades to base context (no slots).
    pub async fn gather_prompt(
        &self,
        workspace_id: &str,
        user_message: &str,
    ) -> Result<String, WorkspacesClientError> {
        let data = self.gather_prompt_raw(workspace_id, user_message).await?;
        Ok(data
            .get("slots")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned())
    }

    /// `workspaces.gather_prompt`, full reply — `{slots, persona,
    /// memory_snippets, rag_snippets}`. The harness gather consumes the
    /// structured fields so persona / notes / RAG map onto separate
    /// eviction tiers instead of one opaque block (improvement plan B6);
    /// [`Self::gather_prompt`] remains the rendered-string convenience.
    pub async fn gather_prompt_raw(
        &self,
        workspace_id: &str,
        user_message: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.gather_prompt",
            serde_json::json!({
                "workspace_id": workspace_id,
                "user_message": user_message,
            }),
            1,
        )
        .await
    }

    // ── Slice 0c — workspace notes tier ─────────────────────────────────

    /// `workspaces.notes.list` — every note for a workspace.
    pub async fn notes_list(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.notes.list",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    /// `workspaces.notes.add` — append a note (embeds on write).
    pub async fn notes_add(
        &self,
        workspace_id: &str,
        text: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.notes.add",
            serde_json::json!({ "workspace_id": workspace_id, "text": text }),
            1,
        )
        .await
    }

    /// `workspaces.notes.update` — edit a note's text (re-embeds).
    pub async fn notes_update(
        &self,
        workspace_id: &str,
        id: &str,
        text: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.notes.update",
            serde_json::json!({ "workspace_id": workspace_id, "id": id, "text": text }),
            1,
        )
        .await
    }

    /// `workspaces.notes.delete` — remove a note by id.
    pub async fn notes_delete(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.notes.delete",
            serde_json::json!({ "workspace_id": workspace_id, "id": id }),
            1,
        )
        .await
    }

    /// `workspaces.notes.search` — recency+relevance ranked note search.
    pub async fn notes_search(
        &self,
        workspace_id: &str,
        query: &str,
        limit: Option<u64>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({ "workspace_id": workspace_id, "query": query });
        if let Some(l) = limit {
            payload["limit"] = serde_json::Value::from(l);
        }
        self.call_verb("workspaces.notes.search", payload, 1).await
    }

    /// `workspaces.notes.propose` — reflection candidate (not persisted).
    pub async fn notes_propose(
        &self,
        workspace_id: &str,
        text: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.notes.propose",
            serde_json::json!({ "workspace_id": workspace_id, "text": text }),
            1,
        )
        .await
    }

    // ── Slice 0c — workspace-scoped conversations ───────────────────────

    /// `workspaces.conversations.list` — metadata for one workspace.
    pub async fn conversations_list(
        &self,
        workspace_id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.conversations.list",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    /// `workspaces.conversations.get` — the full conversation document.
    pub async fn conversations_get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.conversations.get",
            serde_json::json!({ "workspace_id": workspace_id, "id": id }),
            1,
        )
        .await
    }

    /// `workspaces.conversations.delete` — remove one workspace conversation.
    pub async fn conversations_delete(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.conversations.delete",
            serde_json::json!({ "workspace_id": workspace_id, "id": id }),
            1,
        )
        .await
    }

    /// `chat.export` (Slice J) — one workspace conversation as a portable
    /// envelope. Reply `{export, id}`; persisting the file is the caller's
    /// concern.
    pub async fn chat_export(
        &self,
        workspace_id: &str,
        conversation_id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "chat.export",
            serde_json::json!({
                "workspace_id": workspace_id,
                "conversation_id": conversation_id,
            }),
            1,
        )
        .await
    }

    /// `chat.import` (Slice J) — land a portable envelope in a workspace. An
    /// id collision replies `already_exists` unless `overwrite`.
    pub async fn chat_import(
        &self,
        workspace_id: &str,
        export: Value,
        overwrite: bool,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "chat.import",
            serde_json::json!({
                "workspace_id": workspace_id,
                "export": export,
                "overwrite": overwrite,
            }),
            1,
        )
        .await
    }

    /// `workspaces.conversations.refresh_summary` — persist an LLM summary +
    /// embedding the harness computed for a workspace conversation (Slice E
    /// parity). The service folds the derived fields into the stored doc so the
    /// scoped semantic search can rank it by cosine, like standalone convos.
    #[allow(clippy::too_many_arguments)]
    pub async fn conversations_refresh_summary(
        &self,
        workspace_id: &str,
        conversation_id: &str,
        summary: &str,
        topic_tags: &[String],
        embedding: &[f32],
        summary_msg_count: u64,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.conversations.refresh_summary",
            serde_json::json!({
                "workspace_id": workspace_id,
                "conversation_id": conversation_id,
                "summary": summary,
                "topic_tags": topic_tags,
                "embedding": embedding.iter().map(|x| *x as f64).collect::<Vec<f64>>(),
                "summary_msg_count": summary_msg_count,
            }),
            1,
        )
        .await
    }

    // ── Slice I — file watcher control ──────────────────────────────────

    /// `workspaces.watcher.status` — the file-watcher observability snapshot
    /// (`{active_workspace, files_watched, last_event_at, paused}`).
    pub async fn watcher_status(&self) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.watcher.status", Value::Null, 1)
            .await
    }

    /// `workspaces.watcher.pause` — pause the active workspace's watcher.
    pub async fn watcher_pause(&self) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.watcher.pause", Value::Null, 1)
            .await
    }

    /// `workspaces.watcher.resume` — resume + re-walk to catch up.
    pub async fn watcher_resume(&self) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.watcher.resume", Value::Null, 1)
            .await
    }

    // ── Slice N-data — workspace anchor store ───────────────────────────
    //
    // Thin wrappers over `call_verb` returning the raw reply payload (the
    // typed `Anchor` model lives in `wylde_shared::anchor`; this crate stays
    // decoupled, like the other wrappers). The global `anchors.*` (Global
    // scope) verbs are in-process on the harness pipe — they have no client
    // wrapper here.

    /// `workspaces.anchors.list` — every anchor for a workspace (30s cache).
    pub async fn anchors_list(&self, workspace_id: &str) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.list",
            serde_json::json!({ "workspace_id": workspace_id }),
            1,
        )
        .await
    }

    /// `workspaces.anchors.create` — mint a workspace anchor. Pass the full
    /// `{workspace_id, identifier, kind?, target, description?, parent_anchor?,
    /// domain?, related_to?}` payload. An `already_exists` code (details carry
    /// the existing definition) signals a duplicate identifier.
    pub async fn anchors_create(&self, payload: Value) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.anchors.create", payload, 1)
            .await
    }

    /// `workspaces.anchors.update` — patch an anchor. Pass the full
    /// `{workspace_id, identifier, ...patch}` payload.
    pub async fn anchors_update(&self, payload: Value) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.anchors.update", payload, 1)
            .await
    }

    /// `workspaces.anchors.delete` — remove an anchor by identifier.
    pub async fn anchors_delete(
        &self,
        workspace_id: &str,
        identifier: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.delete",
            serde_json::json!({ "workspace_id": workspace_id, "identifier": identifier }),
            1,
        )
        .await
    }

    /// `workspaces.anchors.find_by_token` — resolve `{{token}}` (or bare name)
    /// → anchors. The composer's per-keystroke recognition call.
    pub async fn anchors_find_by_token(
        &self,
        workspace_id: &str,
        token: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.find_by_token",
            serde_json::json!({ "workspace_id": workspace_id, "token": token }),
            1,
        )
        .await
    }

    /// `workspaces.anchors.find_by_target` — inverse lookup `symbol_id` →
    /// anchors (OI-20).
    pub async fn anchors_find_by_target(
        &self,
        workspace_id: &str,
        symbol_id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.find_by_target",
            serde_json::json!({ "workspace_id": workspace_id, "symbol_id": symbol_id }),
            1,
        )
        .await
    }

    /// `workspaces.anchors.list_under` — anchors under a taxonomy parent
    /// (OI-19 hierarchy).
    pub async fn anchors_list_under(
        &self,
        workspace_id: &str,
        parent_id: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.list_under",
            serde_json::json!({ "workspace_id": workspace_id, "parent_id": parent_id }),
            1,
        )
        .await
    }

    /// `workspaces.anchors.propose` — an LLM reflection candidate (NOT
    /// persisted). Pass the full `{workspace_id, identifier, target, ...,
    /// confidence?, proposals_so_far?, last_proposal_at?}` payload.
    pub async fn anchors_propose(&self, payload: Value) -> Result<Value, WorkspacesClientError> {
        self.call_verb("workspaces.anchors.propose", payload, 1)
            .await
    }

    /// `workspaces.anchors.promote_via_alias` — request whole-anchor promotion
    /// because the user acted on `alias` (Slice N-data-aliases). Returns the
    /// promotion payload `{anchor, via_alias, promote}` for the caller to land
    /// via the global `anchors.promote_via_alias`. Fast · NoRetry (promotion is
    /// non-idempotent, always user-confirmed).
    pub async fn anchors_promote_via_alias(
        &self,
        workspace_id: &str,
        anchor_id: &str,
        alias: &str,
    ) -> Result<Value, WorkspacesClientError> {
        self.call_verb(
            "workspaces.anchors.promote_via_alias",
            serde_json::json!({
                "workspace_id": workspace_id,
                "anchor_id": anchor_id,
                "alias": alias,
            }),
            1,
        )
        .await
    }

    // ── Slice M — symbol ignore list (workspace + conversation tiers) ────

    /// `workspaces.ignore.list` — both service-side tiers; the conversation
    /// tier scoped to `conversation_id` (pass `None` for workspace-only).
    pub async fn ignore_list(
        &self,
        workspace_id: &str,
        conversation_id: Option<&str>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({ "workspace_id": workspace_id });
        if let Some(c) = conversation_id.filter(|c| !c.is_empty()) {
            payload["conversation_id"] = serde_json::Value::from(c);
        }
        self.call_verb("workspaces.ignore.list", payload, 1).await
    }

    /// `workspaces.ignore.add` — ignore `token` in `tier`
    /// (`"workspace" | "conversation"`); idempotent (re-adds succeed with
    /// `added: false`).
    pub async fn ignore_add(
        &self,
        workspace_id: &str,
        tier: &str,
        token: &str,
        conversation_id: Option<&str>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({
            "workspace_id": workspace_id,
            "tier": tier,
            "token": token,
        });
        if let Some(c) = conversation_id.filter(|c| !c.is_empty()) {
            payload["conversation_id"] = serde_json::Value::from(c);
        }
        self.call_verb("workspaces.ignore.add", payload, 1).await
    }

    /// `workspaces.ignore.remove` — stop ignoring `token` in `tier`.
    pub async fn ignore_remove(
        &self,
        workspace_id: &str,
        tier: &str,
        token: &str,
        conversation_id: Option<&str>,
    ) -> Result<Value, WorkspacesClientError> {
        let mut payload = serde_json::json!({
            "workspace_id": workspace_id,
            "tier": tier,
            "token": token,
        });
        if let Some(c) = conversation_id.filter(|c| !c.is_empty()) {
            payload["conversation_id"] = serde_json::Value::from(c);
        }
        self.call_verb("workspaces.ignore.remove", payload, 1).await
    }

    /// Drive one verb call through the full resilience pipeline: cache →
    /// breaker → timed transport attempt(s) with retry → breaker bookkeeping.
    ///
    /// `hops` only matters for per-hop-budget verbs (`symbol_context`); pass
    /// `1` for everything else. Returns the verb's raw `data` payload on
    /// success.
    pub async fn call_verb(
        &self,
        verb: &str,
        payload: Value,
        hops: u32,
    ) -> Result<Value, WorkspacesClientError> {
        let def = verbs::lookup(verb).ok_or_else(|| WorkspacesClientError::unknown_verb(verb))?;

        // 1. Read-through cache (verbs with a TTL only).
        let cache_key = VerbCache::key(verb, &payload);
        if def.cache_ttl.is_some() {
            if let Some(hit) = self.cache.get(&cache_key) {
                return Ok(hit);
            }
        }

        // 2. Circuit breaker — fail fast when open.
        if let BreakerDecision::Open = self.breaker.check(verb) {
            return Err(WorkspacesClientError::breaker_open(verb));
        }

        // 3. Timed attempt(s) with retry-on-transport-failure.
        let timeout = def.timeout.budget(hops);
        let max_attempts = def.retry.max_attempts();
        let mut last_err: Option<WorkspacesClientError> = None;

        for attempt in 1..=max_attempts {
            let reply = transport::call_action(&self.service, verb, payload.clone(), timeout).await;

            if reply.ok {
                self.breaker.record_success(verb);
                if let Some(ttl) = def.cache_ttl {
                    self.cache.put(cache_key, reply.data.clone(), ttl);
                }
                return Ok(reply.data);
            }

            let err = WorkspacesClientError::from_ipc(reply.error.unwrap_or_else(|| {
                wylde_shared::ipc::IpcError::new("unknown", "ok=false reply with no error body")
            }));

            // Application errors (no_action, bad_request, …) mean the service
            // is healthy — don't retry, don't trip the breaker.
            if !err.transport {
                return Err(err);
            }

            last_err = Some(err);
            if let Some(delay) = def.retry.backoff_delay(attempt) {
                tokio::time::sleep(delay).await;
            }
        }

        // Retries exhausted on a transport failure → count one failed
        // operation against the breaker and surface the last error.
        self.breaker.record_failure(verb);
        Err(last_err.unwrap_or_else(|| {
            WorkspacesClientError::from_ipc(wylde_shared::ipc::IpcError::new(
                "pipe_io",
                "transport failed with no recorded error",
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_derives_service_from_pipe_path() {
        let c = WorkspacesClient::new(PathBuf::from(r"\\.\pipe\wylde-workspaces"));
        assert_eq!(c.service(), "wylde-workspaces");
    }

    #[test]
    fn for_service_keeps_name() {
        let c = WorkspacesClient::for_service("wylde-workspaces-test-xyz");
        assert_eq!(c.service(), "wylde-workspaces-test-xyz");
    }
}
