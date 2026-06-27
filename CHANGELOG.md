# Changelog

All notable changes to Wylde are recorded here. Versions follow
[SemVer](https://semver.org/); pre-1.0 alphas may break between builds.

## [Unreleased]

### Added

- **Thought Bubble System — structural awareness for chat.** A floating
  Thought-Bubble composer layer over the chat input, with a unified
  `Ctrl+Z` undo timeline spanning both typed text and bubbles. The composer is
  symbol-aware (a `Ctrl+P` palette resolves code symbols) and, before each turn,
  an AI context-gather hook performs structural retrieval — it detects symbols
  and anchors in the prompt, pulls a bounded k-hop code neighbourhood
  (callers/callees/types), the user profile, short-term memory, and workspace
  notes, then evicts to a token budget and injects them as named prompt slots.
- **Anchors & Vocabulary.** A durable anchor/vocabulary layer (workspace + global
  stores, shared tokenizer, human-friendly aliases) with a Vocabulary tab, an
  LLM-proposal review queue, a composer "Anchor-this" action, recommended-cleanup
  / stale-mark / archive semantics, and a graph vocabulary overlay.
- **User profile.** A `user_profile` module (name / style / freeform rules) with
  an editable Settings section and an Accept / Edit / Reject queue for
  model-proposed profile changes; user edits always win, proposals are
  spam-gated and time-suppressed. Encrypted at rest (DPAPI).
- **Workspace knowledge-graph verbs.** New read/query surface over the workspace
  code graph: `workspaces.graph` (cached projection), `symbols.find` (in-memory
  fuzzy symbol index), `workspaces.symbol_context` (k-hop caller/callee/type
  neighbourhood with git-blame), `anchors.*`, and scoped chat-history search.
  A file watcher keeps the graph fresh via per-file delta-upsert (and graph-clean
  on delete).
- **Workspaces graph panel.** A native gpui graph visualization for the workspace
  code graph: force-directed layout (Barnes-Hut, off-thread physics worker),
  plus deterministic hierarchical and stable-grid layouts with animated 500 ms
  swaps; space-map navigation (zoom, breadcrumb, exit edges), auto-clustering
  with expand-in-place, a clusters-first "galaxy" tier with aggregate edges,
  viewport culling / LOD, fit-to-view, and a settings menu + per-workspace layout
  profile library. Every colour/size is read from the locked Visual Style v1
  theme.
- **Workspaces IDE.** An in-app IDE for the active workspace: jailed
  `workspaces.fs.*` verbs (read / write / list_dir), a Files + Editor tab shell,
  a from-scratch gpui code-editor element with syntax highlighting, a lazy
  file-tree, and cross-panel deep-links (vocab word → graph node, GraphView
  `focus_node`). An optional `wylde-lsp` service wraps rust-analyzer to provide
  in-editor diagnostics, completions, and hover.
- **Concept system + concept-routing (R0–R4).** A concept layer over the index
  (schema, directory-labeled cheap concepts, then semantic concepts via
  embedding clustering + centroids + curation) feeding concept-driven retrieval,
  a freshness signal, and an additive four-colour highlight. A browse surface
  (Concepts sub-tab, hybrid search, vocab hierarchy). Concept-**routing** ships
  as an isolated, **default-OFF, byte-identical-when-disabled** crate: toggle +
  route-and-log → typed-relation store + spreading-activation engine → relations
  authoring GUI → a curate-before-inject menu with Augment injection →
  scoped-lens narrowing + typed dependency-tree viz → an eval harness with
  calibrated thresholds. Augment is the default mode; Replace is opt-in.
- **Definitional concept hierarchy (H0–H6).** A navigable, drill-down DAG that
  unifies concepts + vocabulary anchors into one `{id, label, definition,
  parents, children}` node model — every node carries a definition; you drill
  until leaves are definition-only. Shipped as an isolated, **default-OFF,
  byte-identical-when-disabled** crate (`wylde-concept-hierarchy`) that
  *projects* the view read-only from the existing concept / anchor / relation
  stores (multi-parent preserved, diamonds/cycles guarded, the definitional
  ancestor-chain accessor), plus a thin additive `hierarchy.json` overlay for
  net-new authored data: authored/overriding definitions by a priority ladder, a
  never-reused `node:<n>` id allocator, authored containment edges, and node
  merges — all with the `Relation.dangling` retain-but-exclude rule. A deletable
  `workspaces.hierarchy.*` verb seam (`get_tree`, `get_node`, `set_definition`,
  `add_edge`, `remove_edge`, `merge_nodes`) maps the Core `Concept` into the
  crate's Core-free `ConceptView` so the crate never touches Core. A read-only
  **Hierarchy** sub-tab (in the isolated `hierarchy/` GUI folder, fourth tab of
  the Vocabulary tab) renders the DAG as a cycle-safe, indented drill-down —
  definitions shown at every level with a priority-ladder source badge, a "needs
  definition" badge on `Missing` nodes, "also under: …" for multi-parent nodes,
  a selected-node ancestor-chain breadcrumb, and a Graph deep-link via the focus
  bus. The sub-tab also **authors**: edit/override or clear a node's definition,
  mint brand-new authored nodes, add/remove containment edges, and merge/unmerge
  nodes (a target picker over the loaded universe) — with an "authored edges &
  merges" panel that surfaces dangling records for re-point/remove. Toggle OFF ⇒
  the verbs are inert, the sub-tab renders an inert disabled state, and the
  overlay is never written; deleting the crate + bridge + overlay + sub-tab
  folder reverts to today. **H5 retrieval injection** rides the existing
  `### Concepts` slot: for each curated concept it adds a high-signal
  definitional ancestor-chain line (`Label — definition — under Parent — under
  Root`), blurb-first so token-budget eviction sheds snippets before
  definitions, Augment-only, missing-definition nodes skipped — and gated
  identity-when-off (the block is never added unless the toggle is on, so today's
  prompt is byte-identical). **H6 containment-spread** wires the hierarchy's
  parent/child containment edges into the spreading-activation router as a
  *separate*, gated propagation channel (not a `RelationKind` — the
  `concept_relations.json` wire shape stays frozen): activation flows along
  containment with an asymmetric decay (child→parent strong, parent→child weak —
  both tunable knobs, conservative defaults), reusing the same Dijkstra
  relaxation + cycle guard as the dependency step, and slotted before the IS-NOT
  inhibition so a strong exclusion still has the last word. The channel is
  sourced at the workspaces wiring layer from the applied hierarchy graph and
  mapped into the router's node space, so the routing crate stays decoupled from
  hierarchy storage. **Doubly identity-safe:** the master toggle OFF ⇒ no
  containment adjacency is passed (built without touching the hierarchy stores),
  and even ON an empty adjacency is the spread step's identity — so routing is
  byte-identical to today unless containment edges actually exist and the toggle
  is on.
- **Tree-sitter expansion.** Code outline + highlight verbs (with a graph-panel
  outline card) and added JSON, TOML, YAML, and Bash grammars.
- **Conversation export / import.** An escape-hatch to move conversations in and
  out.
- **Out-of-tree runtime foundation.** Core's tracked tree stays "just Core"; three
  out-of-tree buckets (`Services/`, `Extensions/`, `Core/Plugins/`) ship empty
  and are populated out-of-band, each keeping its own `.git`. The lifecycle
  registry descends into `Services/*`, dynamically supervises siblings, resolves
  per-service data dirs (`service_paths.json` + `WYLDE_<SVC>_DATA_DIR`), and
  cleanly no-ops when a bucket is absent. Adds a `cargo xtask build-all`, a
  compiled-in plugin mechanism (`wylde-plugin-api` SDK + reference plugin), and
  N8N as a first-class Rust service (`wylde-n8n`).
- **GUI test harness.** A gpui windowed-test harness with dock-scoping tests
  across the Workspaces, Chat, Memory, Editor, Files, and Graph surfaces.
- **Lexical (BM25) retrieval + RRF fusion (default OFF).** A per-workspace
  pure-Rust [tantivy](https://github.com/quickwit-oss/tantivy) inverted index
  over the *same* chunk corpus the dense index already holds, fused with the
  existing cosine retrieval via Reciprocal Rank Fusion so an exact-token recall
  signal (rare identifiers, error codes, literal names the embedder blurs) sits
  alongside semantic relevance. Behind a `settings.lexical.*` master toggle that
  is **OFF by default** — OFF is byte-for-byte today's dense-only behaviour. The
  lexical index is built from the post-exclusion chunk set (never a fresh walk,
  so it can never drift from `chunks.jsonl`), holds term postings + chunk ids
  only (no second copy of chunk bodies), and stays in step via the existing
  content-hash manifest (full rebuild + cheap embed-free backfill + incremental
  watcher delta). Under fusion a strong BM25 hit at low cosine bypasses the
  absolute cosine floor (the recall win) while a query off-topic to both signals
  still injects nothing; the anchor-bias is reworked from a substring boost into
  an IDF-weighted, exact-token BM25 sub-query. A dense/lexical/fused eval harness
  (with a lexical gold class) proves the recall gain and the semantic
  no-regression guardrail.

### Changed

- **Full-Rust cutover.** Every remaining Python runtime component was ported
  to Rust and its source deleted (~350 files): the Lifecycle daemon +
  rollback path (`Core/Lifecycle/`), the Python harness runtime
  (`Core/harness/` — pipe verbs, memory layers, tooling, model registry,
  backend), the shared IPC helpers (`Core/shared/`), and the Memgraph
  Python wrapper (the lifecycle daemon now supervises the bundled Neo4j JVM
  directly). New in Rust with this wave: `memory.reflect` for all three
  scopes (conversation reflection, workspace curation, long-term
  consolidation) and the background memory scheduler (same
  `scheduler_state.json` + `WYLDE_SCHED_*` envs), now a tokio task inside
  `wylde-harness` gated on `WYLDE_HARNESS_SCHEDULER`.
- **Rust-only boot.** `launch_wylde.ps1` lost its Python daemon fallback and
  PYTHONPATH overlay; the per-service `WYLDE_<SERVICE>_IMPL=python`
  strangler flags now only log a warning. The kept Python — the
  `wylde_check` lint tool (`Core/harness/dev/`) and the stdlib N8N tool
  stubs — is dev-only; `pyproject.toml` carries no runtime dependencies and
  the stale `uv.lock` was removed.
- **Images extracted to a service.** The Images suite was lifted out of Core into
  a standalone `wylde-images` service (subtractive removal from Core, with a
  removability acceptance check).
- **Security boundary hardened (P1–P4).** The gateway gained an egress SSRF guard
  (deny-list + DNS-rebinding pin + host allowlist); the extension bridge gained a
  capability-checked inference gate (`inference.embed` / `inference.chat`
  forwarders) and a least-privilege, allowlist-scrubbed spawn environment; the
  webcrawler is now gateway-only for egress, and spawned-process cwd is
  placeholdered.
- **Prompt engineering (B-series).** Per-model `num_ctx` overrides now drive the
  slot budget; windowed conversation history is sent every turn; long-term memory
  and the auto-summary are injected into dedicated tiers; hardcoded prompts were
  migrated into a catalog (with a golden-snapshot harness and a lint rule banning
  literal prompts); a post-turn extraction pass assigns importance; the base
  instruction is capability-conditioned and the message layout is cache-aware;
  vectors use int8 scalar quantization.
- **RAG relevance levers (2.1–2.5).** MMR diversity rerank, dynamic
  where-warranted top-k, conversation-aware query construction, anchor-biased
  retrieval, and an active-file / current-focus boost.
- **Chat scoping.** The Workspaces dock no longer shares the global ChatPanel
  singleton; global Chat is strictly workspace-free, while in-workspace chat gets
  a per-workspace conversation list + switcher, create-and-bind, a last-open
  pointer, and a delete-time sweep of bound conversations.
- **Index hygiene (P1–P4).** Walk-time exclusion, filter-only purge, a
  content-hash manifest, and concept stability across re-indexes.
- **Memory subsystem (M-series).** Tier-7 fit guarantee, pressure-triggered
  consolidation, reflection dedup + recency-touch damping, slot-liveness net,
  server-side query embedding for graph queries, and retirement of the old
  harness RAG subsystem.
- **Accessibility / theme.** Text tokens lifted toward white (ladder preserved),
  non-colour cues for placeholder / disabled / ignored states, and real
  Lucide + Seti file-tree icons replacing the CC0 placeholders.
- **Dev environment.** Fast dev rebuild loop, theme hot-reload, desktop
  shortcuts, and a dev-only hot-reload path (`dev.restart_service` verb +
  backend watcher).

### Security

- **Dependency advisory sweep (RustSec / GitHub Dependabot).** Bumped two
  transitive crates to their patched releases across the affected lockfiles:
  `quinn-proto` 0.11.14 → 0.11.15 (RUSTSEC-2026-0185, HIGH — remote memory
  exhaustion from unbounded out-of-order QUIC stream reassembly; pulled via
  `reqwest`/`quinn` in the `rust/`, `Core/GUI/`, and `Services/wylde-images/`
  workspaces) and `memmap2` 0.9.10 → 0.9.11 (RUSTSEC-2026-0186, unsound —
  unchecked pointer offset; `Core/GUI/`). Lockfile-only patch bumps; no manifest
  or API changes. Remaining advisories are RustSec *unmaintained* notices with no
  patched release (`async-std`, the GTK3 `gtk`/`gdk`/`atk` binding family,
  `glib` unsoundness, `paste`, `instant`, `backoff`, `bincode`, `fxhash`,
  `proc-macro-error`/`proc-macro-error2`, `rustls-pemfile`); these are transitive
  and deferred — clearing them needs upstream/major migrations, not a bump.

### Fixed

- **Short-term memory store now honours encryption-at-rest (OI-14).** It
  used plain file IO on the same conversation documents the conversations
  store reads/writes encrypted; a lazy-migration read could flip a document
  to ciphertext mid-flow, after which the short-term store's plain reads
  saw an unreadable file and silently minted a stub over live data (losing
  the workspace binding and the working-memory list). Both stores now route
  through the same `wylde_shared::encryption` read/write path.
- **Re-index no longer exhausts the OS ephemeral-port pool.** Bolt
  connections are pooled and embed requests rate-capped, and graph
  upsert/relate calls are batched with their own timeout, so whole-repo
  indexing stops crashing the runner.
- **Dev stage deploy-gap.** `wylde-dev.ps1` re-seeds a stale `target-dev/stage`
  from the freshest build (fail-soft when the binary is locked), so newly-added
  verbs stop `no_action`-ing in dev.
- **Slow pipe verbs.** A `call_with_deadline` path gives long-running verbs
  (re-index, graph) a generous deadline instead of timing out.
- **GUI responsiveness.** Surfaced previously-swallowed failures across the
  memory / devices / images / shell / chat surfaces, made graph degrade-retry
  clickable, and wired the vocab undo chord. Shaped-text `TextInput` with real
  glyph metrics (in-input wavy underline).
- **Lifecycle / ollama robustness.** Memgraph/Neo4j spawn anchored to an absolute
  root; a staleness guard flags running services on a rebuilt binary; implicit
  `:latest` tags resolve in ollama `model_matches`; the Start-Ollama button now
  starts the upstream daemon; service-down is distinguished from out-of-date in
  `no_action`.

## [0.1.0-alpha.1] — 2026-06-04

First tagged alpha. Published as a GitHub **pre-release** (beta channel).

### Added

- **gpui-native desktop app.** The full UI was rebuilt on
  [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui),
  retiring the earlier Tauri + Svelte alpha. All panels (Chat, Models, Memory,
  Dashboard, Devices, Workspaces, Tools, Settings, Images, RemoteAccess) talk
  to the in-process Rust harness over named pipes — no web stack, no embedded
  browser.
- **On-device voice, in-process.** STT (Whisper) and TTS (Kokoro) run directly
  in the orchestrator (ONNX); the Python voice service was deleted. Settings
  gains a Voice section (input-device selection, mic test) and a live
  push-to-talk hotkey.
- **In-app self-updater.** Opt-in updates from this repo's GitHub Releases,
  verified against one embedded minisign/Ed25519 public key and fail-closed (an
  unsigned or mis-signed binary is never installed). Stable / Beta channels, a
  manual "Check now", and an optional background check on a chosen cadence. No
  telemetry; the only outbound call is an unauthenticated GitHub REST GET.
- **Per-user installer.** A no-UAC NSIS installer (`WyldeSetup`) that installs
  to `%LOCALAPPDATA%\Programs\Wylde`, with daemon-first Start-menu / desktop
  shortcuts and optional sign-in autostart.
- **Conversation switching.** Per-conversation working memory with a switcher
  UI and a cross-panel nav-bus, so the Memory panel mirrors the active chat's
  buffer; `conversations.*` and `memory.short_term.*` ported to Rust.

### Release assets

- `wylde-gui-x86_64-pc-windows-msvc.exe` (+ `.minisig`) — bare signed GUI
  binary consumed by the self-updater.
- `WyldeSetup-0.1.0-alpha.1.exe` (+ `.minisig`) — per-user installer.

Both signed with the production minisign key (ID `DA7E13F4E9F2ACB6`).

[0.1.0-alpha.1]: https://github.com/PeopleWonder/wylde/releases/tag/v0.1.0-alpha.1
