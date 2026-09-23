//! Central action routing for `wylde-workspaces`.
//!
//! Mirrors the `service::install` pattern every other Rust service uses
//! (`wylde-ollama::service`, `wylde-vram-broker::service`): register the
//! action surface on the process-wide shared registry, then let the shared
//! pipe server dispatch `/__action__` frames into it. Unknown actions get
//! the shared dispatcher's `no_action` reply for free — the same code every
//! service emits — so we don't reinvent routing.
//!
//! Slice 0a registered exactly one verb: [`PING`]. Slice 0b adds the
//! relocated workspace verb surface ([`crate::api`]) — registry CRUD +
//! active-selection, persona write, and the RAG query / reindex verbs — so
//! the new pipe natively serves everything the harness pipe used to. The
//! harness keeps the same verbs as a thin proxy (compat shim) during the
//! migration window; both pipes answer the same `workspaces.*` names.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, Reply};

use crate::api;

const META_MODULE: &str = "wylde_workspaces::action_dispatch";

/// A no-op verb that proves the transport works.
pub const PING: &str = "ping";

// ── Relocated workspace verbs (Slice 0b) ─────────────────────────────────
pub const SET_ACTIVE: &str = "workspaces.set_active";
pub const CREATE: &str = "workspaces.create";
pub const UPDATE: &str = "workspaces.update";
pub const DELETE: &str = "workspaces.delete";
pub const SET_PERSONA: &str = "workspaces.set_persona";
pub const LIST_MRU: &str = "workspaces.list_mru";
pub const LIST_ALL: &str = "workspaces.list_all";
pub const RAG_QUERY: &str = "workspaces.rag_query";
pub const REINDEX: &str = "workspaces.reindex";

// ── Index hygiene (P1) — exclusion purge + dry-run preview ───────────────
pub const REINDEX_PURGE: &str = "workspaces.reindex_purge";
pub const WALK_PREVIEW: &str = "workspaces.rag.walk_preview";

// ── Lexical/BM25 + RRF master toggle (lexical-bm25 plan L0) ───────────────
pub const LEXICAL_GET: &str = "settings.lexical.get";
pub const LEXICAL_SET: &str = "settings.lexical.set";

// ── Chat-turn prompt context (Slice 0d — relocated from the harness) ─────
pub const GATHER_PROMPT: &str = "workspaces.gather_prompt";

// ── Code graph read API (Slice B — Phase 1) ──────────────────────────────
pub const GRAPH: &str = "workspaces.graph";

// ── Symbol index read API (Slice F-data — Phase 1) ───────────────────────
pub const SYMBOLS_FIND: &str = "workspaces.symbols.find";

// ── Symbol context read API (Slice G-data — Phase 1) ─────────────────────
pub const SYMBOL_CONTEXT: &str = "workspaces.symbol_context";

// ── Concept system (TBS concept-system — Phase 0) ────────────────────────
pub const CONCEPTS_LIST: &str = "workspaces.concepts.list";
pub const CONCEPTS_GET: &str = "workspaces.concepts.get";
pub const CONCEPTS_BUILD: &str = "workspaces.concepts.build";
pub const CONCEPTS_CREATE: &str = "workspaces.concepts.create";
pub const CONCEPTS_UPDATE: &str = "workspaces.concepts.update";
pub const CONCEPTS_DELETE: &str = "workspaces.concepts.delete";
pub const CONCEPTS_LIST_UNDER: &str = "workspaces.concepts.list_under";
pub const CONCEPTS_REVERSE_LOOKUP: &str = "workspaces.concepts.reverse_lookup";
pub const CONCEPTS_SEARCH: &str = "workspaces.concepts.search";
// Phase 2 — semantic build + curation loop
pub const CONCEPTS_BUILD_SEMANTIC: &str = "workspaces.concepts.build_semantic";
pub const CONCEPTS_PROPOSE: &str = "workspaces.concepts.propose";
pub const CONCEPTS_LIST_PROPOSALS: &str = "workspaces.concepts.list_proposals";
pub const CONCEPTS_ACCEPT_PROPOSAL: &str = "workspaces.concepts.accept_proposal";
pub const CONCEPTS_REJECT_PROPOSAL: &str = "workspaces.concepts.reject_proposal";
// Phase 3 — concept-driven retrieval (routing deferred)
pub const CONCEPTS_LENS: &str = "workspaces.concepts.lens";
pub const CONCEPTS_RETRIEVE: &str = "workspaces.concepts.retrieve";
// Phase 4 — freshness / drift
pub const CONCEPTS_FRESHNESS: &str = "workspaces.concepts.freshness";
// Concept-routing R1.5a — typed relation store (deletable relations_bridge)
pub const CONCEPTS_RELATIONS_LIST: &str = "workspaces.concepts.relations.list";
pub const CONCEPTS_RELATIONS_GRAPH: &str = "workspaces.concepts.relations.graph";
pub const CONCEPTS_RELATIONS_ADD: &str = "workspaces.concepts.relations.add";
pub const CONCEPTS_RELATIONS_REMOVE: &str = "workspaces.concepts.relations.remove";

// Definitional concept hierarchy H1 — the deletable overlay verbs (hierarchy_bridge)
pub const HIERARCHY_GET_TREE: &str = "workspaces.hierarchy.get_tree";
pub const HIERARCHY_GET_NODE: &str = "workspaces.hierarchy.get_node";
pub const HIERARCHY_SET_DEFINITION: &str = "workspaces.hierarchy.set_definition";
pub const HIERARCHY_ADD_EDGE: &str = "workspaces.hierarchy.add_edge";
pub const HIERARCHY_REMOVE_EDGE: &str = "workspaces.hierarchy.remove_edge";
pub const HIERARCHY_MERGE_NODES: &str = "workspaces.hierarchy.merge_nodes";
pub const HIERARCHY_REMOVE_MERGE: &str = "workspaces.hierarchy.remove_merge";
pub const HIERARCHY_GET_CONFIG: &str = "workspaces.hierarchy.get_config";
pub const HIERARCHY_SET_ENABLED: &str = "workspaces.hierarchy.set_enabled";
pub const HIERARCHY_GET_OVERLAY: &str = "workspaces.hierarchy.get_overlay";

// ── File I/O — jailed editor/file-tree surface (S1 / IDE plan P0.2) ──────
pub const FS_READ: &str = "workspaces.fs.read";
pub const FS_WRITE: &str = "workspaces.fs.write";
pub const FS_LIST_DIR: &str = "workspaces.fs.list_dir";

// ── Workspace notes tier (Slice 0c) ──────────────────────────────────────
pub const NOTES_LIST: &str = "workspaces.notes.list";
pub const NOTES_ADD: &str = "workspaces.notes.add";
pub const NOTES_UPDATE: &str = "workspaces.notes.update";
pub const NOTES_DELETE: &str = "workspaces.notes.delete";
pub const NOTES_SEARCH: &str = "workspaces.notes.search";
pub const NOTES_PROPOSE: &str = "workspaces.notes.propose";

// ── Workspace-scoped conversations (Slice 0c) ────────────────────────────
pub const CONVERSATIONS_LIST: &str = "workspaces.conversations.list";
pub const CONVERSATIONS_GET: &str = "workspaces.conversations.get";
pub const CONVERSATIONS_DELETE: &str = "workspaces.conversations.delete";
pub const CONVERSATIONS_REFRESH_SUMMARY: &str = "workspaces.conversations.refresh_summary";
// Slice J escape hatch — `chat.*` names per Plan v2 Appendix A (the
// conversations api owns chat.{export,import} on THIS service's pipe).
pub const CHAT_EXPORT: &str = "chat.export";
pub const CHAT_IMPORT: &str = "chat.import";

// ── File watcher control (Slice I) ───────────────────────────────────────
pub const WATCHER_STATUS: &str = "workspaces.watcher.status";
pub const WATCHER_PAUSE: &str = "workspaces.watcher.pause";
pub const WATCHER_RESUME: &str = "workspaces.watcher.resume";

// ── Workspace anchor store (Slice N-data — Phase 2) ──────────────────────
pub const ANCHORS_LIST: &str = "workspaces.anchors.list";
pub const ANCHORS_CREATE: &str = "workspaces.anchors.create";
pub const ANCHORS_UPDATE: &str = "workspaces.anchors.update";
pub const ANCHORS_DELETE: &str = "workspaces.anchors.delete";
pub const ANCHORS_FIND_BY_TOKEN: &str = "workspaces.anchors.find_by_token";
pub const ANCHORS_FIND_BY_TARGET: &str = "workspaces.anchors.find_by_target";
pub const ANCHORS_LIST_UNDER: &str = "workspaces.anchors.list_under";
pub const ANCHORS_PROPOSE: &str = "workspaces.anchors.propose";
pub const ANCHORS_PROMOTE_VIA_ALIAS: &str = "workspaces.anchors.promote_via_alias";

// ── Symbol ignore list — workspace + conversation tiers (Slice M) ────────
pub const IGNORE_LIST: &str = "workspaces.ignore.list";
pub const IGNORE_ADD: &str = "workspaces.ignore.add";
pub const IGNORE_REMOVE: &str = "workspaces.ignore.remove";

// ── LLM anchor proposals — the review queue (Slice N) ────────────────────
pub const ANCHORS_LIST_PROPOSALS: &str = "workspaces.anchors.list_proposals";
pub const ANCHORS_ACCEPT_PROPOSAL: &str = "workspaces.anchors.accept_proposal";
pub const ANCHORS_REJECT_PROPOSAL: &str = "workspaces.anchors.reject_proposal";

/// Every action this service registers. Grows one slice at a time.
pub const ALL_ACTIONS: &[&str] = &[
    PING,
    SET_ACTIVE,
    CREATE,
    UPDATE,
    DELETE,
    SET_PERSONA,
    LIST_MRU,
    LIST_ALL,
    RAG_QUERY,
    REINDEX,
    // Index hygiene (P1)
    REINDEX_PURGE,
    WALK_PREVIEW,
    // lexical-bm25 plan L0 — lexical/RRF master toggle
    LEXICAL_GET,
    LEXICAL_SET,
    // Slice 0d — chat-turn prompt context
    GATHER_PROMPT,
    // Slice B — code graph read API
    GRAPH,
    // Slice F-data — symbol index read API
    SYMBOLS_FIND,
    // Slice G-data — symbol context read API
    SYMBOL_CONTEXT,
    // TBS concept-system — Phase 0
    CONCEPTS_LIST,
    CONCEPTS_GET,
    CONCEPTS_BUILD,
    CONCEPTS_CREATE,
    CONCEPTS_UPDATE,
    CONCEPTS_DELETE,
    CONCEPTS_LIST_UNDER,
    CONCEPTS_REVERSE_LOOKUP,
    CONCEPTS_SEARCH,
    CONCEPTS_BUILD_SEMANTIC,
    CONCEPTS_PROPOSE,
    CONCEPTS_LIST_PROPOSALS,
    CONCEPTS_ACCEPT_PROPOSAL,
    CONCEPTS_REJECT_PROPOSAL,
    CONCEPTS_LENS,
    CONCEPTS_RETRIEVE,
    CONCEPTS_FRESHNESS,
    // Concept-routing R1.5a — typed relation store
    CONCEPTS_RELATIONS_LIST,
    CONCEPTS_RELATIONS_GRAPH,
    CONCEPTS_RELATIONS_ADD,
    CONCEPTS_RELATIONS_REMOVE,
    // S1 (IDE plan P0.2) — jailed file I/O
    FS_READ,
    FS_WRITE,
    FS_LIST_DIR,
    // Slice 0c — notes
    NOTES_LIST,
    NOTES_ADD,
    NOTES_UPDATE,
    NOTES_DELETE,
    NOTES_SEARCH,
    NOTES_PROPOSE,
    // Slice 0c — workspace conversations
    CONVERSATIONS_LIST,
    CONVERSATIONS_GET,
    CONVERSATIONS_DELETE,
    // Registered and handled since Slice 0c but absent from this table until
    // #130 — the reverse-direction gate caught it. Without the entry it leaked
    // past reset_for_tests and the gpui-contract lint would flag its callers.
    CONVERSATIONS_REFRESH_SUMMARY,
    // Concept-hierarchy overlay — registered in install() but absent from this
    // table until #130 (all ten leaked past reset_for_tests and were invisible
    // to the gpui-contract lint).
    HIERARCHY_GET_TREE,
    HIERARCHY_GET_NODE,
    HIERARCHY_SET_DEFINITION,
    HIERARCHY_ADD_EDGE,
    HIERARCHY_REMOVE_EDGE,
    HIERARCHY_MERGE_NODES,
    HIERARCHY_REMOVE_MERGE,
    HIERARCHY_GET_CONFIG,
    HIERARCHY_SET_ENABLED,
    HIERARCHY_GET_OVERLAY,
    // Slice I — file watcher control
    WATCHER_STATUS,
    WATCHER_PAUSE,
    WATCHER_RESUME,
    // Slice N-data — workspace anchor store
    ANCHORS_LIST,
    ANCHORS_CREATE,
    ANCHORS_UPDATE,
    ANCHORS_DELETE,
    ANCHORS_FIND_BY_TOKEN,
    ANCHORS_FIND_BY_TARGET,
    ANCHORS_LIST_UNDER,
    ANCHORS_PROPOSE,
    ANCHORS_PROMOTE_VIA_ALIAS,
    // Slice M — symbol ignore list (workspace + conversation tiers)
    IGNORE_LIST,
    IGNORE_ADD,
    IGNORE_REMOVE,
    // Slice N — LLM anchor-proposal review queue
    ANCHORS_LIST_PROPOSALS,
    ANCHORS_ACCEPT_PROPOSAL,
    ANCHORS_REJECT_PROPOSAL,
    // Slice J — conversation export / import (the escape hatch)
    CHAT_EXPORT,
    CHAT_IMPORT,
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `wylde-workspaces` action on the shared registry.
/// Idempotent — repeat calls are no-ops, matching the broker/ollama shape.
///
/// Must run before `serve()` so the registry is populated when the first
/// pipe client connects.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        PING,
        |_payload: Value| async move { handle_ping() },
        "Liveness proof. Reply: {ok: true, service: \"wylde-workspaces\", version: <crate version>}.",
        META_MODULE,
    );

    register_action_with_meta(
        SET_ACTIVE,
        |p: Value| async move { api::handle_set_active(p).await },
        "Set the active workspace + bump MRU. Payload: {workspace_id}. \
         Reply: {active_id, mru}.",
        META_MODULE,
    );
    register_action_with_meta(
        CREATE,
        |p: Value| async move { api::handle_create(p).await },
        "Register a folder as a workspace (and activate it). Payload: \
         {folder, name?}. Reply: WorkspaceDefinition.",
        META_MODULE,
    );
    register_action_with_meta(
        UPDATE,
        |p: Value| async move { api::handle_update(p).await },
        "Rename / toggle persona_enabled / rag_enabled. Payload: \
         {workspace_id, name?, persona_enabled?, rag_enabled?}. Reply: \
         WorkspaceDefinition.",
        META_MODULE,
    );
    register_action_with_meta(
        DELETE,
        |p: Value| async move { api::handle_delete(p).await },
        "Remove a workspace + its data dir. Payload: {workspace_id}. \
         Reply: {ok, workspace_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        SET_PERSONA,
        |p: Value| async move { api::handle_set_persona(p).await },
        "Write persona.md for a workspace. Payload: {workspace_id, text?}. \
         Reply: {ok, workspace_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        LIST_MRU,
        |p: Value| async move { api::handle_list_mru(p).await },
        "MRU-5 workspace list + active id. No payload. Reply: \
         {workspaces: [WorkspaceDefinition], active_id}.",
        META_MODULE,
    );
    register_action_with_meta(
        LIST_ALL,
        |p: Value| async move { api::handle_list_all(p).await },
        "Every workspace on disk (disk-walk, not just the MRU-5 window); \
         surfaces bundles the index lost or never knew about and reconciles \
         stale entries (#134). No payload. Reply: {workspaces: \
         [WorkspaceDefinition]}.",
        META_MODULE,
    );
    register_action_with_meta(
        RAG_QUERY,
        |p: Value| async move { api::handle_rag_query(p).await },
        "k-NN search over a workspace's file index. Payload: \
         {workspace_id, query, k?}. Reply: {workspace_id, hits}. Fail-soft \
         to empty hits.",
        META_MODULE,
    );
    register_action_with_meta(
        REINDEX,
        |p: Value| async move { api::handle_reindex(p).await },
        "Force a synchronous full reindex of a workspace's folder. Payload: \
         {workspace_id}. Reply: {ok, file_count, chunk_count, last_error}.",
        META_MODULE,
    );
    register_action_with_meta(
        REINDEX_PURGE,
        |p: Value| async move { api::handle_reindex_purge(p).await },
        "One-time index-hygiene purge: drop already-indexed chunks whose path \
         the exclusion matcher now excludes (build artifacts like target-dev/ \
         rustdoc), filter-only (no re-embed). Payload: {workspace_id}. Reply: \
         {ok, workspace_id, before, dropped, kept, files_dropped, \
         excluded_remaining, graph_cleaned, graph_error?}. Idempotent. \
         Re-cluster concepts after via concepts.build_semantic.",
        META_MODULE,
    );
    register_action_with_meta(
        WALK_PREVIEW,
        |p: Value| async move { api::handle_walk_preview(p).await },
        "Read-only dry-run of the walk-time exclusion over a workspace folder \
         (de-risks a purge). Payload: {workspace_id, sample?=20}. Reply: \
         {workspace_id, would_index, would_exclude, sample_excluded:[paths]}. \
         No embed, no persist.",
        META_MODULE,
    );

    register_action_with_meta(
        LEXICAL_GET,
        |p: Value| async move { api::handle_lexical_get(p).await },
        "Read the lexical/BM25 + RRF master toggle + fusion knobs. Payload: {}. \
         Reply: {enabled, rrf_k, w_dense, w_lex, min_bm25, fused_relative_floor, \
         active_file_focus_boost, active_file_dir_focus_boost}. Default-off.",
        META_MODULE,
    );
    register_action_with_meta(
        LEXICAL_SET,
        |p: Value| async move { api::handle_lexical_set(p).await },
        "Persist the lexical/RRF config (partial patch — omitted fields keep \
         their current value). Payload: any subset of the get-reply keys. Reply: \
         the persisted config. Master toggle defaults off; only an explicit \
         opt-in here turns the lexical arm on.",
        META_MODULE,
    );

    register_action_with_meta(
        GATHER_PROMPT,
        |p: Value| async move { api::handle_gather_prompt(p).await },
        "Resolve a workspace's contribution to a chat turn's system prompt \
         (persona + notes + RAG). Payload: {workspace_id, user_message?}. \
         Reply: {workspace_id, slots, persona, memory_snippets, \
         rag_snippets}. `slots` is the ready-to-append rendered block; \
         empty for an unknown/blank workspace.",
        META_MODULE,
    );

    // ── Slice B — code graph read API ────────────────────────────────────
    register_action_with_meta(
        GRAPH,
        |p: Value| async move { crate::graph::api::handle_graph(p).await },
        "The active workspace's code graph, read live from Neo4j. Payload: \
         {workspace_id}. Reply: WorkspaceGraph {nodes, edges, clusters}. \
         Read-only; idempotent. Empty graph for an unknown/empty workspace; \
         bolt_* codes when the graph backend is unreachable.",
        META_MODULE,
    );

    // ── Slice F-data — symbol index read API ─────────────────────────────
    register_action_with_meta(
        SYMBOLS_FIND,
        |p: Value| async move { crate::graph::symbol_index::handle_symbols_find(p).await },
        "Resolve a name to workspace symbols (exact-first, fuzzy-fill). \
         Payload: {workspace_id, query, limit?}. Reply: {query, matches:[{entry, \
         score}]}. Served from the active workspace's in-memory index \
         (microsecond exact / <50ms fuzzy); on-demand build fallback when the \
         index isn't warm. limit defaults to 20.",
        META_MODULE,
    );

    // ── Slice G-data — symbol context read API ───────────────────────────
    register_action_with_meta(
        SYMBOL_CONTEXT,
        |p: Value| async move { crate::graph::neighborhood::handle_symbol_context(p).await },
        "Structural context for one symbol: body + callers + callees + types \
         used + file siblings, read live from Neo4j. Payload: {workspace_id, \
         symbol_id, hops?=1, include_body?=true, include_blame?=true}. Reply: \
         SymbolContext {symbol, callers, callees, types_used, siblings, \
         hops_traversed, took_ms}; symbol.blame carries recent per-commit git \
         blame for the focal's body lines (Slice L — tracked files only, \
         fail-soft absent otherwise). Read-only; idempotent. `hops` walks the \
         call graph (per-hop time budget 200ms+300ms×N); not_found when the \
         symbol isn't in the workspace; bolt_* codes when the backend is \
         unreachable.",
        META_MODULE,
    );

    // ── TBS concept-system — Phase 0 ─────────────────────────────────────
    register_action_with_meta(
        CONCEPTS_LIST,
        |p: Value| async move { crate::concepts::api::handle_list(p).await },
        "Every concept for a workspace. Payload: {workspace_id}. Reply: \
         {workspace_id, concepts, count}. Read-only; served from the \
         authoritative concepts.json (no Neo4j needed).",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_GET,
        |p: Value| async move { crate::concepts::api::handle_get(p).await },
        "One concept by id (members + files + parents). Payload: {workspace_id, \
         id}. Reply: the Concept. not_found for an unknown id.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_BUILD,
        |p: Value| async move { crate::concepts::api::handle_build(p).await },
        "Phase-0 cheap-concept pass: read the workspace code graph, label its \
         directory clusters into stand-in concepts, and replace concepts.json. \
         Idempotent. Payload: {workspace_id}. Reply: {workspace_id, built, \
         projected, source: directory_cluster}. bolt_* codes when the graph \
         backend is unreachable (the build needs the directory clusters).",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_CREATE,
        |p: Value| async move { crate::concepts::api::handle_create(p).await },
        "Hand-author one concept (curation). Payload: {workspace_id, id, label?, \
         description?, members?, member_files?, parent_concepts?}. Reply: the \
         Concept. already_exists on a duplicate id.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_UPDATE,
        |p: Value| async move { crate::concepts::api::handle_update(p).await },
        "Patch a concept's label/description/members/parent_concepts/described_by. \
         Payload: {workspace_id, id, ...patch}. Reply: the updated Concept. \
         not_found for an unknown id.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_DELETE,
        |p: Value| async move { crate::concepts::api::handle_delete(p).await },
        "Remove a concept by id. Payload: {workspace_id, id}. Reply: \
         {ok, removed, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_LIST_UNDER,
        |p: Value| async move { crate::concepts::api::handle_list_under(p).await },
        "Concepts whose parent set contains parent_id (the CHILD_OF DAG). \
         Payload: {workspace_id, parent_id}. Reply: {workspace_id, parent_id, \
         concepts, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_REVERSE_LOOKUP,
        |p: Value| async move { crate::concepts::api::handle_reverse_lookup(p).await },
        "Reverse lookup (thesis §4.2): from a symbol/file to the concepts and \
         vocabulary it belongs to. Payload: {workspace_id, symbol_id?, file?} \
         (one of symbol_id/file required). Reply: {workspace_id, symbol_id, \
         file, concepts, vocabulary}. Pure store query; no Neo4j.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_SEARCH,
        |p: Value| async move { crate::concepts::api::handle_search(p).await },
        "Hybrid concept search (thesis §3.2): nucleo fuzzy + centroid-cosine \
         (semantic half active once concepts carry centroids). Payload: \
         {workspace_id, query, limit?}. Reply: {workspace_id, query, results: \
         [{concept, score, fuzzy, semantic}], count}. Empty query → full set by \
         label. Semantic embed is skipped (no Ollama round-trip) until a \
         concept has a centroid.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_BUILD_SEMANTIC,
        |p: Value| async move { crate::concepts::api::handle_build_semantic(p).await },
        "Force the embedding-clustering concept build (thesis S2.1/S2.2): \
         spherical k-means over the chunk vectors + centroids + overlapping \
         membership. Payload: {workspace_id, k?, overlap_margin?, seed?}. Reply: \
         {workspace_id, built, projected, source: embedding}. built:0 when the \
         index has too few vectors. (`concepts.build` auto-prefers this when an \
         index exists.)",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_PROPOSE,
        |p: Value| async move { crate::concepts::api::handle_propose(p).await },
        "Queue an AI-proposed concept for review (NOT persisted; user-accept-\
         always). Payload: {workspace_id, id, label?, description?, members?, \
         confidence?, rationale?}. Reply: {outcome: queued|already_pending|\
         suppressed}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_LIST_PROPOSALS,
        |p: Value| async move { crate::concepts::api::handle_list_proposals(p).await },
        "Pending concept proposals awaiting review. Payload: {workspace_id}. \
         Reply: {workspace_id, proposals, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_ACCEPT_PROPOSAL,
        |p: Value| async move { crate::concepts::api::handle_accept_proposal(p).await },
        "Land a pending concept proposal in concepts.json. Payload: \
         {workspace_id, id}. Reply: {accepted, concept}. not_found when absent.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_REJECT_PROPOSAL,
        |p: Value| async move { crate::concepts::api::handle_reject_proposal(p).await },
        "Dismiss a pending concept proposal + record a 30-day suppression. \
         Payload: {workspace_id, id}. Reply: {ok, rejected, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_LENS,
        |p: Value| async move { crate::concepts::api::handle_lens(p).await },
        "Concept-as-scoped-lens (thesis §3.1): a concept's members intersected \
         with a scope region (path subtree). Payload: {workspace_id, id, scope?}. \
         Reply: {concept_id, scope, files, count}. Composes with workspace \
         scoping; pure store query.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_FRESHNESS,
        |p: Value| async move { crate::concepts::api::handle_freshness(p).await },
        "Concept drift detection (thesis S4.3): which concepts went stale — a \
         member file changed since the concept was built, or vanished from the \
         index. Payload: {workspace_id, id?}. Reply: {workspace_id, stale_count, \
         freshness:[{id, stale, churned_files, missing_files, built_at, \
         newest_member_mtime}]}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_RETRIEVE,
        |p: Value| async move { crate::concepts::api::handle_retrieve(p).await },
        "Concept-driven retrieval (thesis §3.3): the concept as RAG unit — \
         representative member chunks ranked by cosine-to-centroid + MMR, \
         optionally lens-scoped. Payload: {workspace_id, id, scope?, k?=5}. \
         Reply: {concept_id, scope, snippets:[{path,start_line,end_line,content,\
         score}], count}. The retrieval MECHANISM; query→concept ROUTING is the \
         deferred §3.4 phase.",
        META_MODULE,
    );

    // ── Concept-routing R1.5a — typed relation store ─────────────────────
    register_action_with_meta(
        CONCEPTS_RELATIONS_GRAPH,
        |p: Value| async move { crate::concepts::relations_bridge::handle_graph(p).await },
        "The whole typed relation graph for a workspace (tree view + routing \
         engine warm-load). Payload: {workspace_id}. Reply: {workspace_id, \
         count, relations:[{from, to, kind, note?, created_at}]}. Read-only; \
         fail-soft to empty.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_RELATIONS_LIST,
        |p: Value| async move { crate::concepts::relations_bridge::handle_list(p).await },
        "Typed edges touching one node (both directions), grouped by kind. \
         Payload: {workspace_id, node: {node:concept,id}|{node:vocab,identifier}}. \
         Reply: {node, count, relations, by_kind:{positive, negative, \
         dependency_out, dependency_in}}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_RELATIONS_ADD,
        |p: Value| async move { crate::concepts::relations_bridge::handle_add(p).await },
        "Author one typed relation edge. Payload: {workspace_id, from, to, \
         kind: positive|negative|dependency, note?}. positive/negative are \
         symmetric (orientation canonicalised); dependency is directional. \
         Reply: {relation}. bad_request on a self-edge / unknown node; \
         already_exists (details: the existing edge) on a duplicate (from,to,kind).",
        META_MODULE,
    );
    register_action_with_meta(
        CONCEPTS_RELATIONS_REMOVE,
        |p: Value| async move { crate::concepts::relations_bridge::handle_remove(p).await },
        "Delete one relation edge by (from,to,kind); symmetric kinds match \
         either orientation. Payload: {workspace_id, from, to, kind}. Reply: \
         {removed: bool}.",
        META_MODULE,
    );

    // ── Definitional concept hierarchy H1 — overlay store + verbs ─────────
    register_action_with_meta(
        HIERARCHY_GET_TREE,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_get_tree(p).await },
        "The whole projected+overlaid concept-hierarchy DAG (definitional \
         hierarchy plan H1). Payload: {workspace_id}. Reply: {enabled, count, \
         roots, leaves, nodes:[{id,label,definition,kind,parents,children,\
         is_leaf}], xrefs, dangling_count}. Master-toggle OFF ⇒ \
         {enabled:false, nodes:[]} (inert). Read-only; fail-soft to the bare \
         projection when no overlay exists.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_GET_NODE,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_get_node(p).await },
        "One hierarchy node with its parents, children, the definitional \
         ancestor-chain (nearest-first, each resolved to its definition), and \
         the cross-references touching it. Payload: {workspace_id, id}. \
         not_found on an unknown id; OFF ⇒ {enabled:false, node:null}.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_SET_DEFINITION,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_set_definition(p).await },
        "Author/override a node's definition (and optional label), or mint a \
         brand-new authored node. Payload: {workspace_id, id?, definition?, \
         source?=authored|llm_draft, label?}. With id: empty definition CLEARS \
         the override (reverts to inherited) and prunes the record. Without id: \
         mints a never-reused node:<n> id (a non-empty definition is required). \
         Reply: {id, node}. OFF ⇒ disabled.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_ADD_EDGE,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_add_edge(p).await },
        "Author one containment edge (parent contains child). Payload: \
         {workspace_id, parent, child}. Re-adding a dangling edge clears its \
         flag. bad_request on a self-edge / unknown endpoint; already_exists on \
         a live duplicate. OFF ⇒ disabled.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_REMOVE_EDGE,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_remove_edge(p).await },
        "Delete one authored containment edge. Payload: {workspace_id, parent, \
         child}. Reply: {removed: bool}. Only overlay edges are removable (the \
         projection's own edges live in the concept store). OFF ⇒ disabled.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_MERGE_NODES,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_merge_nodes(p).await },
        "Declare two nodes are one (OQ-2): the alias folds into the primary on \
         apply. Payload: {workspace_id, primary, alias}. bad_request on a \
         self-merge / unknown endpoint; already_exists on a live duplicate; \
         re-adding a dangling merge clears its flag. OFF ⇒ disabled.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_REMOVE_MERGE,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_remove_merge(p).await },
        "Undo a merge by (primary, alias) so the alias re-appears as its own \
         node — authoring stays reversible. Payload: {workspace_id, primary, \
         alias}. Reply: {removed: bool}. OFF ⇒ disabled.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_GET_CONFIG,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_get_config(p).await },
        "The hierarchy master-toggle state. Payload: {}. Reply: {enabled}. \
         Ungated (must be readable while the feature is off).",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_SET_ENABLED,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_set_enabled(p).await },
        "Flip the hierarchy master toggle. Payload: {enabled: bool}. Reply: \
         {enabled}. Ungated; persists to <data_dir>/settings/hierarchy.json \
         (fail-closed OFF). Off ⇒ all hierarchy verbs go inert.",
        META_MODULE,
    );
    register_action_with_meta(
        HIERARCHY_GET_OVERLAY,
        |p: Value| async move { crate::concepts::hierarchy_bridge::handle_get_overlay(p).await },
        "The RAW authored overlay (authored nodes + containment edges + merges) \
         WITH dangling flags, for the authoring UI's re-point/remove affordances \
         — unlike get_tree, which folds + excludes dangling. Payload: \
         {workspace_id}. Reply: {enabled, nodes, edges:[{parent,child,dangling}], \
         merges:[{primary,alias,dangling}]}. OFF ⇒ {enabled:false, edges:[]}.",
        META_MODULE,
    );

    // ── S1 (IDE plan P0.2) — jailed file I/O ─────────────────────────────
    register_action_with_meta(
        FS_READ,
        |p: Value| async move { crate::fs::api::handle_read(p).await },
        "Read one workspace file's text, root-jailed. Payload: {workspace_id, \
         path}. Reply: {content, encoding: utf8|utf8-lossy|binary, binary, \
         truncated, size_bytes, mtime}. `binary` (null byte in first 1KB) → \
         empty content; oversized (> fs_max_read_bytes, default 2MiB) → \
         truncated:true with the head only. path_escape on a jail breach; io \
         on a missing/unreadable file.",
        META_MODULE,
    );
    register_action_with_meta(
        FS_WRITE,
        |p: Value| async move { crate::fs::api::handle_write(p).await },
        "Atomically save text to a workspace file, root-jailed. Payload: \
         {workspace_id, path, content, expected_mtime?}. Reply: {mtime, \
         size_bytes}. expected_mtime gives optimistic concurrency — a newer \
         on-disk mtime returns `conflict` (details: current_mtime) so the \
         editor can prompt. Temp-file+rename atomic write; the existing watcher \
         picks up the change for a debounced re-index (the write does not \
         enqueue indexing itself). path_escape on a jail breach.",
        META_MODULE,
    );
    register_action_with_meta(
        FS_LIST_DIR,
        |p: Value| async move { crate::fs::api::handle_list_dir(p).await },
        "List one directory level under a workspace, root-jailed (lazy tree \
         expansion). Payload: {workspace_id, path?=root}. Reply: {path, \
         entries:[{name, kind: file|dir|symlink, size_bytes?, mtime?, \
         ignored}]}. `ignored` marks entries the indexer's walk skips (.git/ \
         target/ node_modules/ dotfiles/ binary suffixes) — still listed so \
         the tree can show binaries/oversized. Dirs sort first. path_escape on \
         a jail breach.",
        META_MODULE,
    );

    // ── Slice 0c — workspace notes tier ──────────────────────────────────
    register_action_with_meta(
        NOTES_LIST,
        |p: Value| async move { crate::notes::api::handle_list(p).await },
        "Every note for a workspace. Payload: {workspace_id}. Reply: \
         {workspace_id, notes, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_ADD,
        |p: Value| async move { crate::notes::api::handle_add(p).await },
        "Append a workspace note (embeds on write). Payload: {workspace_id, \
         text}. Reply: the new note {id, text, created_at, last_used_at}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_UPDATE,
        |p: Value| async move { crate::notes::api::handle_update(p).await },
        "Edit a note's text (re-embeds). Payload: {workspace_id, id, text}. \
         Reply: the updated note. not_found for an unknown id.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_DELETE,
        |p: Value| async move { crate::notes::api::handle_delete(p).await },
        "Remove a note by id. Payload: {workspace_id, id}. Reply: {ok, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_SEARCH,
        |p: Value| async move { crate::notes::api::handle_search(p).await },
        "Recency+relevance ranked search over a workspace's notes. Payload: \
         {workspace_id, query, limit?}. Fail-soft to empty. Reply: \
         {workspace_id, notes, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        NOTES_PROPOSE,
        |p: Value| async move { crate::notes::api::handle_propose(p).await },
        "Reflection candidate note (NOT persisted; user accepts via \
         notes.add). Payload: {workspace_id, text}. Reply: {candidate} or \
         {candidate: null} for blank text.",
        META_MODULE,
    );

    // ── Slice 0c — workspace-scoped conversations ────────────────────────
    register_action_with_meta(
        CONVERSATIONS_LIST,
        |p: Value| async move { crate::conversations::api::handle_list(p).await },
        "Metadata for one workspace's conversations, newest-first. Payload: \
         {workspace_id}. Reply: {workspace_id, conversations, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_GET,
        |p: Value| async move { crate::conversations::api::handle_get(p).await },
        "Full conversation document. Payload: {workspace_id, id}. \
         bad_request for a missing/invalid id, not_found when absent.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_DELETE,
        |p: Value| async move { crate::conversations::api::handle_delete(p).await },
        "Remove one workspace conversation. Payload: {workspace_id, id}. \
         Reply: {ok, id}.",
        META_MODULE,
    );
    register_action_with_meta(
        CONVERSATIONS_REFRESH_SUMMARY,
        |p: Value| async move { crate::conversations::api::handle_refresh_summary(p).await },
        "Persist an LLM summary + embedding for a workspace conversation \
         (Slice E parity; harness generates, service stores). Payload: \
         {workspace_id, conversation_id, summary, embedding, topic_tags?, \
         summary_msg_count?}. Reply: {ok, id}. not_found when absent.",
        META_MODULE,
    );

    // ── Slice J — conversation export / import (the escape hatch) ────────
    register_action_with_meta(
        CHAT_EXPORT,
        |p: Value| async move { crate::conversations::api::handle_export(p).await },
        "Export one workspace conversation as a portable plaintext envelope \
         (wylde-conversation-export v1). Payload: {workspace_id, \
         conversation_id}. Reply: {export, id}. The caller persists the file.",
        META_MODULE,
    );
    register_action_with_meta(
        CHAT_IMPORT,
        |p: Value| async move { crate::conversations::api::handle_import(p).await },
        "Import a portable conversation envelope into a workspace. Payload: \
         {workspace_id, export, overwrite?}. Reply: {imported, workspace_id}. \
         already_exists on an id collision unless overwrite:true.",
        META_MODULE,
    );

    // ── Slice I — file watcher control ───────────────────────────────────
    register_action_with_meta(
        WATCHER_STATUS,
        |p: Value| async move { api::handle_watcher_status(p).await },
        "File-watcher status for observability. No payload. Reply: \
         {active_workspace, files_watched, last_event_at, paused}.",
        META_MODULE,
    );
    register_action_with_meta(
        WATCHER_PAUSE,
        |p: Value| async move { api::handle_watcher_pause(p).await },
        "Pause the active workspace's file watcher (e.g. before a big \
         checkout). No payload. Reply: {ok, paused: true, active_workspace}.",
        META_MODULE,
    );
    register_action_with_meta(
        WATCHER_RESUME,
        |p: Value| async move { api::handle_watcher_resume(p).await },
        "Resume the file watcher and re-walk the workspace to catch up on \
         edits missed while paused. No payload. Reply: {ok, paused: false, \
         active_workspace}.",
        META_MODULE,
    );

    // ── Slice N-data — workspace anchor store ────────────────────────────
    register_action_with_meta(
        ANCHORS_LIST,
        |p: Value| async move { crate::anchors::api::handle_list(p).await },
        "Every anchor for a workspace. Payload: {workspace_id}. Reply: \
         {workspace_id, anchors, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_CREATE,
        |p: Value| async move { crate::anchors::api::handle_create(p).await },
        "Mint a workspace anchor. Payload: {workspace_id, identifier, kind?, \
         target, description?, parent_anchor?, domain?, related_to?}. Reply: \
         the Anchor. `already_exists` (details carry the existing definition) \
         on a duplicate identifier; `bad_request` on a bad identifier/target.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_UPDATE,
        |p: Value| async move { crate::anchors::api::handle_update(p).await },
        "Patch an anchor's description/target/related_to/parent_anchor/domain. \
         Payload: {workspace_id, identifier, ...patch}. Reply: the updated \
         Anchor. not_found for an unknown identifier.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_DELETE,
        |p: Value| async move { crate::anchors::api::handle_delete(p).await },
        "Remove an anchor by identifier. Payload: {workspace_id, identifier}. \
         Reply: {ok, identifier}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_FIND_BY_TOKEN,
        |p: Value| async move { crate::anchors::api::handle_find_by_token(p).await },
        "Resolve a `{{token}}` (or bare name) to a workspace's anchors — \
         composer recognition. Payload: {workspace_id, token}. Reply: \
         {workspace_id, token, anchors, count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_FIND_BY_TARGET,
        |p: Value| async move { crate::anchors::api::handle_find_by_target(p).await },
        "Inverse lookup (OI-20): every anchor referencing a symbol. Payload: \
         {workspace_id, symbol_id}. Reply: {workspace_id, symbol_id, anchors, \
         count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_LIST_UNDER,
        |p: Value| async move { crate::anchors::api::handle_list_under(p).await },
        "Anchors under a taxonomy parent (OI-19 hierarchy). Payload: \
         {workspace_id, parent_id}. Reply: {workspace_id, parent_id, anchors, \
         count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_PROPOSE,
        |p: Value| async move { crate::anchors::api::handle_propose(p).await },
        "LLM reflection candidate anchor (NOT persisted; user accepts via \
         anchors.create). Applies OI-7 spam control from caller-supplied \
         counters. Payload: {workspace_id, identifier, target, kind?, \
         description?, confidence?, rationale?, proposals_so_far?, \
         last_proposal_at?}. Reply: {candidate} or {candidate: null, reason}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_PROMOTE_VIA_ALIAS,
        |p: Value| async move { crate::anchors::api::handle_promote_via_alias(p).await },
        "Promote an anchor to global because the user acted on one of its \
         aliases — the WHOLE anchor (all aliases) promotes. Validates the alias \
         belongs to the anchor + audit-logs the intent, then returns the \
         promotion payload for the caller to land via the global \
         anchors.promote_via_alias. Payload: {workspace_id, anchor_id, alias}. \
         Reply: {anchor, via_alias, promote: true}.",
        META_MODULE,
    );

    // ── Slice M — symbol ignore list (workspace + conversation tiers) ────
    register_action_with_meta(
        IGNORE_LIST,
        |p: Value| async move { crate::ignore::api::handle_list(p).await },
        "Both service-side ignore tiers for a workspace. Payload: \
         {workspace_id, conversation_id?}. Reply: {workspace_id, workspace: \
         [{token, added_at}], conversation: [...], conversation_id}. The \
         global tier lives in the harness.",
        META_MODULE,
    );
    register_action_with_meta(
        IGNORE_ADD,
        |p: Value| async move { crate::ignore::api::handle_add(p).await },
        "Ignore a token in one tier (default-inactive from now on, Plan \
         §5.8). Payload: {workspace_id, tier: workspace|conversation, token, \
         conversation_id? (required for conversation)}. Reply: {ok, added, \
         workspace_id, token} — re-adding succeeds with added=false \
         (idempotent write).",
        META_MODULE,
    );
    register_action_with_meta(
        IGNORE_REMOVE,
        |p: Value| async move { crate::ignore::api::handle_remove(p).await },
        "Stop ignoring a token in one tier. Payload: {workspace_id, tier, \
         token, conversation_id?}. Reply: {ok, removed, workspace_id, token}.",
        META_MODULE,
    );

    // ── Slice N — LLM anchor-proposal review queue ───────────────────────
    register_action_with_meta(
        ANCHORS_LIST_PROPOSALS,
        |p: Value| async move { crate::anchors::api::handle_list_proposals(p).await },
        "Pending LLM anchor proposals awaiting user review (user-accept-\
         always, OI-18). Payload: {workspace_id}. Reply: {workspace_id, \
         proposals: [{anchor, confidence, rationale, proposed_at}], count}.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_ACCEPT_PROPOSAL,
        |p: Value| async move { crate::anchors::api::handle_accept_proposal(p).await },
        "Land a pending proposal in the anchor store. Payload: {workspace_id, \
         identifier, merge?}. Reply: {accepted: created|merged, anchor}. A \
         colliding identifier returns already_exists with {existing, proposal} \
         details (the OI-18 diff view) and keeps the proposal pending; \
         merge:true applies the proposal onto the existing record instead.",
        META_MODULE,
    );
    register_action_with_meta(
        ANCHORS_REJECT_PROPOSAL,
        |p: Value| async move { crate::anchors::api::handle_reject_proposal(p).await },
        "Dismiss a pending proposal + record the OI-11 suppression (30 days \
         default; WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS). Payload: \
         {workspace_id, identifier}. Reply: {ok, rejected, identifier}.",
        META_MODULE,
    );

    tracing::info!(
        "wylde-workspaces: registered {} action(s)",
        ALL_ACTIONS.len()
    );
}

/// Handle the `ping` verb. Pure — no I/O — so it doubles as the unit under
/// test for the reply shape the integration test asserts over the wire.
pub fn handle_ping() -> Reply {
    Reply::ok(json!({
        "ok": true,
        "service": "wylde-workspaces",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Signal stop. Tears down the Slice I file watcher (drops its OS handle +
/// ends its background loop); idempotent if none is running.
pub fn stop() {
    crate::watcher::stop();
    crate::graph::symbol_index::stop();
}

/// Test-only: unregister every action and reset the install flag so a test
/// can re-`install()` on the shared (process-wide) registry cleanly.
pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{assert_action_table_matches_registry, dispatch_action, list_actions};

    // The action registry is process-wide; serialize the tests that
    // install/reset it so parallel threads don't clobber each other's
    // registration. Same guard pattern as `wylde-ollama::service::tests`.
    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[test]
    fn ping_reply_shape() {
        // Pure — does not touch the registry, so no guard needed.
        let reply = handle_ping();
        assert!(reply.ok);
        assert_eq!(reply.data["ok"], json!(true));
        assert_eq!(reply.data["service"], "wylde-workspaces");
        assert_eq!(reply.data["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn install_registers_all_actions_both_directions() {
        // #130: workspaces has the largest table (~80 verbs) and was previously
        // guarded only for PING and SYMBOL_CONTEXT. Assert ALL_ACTIONS and the
        // live registry AGREE in both directions across every namespace this
        // service owns — a registered verb missing from the table (which drives
        // reset_for_tests, so it would leak across tests) now fails here and
        // names itself.
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        assert_action_table_matches_registry(
            &["ping", "workspaces.", "settings.lexical.", "chat."],
            ALL_ACTIONS,
        );
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_registers_ping_and_dispatches() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        assert!(list_actions().contains(&PING.to_string()));

        let reply = dispatch_action(json!({"action": PING, "payload": null})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["service"], "wylde-workspaces");
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_registers_symbol_context_and_validates_payload() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        // The Slice G-data verb is registered and dispatchable.
        assert!(list_actions().contains(&SYMBOL_CONTEXT.to_string()));

        // A blank symbol_id is rejected before any Bolt connection — proves
        // the verb is wired through real dispatch without needing Neo4j.
        let reply = dispatch_action(json!({
            "action": SYMBOL_CONTEXT,
            "payload": { "workspace_id": "ws", "symbol_id": "  " }
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
        reset_for_tests();
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(json!({"action": "workspaces.bogus", "payload": null})).await;
        assert!(!reply.ok);
        // Shared dispatcher's stable code for an unregistered action.
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_serves_graph_verb_not_no_action() {
        // F5: a freshly-built binary REGISTERS workspaces.graph and serves it
        // through the real production dispatcher (the same path the pipe server
        // routes /__action__ frames through). The live failure was purely the
        // stale (pre-6/8) binary, which lacked the verb and returned
        // `no_action: unknown action`. Here the same call is served by the
        // handler: a blank/missing workspace_id is validated to `bad_request`
        // BEFORE any Bolt connection — proving the verb exists without needing
        // the (currently-down) graph backend.
        let _g = registry_guard().await;
        reset_for_tests();
        install();

        assert!(
            list_actions().contains(&GRAPH.to_string()),
            "a fresh binary must register workspaces.graph"
        );

        let reply = dispatch_action(json!({"action": GRAPH, "payload": {}})).await;
        assert!(!reply.ok, "blank id must be rejected by the handler");
        let code = reply.error.unwrap().code;
        assert_eq!(code, "bad_request", "served by the handler, got {code:?}");
        assert_ne!(
            code, "no_action",
            "must NOT be the unknown-action fallthrough"
        );

        reset_for_tests();
    }

    #[tokio::test]
    async fn install_serves_relations_verbs_not_no_action() {
        // R1.5a: the four relations verbs are REGISTERED and SERVED through the
        // real production dispatcher (the path the pipe server routes
        // /__action__ frames through) — the same wire-level guarantee the graph
        // verb gets. Each is validated by its handler (bad_request on a blank
        // payload), proving it's wired in without needing a live workspace.
        let _g = registry_guard().await;
        reset_for_tests();
        install();

        for verb in [
            CONCEPTS_RELATIONS_GRAPH,
            CONCEPTS_RELATIONS_LIST,
            CONCEPTS_RELATIONS_ADD,
            CONCEPTS_RELATIONS_REMOVE,
        ] {
            assert!(
                list_actions().contains(&verb.to_string()),
                "a fresh binary must register {verb}"
            );
            let reply = dispatch_action(json!({"action": verb, "payload": {}})).await;
            assert!(!reply.ok, "{verb}: blank payload must be rejected");
            let code = reply.error.unwrap().code;
            assert_eq!(
                code, "bad_request",
                "{verb} served by handler, got {code:?}"
            );
            assert_ne!(
                code, "no_action",
                "{verb} must NOT be the unknown-action fallthrough"
            );
        }

        reset_for_tests();
    }
}
