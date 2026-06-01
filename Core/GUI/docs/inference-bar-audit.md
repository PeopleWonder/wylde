# InferenceBar.svelte — Function-by-Function Audit

**Source:** [`core/fletch-gui/src/components/InferenceBar.svelte`](../src/components/InferenceBar.svelte) — 1,539 lines
**Audited:** 2026-05-02
**Restructured:** 2026-05-02 (grouped by source module, split by category)
**Purpose:** Inventory every function, reactive block, event handler, and significant code section ahead of an eventual migration of backend mechanics into `wylde-orchestrator`.

> Categories used throughout:
> - **UI-INTERACTION** — genuinely belongs in the frontend (DOM, user input, rendering).
> - **BACKEND-MECHANIC** — should eventually live server-side (HTTP calls, agent loop, tool dispatch, parsing).
> - **BRIDGE** — user-facing control that signals or configures the backend.
> - **SHARED** — needed in both places, or genuinely unclear.

> Each table below groups every function, variable, reactive block, or code section in `InferenceBar.svelte` that consumes one external module's exports. Items that touch multiple modules appear in each relevant table — so `onMount` shows up under `svelte`, `ollama.js`, `api.js`, `manifests.js`, `conversations.js`, `ollamaSettings.js`, and `stores.js`. The "Internal" table at the end captures everything that doesn't import from any external module: constants, parsers, agent-loop control, pure UI handlers.

---

## svelte — lifecycle hooks + runes

Imports: `onMount`, `onDestroy`. The `$state`, `$derived`, `$effect` runes are Svelte 5 language features; lumped here as "svelte internals."

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| `conversationId` $state (43) — current conversation file id; `null` until first send. | `streaming` $state (45) — true during a single Ollama stream (also read by UI for spinners). | `models` $state (46) — list of pulled model names from `/api/tags`. | `messages` $state (42) — full chat transcript including transient `_toolIndicator` placeholders; UI renders, agent loop mutates. |
| `input` $state (44) — textarea value. | `abortController` $state (49) — for the in-flight `streamChat`. | `loadedModels` $state (47) — currently-resident-in-VRAM models. | `pendingToolCalls` $state (361) — non-null renders the approval prompt; the gate itself is loop control. |
| `messagesEl`, `inputEl` $state (50–51) — DOM bindings. | `modelSupportsTools` $state (362) — sticky-false when Ollama returns "does not support tools"; reset on model change. | `connected` $state (48) — Ollama health flag. | `onMount` (440–486) — boot probe (Ollama/voice), manifest load, settings hydrate, conversation restore, sidebar refresh, start 15 s health polls. |
| `showModelSelect` $state (52) — model picker open/closed. | `agentRunning` $state (367) — true while `runAgentLoop` is executing. | `ejecting` $state (53) — disables eject button during round-trip. | `onDestroy` (488–497) — clear health intervals, abort in-flight stream so VRAM frees, settle pending approval. |
| `pendingImages` $state (363) — pasted image dataURLs awaiting send. | `iterationCount` $state (368) — current agent-loop iteration (1..MAX_ITERATIONS). | `voiceAvailable` $state (58) — Wylde voice service health. | |
| `speakEnabled`, `autoSendVoice` $state (60–61) — voice toggles, persisted to localStorage. | `loopAborted` $state (369) — cooperative cancellation flag. | `listening` $state (59) — true while STT is in progress. | |
| `$effect` (376–390) — persist `autoAcceptTools` / `thinkEnabled` / `speakEnabled` / `autoSendVoice` to localStorage. | `streamingContent`, `streamingThinking` $state (373–374) — token accumulators kept outside `messages[]` so per-token mutations don't trigger array-diff churn. | `autoAcceptTools` $state (366) — persistent toggle; when true, destructive tools skip the prompt. | |
| `$effect` (410–416) — mirror `conversationId` to localStorage and `activeConversationId` store. | `cachedSystemPrompt` $state (744) — last-resolved prompt; written but never read. | `thinkEnabled` $state (375) — persistent extended-thinking toggle. | |
| `$effect` (419–422) — debounced save on `messages` length change (skipped during streaming). | `$effect` (509–512) — reset `modelSupportsTools = true` on model change (covers programmatic changes that bypass `selectModel`). | `modelLoaded` $derived (621–623) — true when `$inferenceModel` is in `loadedModels`; drives eject button enabled/disabled. | |
| `$effect` (427–432) — load conversation requested by sidebar. | `ollamaTools` $derived (78–101) — dedup catalog of `{type:'function',function:{...}}` from `$allTools` + `appToolSchemas`; manifests win on name collision. | `$effect` (499–503) — auto-select first model when none is set. | |
| `$effect` (434–438) — handle `requestNewConversation` from sidebar. | `modelTools` $derived (105) — `ollamaTools` minus `AUTO_MANAGED_TOOLS`; this is what's sent on the wire. | | |
| `totalToolCount` $derived (360) — `modelTools.length` for the model-selector pill. | `knownToolNames` $derived (126) — fast `Set` of names used by the text parser to validate Python-style and bare-JSON matches. | | |
| `messageCount` $derived (515) — fires the scroll-to-bottom effect on new messages. | | | |
| `$effect` (516–520) — scroll on new message via `requestAnimationFrame`. | | | |
| `$effect` (523–529) — 100 ms poll-scroll while streaming (avoids per-token re-render). | | | |
| `$effect` (531–535) — auto-focus input when bar opens. | | | |

---

## stores.js

Imports: `inferenceBarOpen`, `inferenceModel`, `ollamaSettings`, `activeConversationId`, `conversations`, `requestedConversationId`, `requestNewConversation`.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| `toggle()` (537–539) — flip `$inferenceBarOpen`. | `streamModel()` (648–737) — reads `$ollamaSettings` to build per-call options/keep_alive. | `selectModel(model)` (541–545) — set `$inferenceModel`, close dropdown, reset `modelSupportsTools`. | `onMount` (440–486) — hydrates `$ollamaSettings` from `loadOllamaSettings()`; writes `$inferenceModel` (auto-pick fallback) and refreshes `$conversations`. |
| `$effect` (410–416) — write `conversationId` into localStorage and `$activeConversationId`. | | `eject()` (625–635) — reads `$inferenceModel` to know which model to unload. | |
| `$effect` (427–432) — react to `$requestedConversationId` from the sidebar, then reset to `null`. | | `$effect` (499–503) — write `$inferenceModel` when none is selected. | |
| `$effect` (434–438) — react to `$requestNewConversation`, then reset to `false`. | | Health-poll fallback writes a default into `$inferenceModel` if the current one disappears. | |
| `flushSave()` (548–556) — refreshes `$conversations` after every save. | | | |
| Markup (1068–1092) — `{#if !$inferenceBarOpen}` collapsed bar, `$inferenceBarOpen` expanded panel. | | | |
| Markup (1290–1423) — model selector reads `$inferenceModel`; toggle row reads voice/think/auto-accept stores. | | | |

---

## ollama.js

Imports: `checkHealth`, `listModels`, `streamChat`, `unloadModel`, `listLoadedModels`. Direct HTTP client to `127.0.0.1:11434` — no service pipe.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `streamModel(chatMessages)` (648–737) — wires `streamChat` callbacks (`onToken`, `onThinking`, `onToolCalls`); handles empty-response edge case with a synthetic message, runs text tool-call recovery via `parseTextToolCalls`, and recovers from "model does not support tools" by re-streaming without the tools array. | `eject()` (625–635) — `unloadModel` (`/api/generate` with `keep_alive=0`) then `listLoadedModels` so the eject pill updates. | `onMount` (440–486) — initial `checkHealth`, `listModels`, `listLoadedModels`, then 15 s polls of all three. |
| | | `send()` (970–1010) — calls `listLoadedModels` after the agent loop returns to refresh the loaded-models pill. | `onDestroy` (488–497) — abort in-flight `streamChat` so Ollama frees VRAM immediately. |

---

## api.js

Imports: `executeTool`, `voiceHealth`, `voiceListen`, `voiceSpeak`. Pipe-based RPC into Wylde services.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `runAgentLoop()` (854–949) — dispatches non-app tool calls via `executeTool(toolId, args)` (pipe-routed). | `startListening()` (1031–1050) — `voiceListen(15)` (POST `/api/listen`), set `input` to transcript, optionally auto-send. | `onMount` (440–486) — initial `voiceHealth` then 15 s voice-poll. |
| | | `startListening()` calls `voiceHealth` to verify the service is reachable before listening. | |
| | | `speakLastResponse()` (1054–1064) — fire-and-forget `voiceSpeak(content)` for the most recent unspoken assistant message when the TTS toggle is on. | |

---

## @tauri-apps/api/core

Imports: `invoke`. Tauri bridge — only one site.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `buildToolCatalog()` (767–806) — `invoke('write_debug_tools', …)` writes `data/debug-tools.json` on every catalog build (once per agent-loop iteration). | | |

---

## manifests.js

Imports: `allTools`, `loadManifests`. `$allTools` is the live, hot-reloaded list of service tool schemas.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `ollamaTools` $derived (78–101) — reads `$allTools` to assemble the API tools array. | | `onMount` (440–486) — `loadManifests()` once on mount; the file watcher and 10 s reload live inside the module. |
| | `buildToolCatalog()` (767–806) — reads `$allTools` for the human-readable in-prompt catalog. | | |

---

## editorContext.js

Imports: `setActiveFile`, `recordEdit`, `setWorkspace` (aliased `ec_*`). Phase 2 RAG retrieval boost — tells `wylde-rag` which file paths the user just touched. `ec_setWorkspace` is imported but unused in this file.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | | `loadConversation(id)` (569–584) — calls `ec_setActiveFile` so retrieval boosts files referenced in the loaded transcript. | |
| | | `_scanInputForFilePaths(text)` (591–596) — emits up to 5 path-shaped tokens via `ec_recordEdit` to bias RAG. | |
| | | `ec_setWorkspace` — imported but never called; the workspace setter actually fires from inside `WorkspacePicker.svelte`. | |

---

## appTools.js

Imports: `appToolSchemas`, `appToolNames`, `executeAppTool`. Client-side tools (page navigation, model browsing) dispatched in-process.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `ollamaTools` $derived (78–101) — merges `appToolSchemas` into the catalog (manifests win on collision). | | |
| | `runAgentLoop()` (854–949) — `appToolNames.has(name)` selects in-process dispatch via `executeAppTool(name, args)` instead of pipe-routed `executeTool`. | | |

---

## conversations.js

Imports: `listConversations`, `readConversation`, `saveConversation`, `newConversationId`. Local conversation persistence (JSON files via Tauri).

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| `flushSave()` (548–556) — strips `_toolIndicator` messages, calls `saveConversation`, then `listConversations` to refresh the sidebar. | | | `onMount` (440–486) — `readConversation` to restore last conversation from localStorage; `listConversations` to populate the sidebar. |
| `loadConversation(id)` (569–584) — `readConversation` to load the selected transcript. | | | `send()` (970–1010) — `newConversationId()` mints an id on first send. |

---

## memory.js

Imports: `buildMemoryBlock`, `saveEpisodicTurn`. Tier-memory client over the pipe (`wylde-rag`).

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `buildChatMessages()` (808–834) — concatenates `system + toolCatalog + memoryBlock` (where `memoryBlock` is `buildMemoryBlock(latestUserText)`'s return — core context + relevant top-K). | | |
| | `send()` (970–1010) — `saveEpisodicTurn(userMsg, assistantMsg)` as best-effort write to `wylde-rag` after each turn. | | |

---

## ollamaSettings.js

Imports: `loadOllamaSettings`, `buildOllamaOptions`, `resolveKeepAlive`. User-tunable Ollama generation knobs.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `streamModel()` (648–737) — `buildOllamaOptions($ollamaSettings)` and `resolveKeepAlive(...)` for per-call options/keep_alive. | `onMount` (440–486) — `loadOllamaSettings()` hydrates `$ollamaSettings`. | |

---

## systemPrompts.js

Imports: `effectivePrompt as effectiveSystemPrompt`. Resolves the live `inference_bar.chat` prompt (default or override).

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| | `resolveSystemPrompt()` (746–750) — `effectiveSystemPrompt('inference_bar.chat')` plus today's date appended. | | |

---

## WorkspacePicker.svelte

Imports: `WorkspacePicker`. Compact workspace pill embedded in the model selector row.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| Markup (1290–1423) — `<WorkspacePicker />` renders between the eject pill and the toggle row; component owns its own state, including the workspace setter that uses (the otherwise-unused) `ec_setWorkspace`. | | | |

---

## Internal — no external imports

Constants, parsers, agent-loop control, pure UI handlers, debounce/timer handles.

| UI-INTERACTION | BACKEND-MECHANIC | BRIDGE | SHARED |
|---|---|---|---|
| `ACTIVE_KEY` (40) — localStorage key `inferenceActiveConversationId`. | `MAX_ITERATIONS` (35) — hard cap of 10 agent-loop iterations per turn. | `_PATH_RE` (589) — regex for file-extension-bearing tokens (py, svelte, ts, tsx, js, jsx, rs, go, cpp, hpp, h, c, yaml, yml, json, toml, md); user-typed signal that reaches the RAG retriever. | `DESTRUCTIVE_TOOLS` (30–34) — set that triggers the approval prompt unless auto-accept is on (`execute_bash`, `execute_python`, `write_file`, `edit_file`, `delete_file`, `apply_patch`, `git_add`, `git_commit`). UI surfaces approval, but the canonical list belongs server-side. |
| `healthInterval`, `voiceHealthInterval` (54–55) — `setInterval` handles cleared on unmount. | `LOOP_TIMEOUT_MS` (36) — 5-minute wall-clock cap on a single agent run. | `cancelLoop()` (848–852) — UI button side; sets `loopAborted`, aborts in-flight stream, denies any pending approval. (Loop-cancellation half is BACKEND-MECHANIC.) | `requestApproval()` (836–838) — returns a Promise whose resolver is parked in `approvalResolve`; gate semantics are loop-side, the Promise/UI binding is frontend. |
| `saveTimer` (395) — `setTimeout` handle for the debounced auto-save. | `AUTO_MANAGED_TOOLS` (66–69) — `rag_query`, `memory_search`, `graph_query`, `graphrag_query`, `graph_query_local`, `graph_query_fallback`. Filtered out of the catalog so the model doesn't double-call them — they're meant to fire automatically (today, only via `buildMemoryBlock`'s implicit RAG search). | `stop()` (1012–1018) — UI button that calls `cancelLoop` if running, else aborts the bare stream. | `runAgentLoop()` (854–949) — multi-iteration loop (≤10 / 5 min). Per iteration: `buildChatMessages` → `streamModel` → if no tool calls break → else for each call: approval gate (destructive + !auto), push transient `_toolIndicator`, parse args (handle string-encoded JSON), dispatch (`executeAppTool` for app tools, else `executeTool`), filter indicator and push `role:'tool'` message → push empty assistant placeholder for next stream → on exit emit `[Reached tool use limit]` / `[Timeout reached]` if applicable → finally reset all loop state and call `speakLastResponse`. |
| `_lastPathScan` (590) — de-dupes `_scanInputForFilePaths` for unchanged input. | `TOOL_CALL_TAG_RE` (122) — regex for `<tool_call>…</tool_call>` (Qwen/Hermes). | | `send()` (970–1010) — validate input, mint conversationId on first send, append user + empty assistant placeholder, run the agent loop, save, refresh loaded models, fire `saveEpisodicTurn`. |
| `newChat()` (558–567) — cancel running loop, flush, reset all in-memory chat state. | `TOOL_CALL_FENCE_RE` (123) — regex for ` ```tool_call ` / ` ```json ` fenced blocks. | | |
| `loadConversation(id)` (569–584) — local persistence side: cancel loop, flush, `readConversation`, replace `messages`. (`ec_setActiveFile` side noted under editorContext.js.) | `approvalResolve` (370) — module-scoped resolver for the in-flight approval Promise; settled by `handleApprove`/`handleDeny`/`cancelLoop`/`onDestroy`. | | |
| `exportCurrentConversation()` (598–618) — build markdown blob, `URL.createObjectURL`, trigger `<a>` click. | `flushStreaming()` (638–645) — copy `streamingContent`/`streamingThinking` into the last message in `messages[]` as a single mutation (vs per-token). | | |
| `handlePaste(e)` (951–968) — capture clipboard images, read as dataURLs, append to `pendingImages`; `preventDefault()` so the dataURL doesn't leak into the textarea. | `_latestUserText()` (755–761) — walk `messages[]` backwards to the most recent user message; anchors memory retrieval. | | |
| `copyMessage(content)` (1020–1022) — copy assistant message to clipboard. | `tryParseToolCallJSON(raw)` (135–147) — lenient JSON-blob parser; accepts `name`/`function.name`, `arguments`/`parameters`/`function.arguments`. Returns `{function:{name,arguments}}` or `null`. | | |
| `handleKeydown(e)` (1024–1029) — Enter without Shift submits. | `tryParseToolCallJSONStrict(raw)` (154–159) — strict variant rejecting unknown tool names. **Defined but never called** — historical artifact. | | |
| `handleApprove()`, `handleDeny()` (840–846) — settle the parked approval Promise. | `parsePythonArgs(argStr)` (166–232) — hand-rolled state-machine parser for Python-call syntax (`q="…", limit=5, verbose=true`); handles single/double-quoted strings with escapes, nested `{...}`/`[...]` with JSON.parse fallback, unquoted numbers/booleans/None. | | |
| Markup top bar / collapsed bar / empty state / message list / image previews / connection-lost banner / input row / `<style>` (1068–1539) — DOM rendering. Per-message renderer hides `tool` role and `_toolIndicator`s, renders user bubbles, assistant text + thinking `<details>`, copy button, dot-wave loading indicator, blinking cursor during stream, image previews. | `parseTextToolCalls(text)` (239–356) — scan assistant text for six tool-call patterns; return `{calls, cleanedText}`. Patterns: (1) `<tool_call>…</tool_call>` tags (Qwen/Hermes); (2) ` ```tool_call ` / ` ```json ` fences; (3) bare JSON `{...,"name":"...","arguments\|parameters":{...}}` (Llama); (4) `<function=name>{json}</function>` (Hermes/OpenChat); (5) `<\|tool_call_begin\|>…<\|tool_call_end\|>` with optional inner `name<\|tool_sep\|>{json}` (DeepSeek); (6) Python-style `tool_name(k="v",k2=5)` validated against `knownToolNames` (most common with Qwen models). Plus a diagnostic warning when text "looks like" a tool call but no pattern matches. | | |
| Markup tool-approval prompt (1229–1266) — yellow-bordered card listing pending calls + args (truncated 200 chars) + Allow/Deny. | `buildToolCatalog()` (767–806) — human-readable tool list (one line per tool with name, description, param summary) for in-prompt injection; includes the `invoke('write_debug_tools', …)` debug dump. | | |
| Markup model-selector + toggles row (1290–1423) — model dropdown trigger, connection dot, tool-count pill, eject button (cyan loaded / dimmed ejected), `WorkspacePicker`, think/auto-accept/speak/auto-send toggle pills. Model-list dropdown rendered conditionally. | `buildChatMessages()` (808–834) — compose system + toolCatalog + memoryBlock as one system message; map `messages[]` to Ollama wire format (role/content/tool_call_id/tool_calls/images), stripping image dataURL prefixes to raw base64. | | |
| Markup cancel/stop button (1269–1286) — label "Cancel (step N/MAX)" while `agentRunning`, "Stop generating" when only `streaming`. | | | |

---

## Companion file roles

These are the modules the audit subject directly imports from:

- **[`src/lib/ollama.js`](../src/lib/ollama.js)** — Direct HTTP client for the Ollama daemon on `http://127.0.0.1:11434`. No service pipe, no `wylde-orchestrator` involvement. Exports `checkHealth`, `listModels`, `listModelsDetailed`, `showModel`, `deleteModel`, `unloadModel`, `listLoadedModels`, `normalizeOllamaPullName`, `pullModel` (with NDJSON streaming + transient-error retry), and `streamChat` (the NDJSON streaming reader that drives `streamModel`'s callbacks). **Role in migration:** if `wylde-orchestrator` brokers Ollama, `streamChat` and friends move there; `checkHealth` / `listModels` could either move or stay (UI quick check).

- **[`src/lib/memory.js`](../src/lib/memory.js)** — Tier-memory client for `wylde-rag` (over the pipe via `ragCall`). Three operations: `getCoreContext()` (always-in-context core memories, GET `/memory/tier/core_context`), `searchRelevantMemories(q)` (POST `/memory/search`, top-K episodic+semantic+procedural), and `saveEpisodicTurn(...)` (POST `/memory/tier/add`). All three are best-effort with a 4 s timeout and console-debug error swallow. `buildMemoryBlock(userQuery)` runs the two reads in parallel and returns labelled sections for prepending to the system prompt. **Role in migration:** memory injection is part of request building — it follows wherever `buildChatMessages` lives.

- **[`src/lib/api.js`](../src/lib/api.js)** — Pipe-based RPC layer. All Wylde services (`security-api`, `device-gate`, `wylde-orchestrator`, `wylde-trainer`, `wylde-rag`, `wylde-voice`, `wylde-caption`, `wylde-vpn`, `webcrawler-service`, `wylde-launcher`, `tool-registry`, `n8n-service`) are reached via `pipeCall(service, verb, path, body)`, which delegates to the Tauri `pipe_call` command. SSE streams (orchestrator workflow, autotuner) still use HTTP because EventSource cannot ride a named pipe. Exports `executeTool(toolId, params)` — the function the agent loop calls for every backend tool. Also exposes `voiceHealth`, `voiceListen`, `voiceSpeak` used here. **Role in migration:** `executeTool` moves wherever tool dispatch moves; the pipe transport stays as the wire.

- **[`src/lib/appTools.js`](../src/lib/appTools.js)** — Client-side tools the LLM can call to operate the GUI itself. Schemas: `navigate_page` (switch sidebar page), `list_local_models` (proxy to Ollama `/api/tags`), `browse_models` (open the Models page, push filters via the `modelBrowserRequest` store, await page response), `pull_model` (Ollama `/api/pull` with progress notify). `executeAppTool(name, args)` is the in-process dispatcher invoked by the agent loop when `appToolNames.has(name)`. **Role in migration:** these stay client-side by definition — they manipulate UI state — but their schemas need to be visible to whoever assembles the model's tool catalog. If the catalog is built server-side, these schemas must be sent up at session start and the loop must round-trip back to the UI to execute them.

- **[`src/lib/systemPrompts.js`](../src/lib/systemPrompts.js)** — Catalog and override store for every LLM system prompt the platform uses (Agent Orchestra stages, training pipeline, optimizer, voice fallback, and `inference_bar.chat`). Default prompt text is baked in here verbatim from the canonical sources. Persistence is `data/system_prompts.json` via Tauri commands, falling back to localStorage outside Tauri. `effectivePrompt(id)` returns the override text if non-empty, else the default. The InferenceBar only consumes the `inference_bar.chat` entry. **Role in migration:** the override store can live anywhere; the resolution must happen wherever the request is built.

- **[`src/lib/manifests.js`](../src/lib/manifests.js)** — Reads service manifests from `data/manifests/{service}.json` via the Tauri `read_manifests` command. Exposes the `manifests` writable store, the `activePipes` set, and derived stores `servicesByCategory`, `allTools`, `allCommands`, `allSettings`. `allTools` is what populates the model's tool catalog. Also includes a status-derivation function (active < 35 s heartbeat / unresponsive < 90 s / inactive) and a Phase-4 file watcher (`watch_manifests` Rust command + `manifests-changed` event listener + 10 s periodic re-load). **Role in migration:** this is shared infra. The catalog assembled from `$allTools` would also need to be available wherever the chat request is built.

---

## Cross-cutting observations (factual, not prescriptive)

- **Persistence is split** into three layers, all client-side today: localStorage (small toggles, active conversation id), JSON files via `saveConversation` (full transcripts), and `wylde-rag`'s episodic store via `saveEpisodicTurn` (best-effort).
- **`streaming` and `agentRunning`** are distinct: `streaming` is true during a single `streamChat` call; `agentRunning` wraps the entire multi-iteration loop. The Stop button uses `agentRunning` to label "Cancel (step N/MAX)" vs "Stop generating".
- **The text-based tool-call parser is non-trivial** (218 lines including helpers). Six recognized patterns plus a diagnostic warning when text looks like a tool call but doesn't match. This is the largest single block of pure backend-mechanic code in the file.
- **`tryParseToolCallJSONStrict` is dead code** — defined but never called.
- **`ec_setWorkspace`** is imported but never used in this file — the workspace setter lives in `WorkspacePicker.svelte`.
- **The debug dump in `buildToolCatalog`** writes `data/debug-tools.json` on every system-prompt build via `invoke('write_debug_tools', …)`. That's once per agent-loop iteration.
- **`AUTO_MANAGED_TOOLS` filtering** is the only place RAG/memory/graph tools are kept out of the model's hands. As written, they're filtered from both the API tools array and the in-prompt catalog, which means today they're effectively "system-only" — the only RAG/memory call actually happening is `buildMemoryBlock` inside `buildChatMessages`.
- **Approval flow** uses a parked-Promise idiom: `requestApproval` returns a Promise, the resolver is held in the module-scoped `approvalResolve` variable, and three different code paths can settle it (Allow, Deny, cancelLoop/onDestroy). This works because there is at most one in-flight approval at a time.
- **Model-tool support is sticky-false** within a stream: if Ollama returns "does not support tools", `modelSupportsTools = false` and `streamModel` recursively re-streams without tools. The `$effect` on `$inferenceModel` resets it to true on every model change so a single bad response doesn't permanently strip tools for the session.
