# Code-Aware Workspaces — Visual Graph + Symbol-Aware AI Context

**Status:** scope · **Date:** 2026-06-07 · **Author:** Aaron + agent

---

## 0. Process change (preamble)

From this slice forward, every feature lives on its own branch (`feat/<name>`, `fix/<name>`, or `chore/<name>`), opens a PR as the durable artifact, and merges `--no-ff` to `main` only when end-to-end working. The Python→Rust strangler-fig era of many-commits-direct-to-main is over. The Wylde core is complete; everything from here is services/plugins/polish, where a broken slice should be isolated to its own branch — not breaking `main` for the rest of the work.

This plan itself lives on `feat/code-aware-workspaces` as a scope doc, in the same pattern PR #12 used for the workspaces redesign.

---

## 1. Vision: what AI-assisted dev in Wylde becomes

**Today.** You ask Wylde: *"fix the bug in `WorkspaceRegistry::set_active` where the MRU eviction sometimes leaves a stale entry."* The chat turn driver gathers context via vector RAG — it pulls chunks similar to your prompt. With luck, the chunk containing `set_active` is in the top-k. With less luck, the LLM gets a couple of nearby-but-wrong functions and hallucinates a fix that doesn't compile.

The LLM is **guessing** what code is relevant. It's working from text similarity, not code structure.

**After this work.** Same question. The chat turn driver:

1. Recognizes `WorkspaceRegistry::set_active` as a symbol reference (via the symbol index)
2. Pulls the function's body
3. Walks the call graph one hop out — every function `set_active` calls (eviction, persistence, MRU shift)
4. Walks the call graph one hop in — every function that calls `set_active` (so the LLM understands the contract callers depend on)
5. Pulls the same module's sibling functions (lexical context)
6. Pulls the type definitions for any structs `set_active` touches
7. **Optionally** pulls recent edits to any of the above (Memgraph + git blame integration)

The LLM is now **seeing the actual code graph**, not vector hopes. Refactors that cross files, API renames, bug fixes that depend on understanding callers — these become reliable instead of best-effort.

**The graph panel** in the Workspaces GUI shows you what the LLM is seeing. You can click nodes to refine. The graph becomes a conversational selection device.

**This is the Wylde edge over Cursor.** Cursor has @-symbol context but no graph visualization, no per-workspace persistent symbol store, no graph traversal as a first-class retrieval primitive. Wylde already has Memgraph and tree-sitter wired up — we're closer to Cursor + graph than Cursor + nothing.

---

## 2. Architecture overview

Three layers, top to bottom:

```
┌─────────────────────────────────────────────────────────────┐
│  UX LAYER (new)                                             │
│  ┌──────────────────┐  ┌──────────────────┐  ┌───────────┐  │
│  │  Workspaces      │  │  Cmd+P symbol    │  │  Chat AI  │  │
│  │  Graph panel     │  │  palette         │  │  context  │  │
│  │  (force-directed)│  │                  │  │  gather   │  │
│  └────────┬─────────┘  └────────┬─────────┘  └─────┬─────┘  │
└───────────┼─────────────────────┼──────────────────┼────────┘
            │                     │                  │
            ▼                     ▼                  ▼
┌─────────────────────────────────────────────────────────────┐
│  QUERY LAYER (new harness verbs)                            │
│  • workspaces.graph(workspace_id) → {nodes, edges}          │
│  • workspaces.symbols.find(query) → [matches]               │
│  • workspaces.symbol_context(name) → {body, callers,        │
│    callees, types, siblings}                                │
│  • treesitter.outline(path) → tree                          │
│  • treesitter.highlight(path) → spans                       │
│  • treesitter.enclosing(path, line, col) → range            │
└───────────┬─────────────────────────────────────────────────┘
            │
            ▼
┌─────────────────────────────────────────────────────────────┐
│  DATA LAYER (mostly already shipped)                        │
│  ┌──────────────────┐  ┌──────────────────────────────────┐ │
│  │  wylde-          │  │  Memgraph                        │ │
│  │  treesitter      │─▶│  • Function/Class/Module nodes   │ │
│  │  (sidecar svc)   │  │  • CALLS/IMPORTS/INHERITS edges  │ │
│  │  • 6 grammars    │  │  • Per-workspace tagged           │ │
│  │  • chunk         │  │  • Embedded entity payloads      │ │
│  │  • extract       │  └──────────────────────────────────┘ │
│  └──────────────────┘                                       │
│         ▲                                                   │
│         │                                                   │
│  ┌──────┴──────────┐                                        │
│  │  N8N rag-ingest │  triggered by file watcher (Slice G)   │
│  └─────────────────┘                                        │
└─────────────────────────────────────────────────────────────┘
```

**Key insight:** the data layer is ~80% done. Tree-sitter sidecar is live with 6 grammars and 4 verbs; N8N's RAG ingest already pipes entities into Memgraph. The bottleneck is that nothing *consumes* the graph data yet — neither the GUI nor the chat turn driver. That's what this plan addresses.

---

## 3. Slices

Each slice = one branch, one PR. Suggested order optimizes for fastest visible payoff and easiest validation.

### Slice A — Payoff verification + data-layer gap-fix (prerequisite)

**Branch:** `verify/tree-sitter-memgraph-payoff` (already spawned)

Verify the tree-sitter → Memgraph pipeline actually populates `CALLS`/`IMPORTS`/`INHERITS` edges. If any stage is broken, fix before going further. **No new features land on top of unverified data.**

**Deliverable:** verdict matrix per pipeline stage + sample edges + any data-layer fixes needed.

### Slice B — `workspaces.graph` verb

**Branch:** `feat/workspaces-graph-verb`

New harness verb that returns the active workspace's full code graph as JSON, queried from Memgraph:

```rust
// rust/crates/wylde-harness/src/workspaces/graph.rs
pub async fn graph(workspace_id: &str) -> Result<WorkspaceGraph> {
    // Query Memgraph for all nodes + edges tagged with this workspace_id
    // Return as { nodes: [{id, kind, name, file, line}], edges: [{src, dst, rel_type}] }
}
```

**Tests:** unit (mock Memgraph response → expected JSON shape), integration (ingest a small fixture corpus, query the verb, assert graph structure).

### Slice C — gpui Workspaces "Graph" tab

**Branch:** `feat/workspaces-graph-panel`

New tab in the existing Workspaces side panel. Renders the graph from Slice B's verb using a force-directed layout. Each node = function/class/file; each edge = CALLS/IMPORTS/INHERITS with color-coded edge type. Pan, zoom, click-to-open-file.

**Library:** `fdg` Rust crate for force-directed layout (small, ~2k LOC, pure Rust, no native deps).

**Per-workspace:** subscribes to the workspace-changed event; re-fetches graph on switch.

### Slice D — Symbol search palette

**Branch:** `feat/workspaces-symbol-palette`

New harness verb `workspaces.symbols.find(query)` does a fuzzy match over all symbol names in the active workspace's Memgraph subgraph. Returns up to N hits with file/line.

New gpui modal (Cmd+P or Ctrl+P): type → live-filter symbols → Enter to open at the symbol's location.

### Slice E — AI context gather hook (THE BIG ONE)

**Branch:** `feat/chat-symbol-context`

Modify the chat turn driver's context gather to recognize symbol references in the user's prompt and pull their graph neighborhoods.

New harness verb `workspaces.symbol_context(name, hops=1)` returns:
```json
{
  "symbol": {"name": "...", "file": "...", "line": 42, "body": "..."},
  "callers": [{"name": "...", "file": "...", "line": ..., "snippet": "..."}],
  "callees": [{"name": "...", "file": "...", "line": ..., "snippet": "..."}],
  "types_used": [...],
  "siblings": [...]
}
```

Turn driver hook (`rust/crates/wylde-harness/src/turn/context_gather.rs`):
1. Pre-LLM hook: tokenize user prompt, extract `\w+::\w+` / `\.\w+\(` / `\w+\(` patterns
2. For each match, query `workspaces.symbols.find` — if exact match, queue it
3. For each queued symbol, query `workspaces.symbol_context`
4. Inject the resulting context into the system prompt slot below the workspace persona, above the RAG chunks

**Tests:** integration test that the symbol context actually lands in the rendered prompt.

### Slice F — Tree-sitter outline + highlight verbs

**Branch:** `feat/treesitter-outline-highlight`

Build the two unfinished verbs from the original tree-sitter plan:
- `treesitter.outline(path)` → tree of `{kind, name, line, children}`
- `treesitter.highlight(path)` → spans of `{start_byte, end_byte, scope}`

Wire into the GUI: outline becomes a per-file sidebar tree; highlight powers syntax coloring in any text view (workspace file browser, future IDE panels).

### Slice G — Real-time delta re-indexing

**Branch:** `feat/workspace-file-watcher`

When a file changes in an active workspace folder, re-extract just that file via `treesitter.extract_entities` and delta-upsert into Memgraph (instead of waiting for the next full N8N ingest cycle). Workspaces becomes always-fresh.

**Implementation:** `notify` Rust crate watches the workspace folder; debounces edits (200ms); per-file Memgraph `upsert` + `relate` calls.

### Slice H — More grammars (as-needed)

**Branch:** `feat/treesitter-grammars-<lang>`

One branch per added grammar. Priority depends on what you actually work in. Suggested order: Go, JSON, YAML, HTML, CSS, C/C++, Java. Each is a small mechanical addition (`cargo add tree-sitter-<lang>` + register in the sidecar's language table).

### Slice I (optional) — Git-blame integration

**Branch:** `feat/symbol-context-git-blame`

Extend Slice E's `symbol_context` to also pull recent git blame for the symbol's lines — so the LLM sees who last touched what and when. Useful for "why did X change recently" questions.

---

## 4. WHY each slice makes AI-assisted dev easier

### Slice B (`workspaces.graph` verb) — *foundation; no direct user impact*

Without this, nothing else in the plan can ship. The verb is the gateway from the data layer to everything in the UX layer.

### Slice C (graph panel) — *the visual model of your codebase*

**Before:** you mentally hold the structure of your project. For large workspaces, this is impossible — nobody fits a 500-file project in their head. You ask the LLM questions partly to discover structure.

**After:** the graph is visible. You see at a glance which modules are central, which are leaf utilities, which have heavy fan-out. You can navigate by clicking. This is the kind of visualization that JetBrains IDEs charge thousands of dollars for; you'd have it baked into your free local assistant.

**AI dev specifically:** when you ask the LLM to refactor something, the graph view makes the **scope of the refactor immediately visible**. You see what gets touched. This is the difference between "approve the diff blindly" and "see the blast radius first."

### Slice D (symbol palette) — *Cmd+P for your whole project*

**Before:** to ask the LLM about a symbol, you either type its full name and hope the LLM finds it, or copy-paste the file path. Friction.

**After:** Cmd+P → start typing → Enter → symbol is selected. The chat composer can auto-insert `@<symbol>` references. The LLM gets exact references.

### Slice E (AI context gather hook) — *the biggest single jump in LLM accuracy*

**Before:** vector RAG retrieval. LLM gets chunks similar to your question. Often wrong-ish, sometimes wrong-completely.

**After:** graph-aware retrieval. LLM gets the actual function bodies, actual callers, actual callees. The same model produces dramatically better answers because the input is dramatically better.

**Concrete example:**
- Prompt: *"why does `apply_overrides` not preserve the migrated profile?"*
- Without graph: LLM gets the file containing `apply_overrides` + 3 similar-looking chunks → guesses at the bug
- With graph: LLM gets `apply_overrides` + the 2 callers (which set up the migration state) + the 4 callees (which write/load the file) + the `OverridesIndex` struct definition + the migration-related sibling functions → can actually trace the bug

This is the slice that takes Wylde from "AI that knows your codebase superficially" to "AI that reasons about your code structurally." It's the structural-retrieval slice — the one that delivers the real payoff of the whole plan.

### Slice F (outline + highlight) — *baseline IDE UX*

**Before:** Wylde GUI has no syntax-aware text views. Plain monospace.

**After:** syntax highlighting in workspace file browsers, future IDE panels, even chat code blocks (consume the highlight verb). Outline view in sidebar for whatever file you're viewing.

**AI dev:** code blocks from the LLM render colored, structured. Easier to scan diffs. Easier to spot what changed.

### Slice G (file watcher) — *always-fresh graph*

**Before:** the graph reflects whatever N8N's last ingest run captured. You make an edit, the graph stales until next cron run.

**After:** save a file → the graph updates within seconds. AI context for "what I'm currently working on" is always current.

**AI dev:** matters most for "I just wrote `foo`, now help me wire it in" workflows — the LLM sees `foo` exists immediately. No "the model doesn't know about your new function" friction.

### Slice H (more grammars) — *coverage*

**Before:** if your workspace is full of (say) Go code, tree-sitter ignores it → no entities → no graph nodes for it.

**After:** the workspace gets full coverage regardless of language mix.

### Slice I (git blame) — *temporal context*

**Before:** the graph is a snapshot of "what is." Refactors over time, evolution of a function, who introduced a bug — invisible.

**After:** `symbol_context` includes recent edit history. LLM can answer "why is this function written this way" with reference to the actual commit history.

---

## 5. What it looks like from the user's chair

### Walkthrough A: refactoring across files

Today:
1. You: *"Rename `set_active` to `set_current_workspace` everywhere."*
2. LLM: "Sure, here's a diff for `registry.rs`." (Misses the 4 call sites in other files.)
3. You: "What about the callers?"
4. LLM: "Let me search... here are 2 of them." (Still missed 2.)
5. You: manual grep, manual fixes.

After:
1. You: *"Rename `set_active` to `set_current_workspace` everywhere."*
2. LLM: uses graph CALLS edges → finds all 6 call sites → presents one unified diff covering registry.rs + 4 caller files + the workspaces module re-export. Done.

### Walkthrough B: understanding unfamiliar code

Today:
1. You open the Workspaces panel. See a list of workspaces. That's it.
2. You ask: *"Walk me through how a chat turn is processed."*
3. LLM gives a high-level answer based on whatever similar chunks RAG pulled.

After:
1. You open the Workspaces panel → click "Graph" tab. See the visual structure of the active workspace. Notice `chat::turn::run` is a central node with many edges.
2. You click that node → it's highlighted in the graph; the right pane shows the function body.
3. You ask: *"Walk me through what happens here."*
4. LLM uses the highlighted symbol's full context (body + 12 callees) → gives a grounded answer that names actual functions.

### Walkthrough C: discovering hidden coupling

Today: you don't.

After: graph panel reveals that 3 modules unexpectedly import from a "utility" module that's supposed to be leaf-only. You notice the smell, ask the LLM to suggest a refactor, get a concrete plan.

---

## 6. Open questions for Aaron

- **OQ1 — Graph layout.** Force-directed (organic, good for exploration) vs hierarchical-by-module (deterministic, good for navigation)? Default force-directed with a toggle for hierarchical?
- **OQ2 — Subgraph filtering granularity.** File-level (collapse all symbols in a file into one node), symbol-level (every function = node), or both with a toggle?
- **OQ3 — AI context gather default.** Slice E's symbol-context injection — opt-in (user adds `@symbol` references explicitly) or default (turn driver auto-detects)? Default-on is more magical; opt-in is more predictable.
- **OQ4 — Cmd+P binding.** Use Cmd+P (Mac convention)? Ctrl+P (cross-platform default for IDEs)? Configurable?
- **OQ5 — Graph node click action.** Open file in the chat composer's @-reference, or open in the workspace file browser, or both?
- **OQ6 — When the graph is huge (1000+ nodes).** Initial view: render all (slow), render top-N by centrality, or empty-until-filter? Suggest "centrality top 50, filter to expand."
- **OQ7 — Slice ordering.** The list above optimizes for fastest visible payoff (A→B→C→D→E). Alternative: prioritize Slice E (the biggest LLM-accuracy jump) before the visual graph for maximum AI-dev benefit faster, even though it's less demo-able. Your call.

---

## 7. Scope guardrails

What this plan does NOT include:
- Inline AI suggestions while typing (Cursor's Tab completion). That's a different surface — would need a dedicated text editor in gpui, not in scope.
- Multi-file edits applied automatically by the LLM. Wylde's pattern is "LLM proposes diff, user reviews, user applies." We're not changing that.
- Hosted/cloud sync of graphs across machines. Wylde is local-first; this stays local-first.

---

## 8. Estimated effort per slice (rough)

| Slice | Effort | Risk |
|---|---|---|
| A — payoff verification | ~half-day | Low (read-only investigation) |
| B — graph verb | 1 day | Low (single verb + tests) |
| C — graph panel | 2-3 days | Medium (gpui rendering + force-directed layout) |
| D — symbol palette | 1-2 days | Low (verb + modal) |
| E — AI context gather | 2-3 days | Medium-high (turn driver hook needs careful testing to not regress chat quality) |
| F — outline + highlight | 1-2 days | Low (verbs are mechanical; GUI consumers are small) |
| G — file watcher | 1 day | Low (notify crate is mature) |
| H — more grammars | ~0.5 days per language | Low |
| I — git blame | 1 day | Low |

Total: ~2 weeks of focused work for the whole plan. Slices A-E are the core of the structural-retrieval payoff and could ship in ~1 week.
