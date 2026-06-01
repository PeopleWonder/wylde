# Gateway Routes Audit — Planning Document

**Audit date:** 2026-05-09
**Scope:** `Gateway/routes/*.py` (active codebase, excluding `_legacy/`)
**Mode:** Planning only — no source edits, no file moves, no deletes.

---

## 1. Headline counts

| Metric                   | Count        |
|--------------------------|--------------|
| Route files before audit | **17**       |
| KEEP                     | **4**        |
| DROP                     | **11**       |
| REPURPOSE                | **0**        |
| NEEDS-DISCUSSION         | **2** (`link.py`, `push.py`) |
| Route files after audit  | **4–6**      |

**Estimated LOC saved if all DROPs execute:**

- Route files dropped: ~1,278 LOC (`chat 70` + `devices 75` + `images 127` + `models 121` + `rag 59` + `services 171` + `settings 94` + `system 64` + `training 189` + `voice 77` + `workflows 308`)
- `Gateway/proxy_core.py` (228 LOC) becomes dead — only routes use it.
- `Gateway/streaming.py` (191 LOC) becomes dead — only routes use it.
- **Total: ~1,697 LOC** of pure deletion (plus ~30 LOC in `routes/__init__.py` register block, plus dead Pydantic helpers).

---

## 2. Per-route table

> Caller search excluded `_legacy/`, `node_modules/`, `__pycache__/`, `target/`, `dist/`, plus the Gateway folder itself.
> The defining fact uncovered during caller-search: **`Core/GUI/src/lib/api.js` exclusively uses Tauri `invoke('pipe_call', ...)` to talk to service named pipes — it never hits Gateway HTTP routes.** The only HTTP target the GUI ever uses (`FLETCH_WEB_URL = 127.0.0.1:3000`) is **fletch-web** (a separate process under `Core/GUI/web/fletch_web.py`), and it is used **only** for SSE streams that EventSource cannot ride a pipe. fletch-web likewise does not call Gateway. Nothing in the active codebase references Gateway's port (`127.0.0.1:8005`).

| File | What it does (one sentence) | Callers found | Verdict | Reason | Replacement / what caller does instead |
|---|---|---|---|---|---|
| `chat.py` | Streamed chat completion + raw generation, proxies to Ollama at 127.0.0.1:11434 (NDJSON → SSE). | None. GUI calls Ollama via the orchestrator pipe (see `api.js` comment line ~38: "Ollama is no longer reached from the browser side directly"). Harness uses its own `ollama_client.py` (`Core/harness/backend/ollama_client.py:352`). | **DROP** | No callers. Everything that needs Ollama already calls 127.0.0.1:11434 directly or via the harness. Ollama is a local daemon, not a network boundary. | Continue to use `harness/backend/ollama_client.py` in-process. If a remote browser ever needs streaming chat, fletch-web already proxies `/ollama/*` (see `fletch_web.py` prefix map). |
| `devices.py` | LAN device-gate proxy (pending/approve/deny/approved/patch/delete). | None over HTTP. GUI calls device-gate pipe directly: `api.js:115-148` uses `pipeCall(SVC_GATE, ...)` against `\\.\pipe\wylde-device-gate`. | **DROP** | The GUI already bypasses Gateway here — proxying to a pipe through HTTP that is then immediately re-pipe-called is round-trip noise. | GUI already uses Tauri pipe directly. No replacement needed. |
| `egress.py` | Outbound proxy with allowlist + kill switch + audit. | `Gateway/client.py` (in-process); harness via `wylde-gateway` pipe action. | **KEEP** | Core Gateway. (Skipped per instructions.) | — |
| `extensions.py` | `/extensions/<name>/<endpoint>` dispatch into `Extensions/`. | `Gateway/extension_routes.py`; in-process. | **KEEP** | Core Gateway. (Skipped per instructions.) | — |
| `health.py` | `/health`, `/ready`, `/live`. | Lifecycle / supervisor. | **KEEP** | Core Gateway. (Skipped per instructions.) | — |
| `images.py` | ComfyUI image-gen proxy (generate, library list/get/delete, models, loras). Reads/writes filesystem under `data/images/` and proxies HTTP to 127.0.0.1:8014. | None found in GUI or harness. The image-gen surface isn't in `api.js` at all. | **DROP** | (a) No callers. (b) The library-listing logic *reads the local filesystem* directly — that is a textbook "GUI has full filesystem access" case that should not pass through HTTP. (c) The generate proxy is to a sibling local service. | If/when an Images page is added: read filesystem directly from Tauri Rust side; call ComfyUI/image-gen via its own pipe (or via Tauri HTTP if it's HTTP-only). Don't put it on Gateway. |
| `link.py` | VPN-self proxy: status / peers / stun / pair / qr / peers-remove. Forwards to 127.0.0.1:8020 (wylde-vpn HTTP). | None over HTTP. GUI uses pipe directly: `api.js:362-386` `pipeCall(SVC_VPN, ...)`. | **NEEDS-DISCUSSION** | This is the *one* route family that has a defensible reason to exist per the file's own docstring: *"Putting the route behind Gateway keeps every mobile-visible URL on one tier; the VPN management port stays loopback-only."* But under the new model, "remote browsers tunnel in and become local" — so they reach the GUI and would call the VPN via the same pipe path the local Tauri uses. The HTTP fronting is only useful if a remote client is **NOT** running through the GUI (e.g. native mobile app speaking HTTP). **the Wylde user needs to decide: is there still a non-GUI mobile client?** | If no native mobile-app surface remains: DROP, GUI uses VPN pipe directly. If yes: KEEP and accept that link.py is the ingress for the mobile app. |
| `models.py` | Ollama model list/running/pull/generate/delete + orchestrator model registry (list/search/discovery/swap). | None over HTTP. GUI uses pipe directly to orchestrator (`api.js: listRegisteredModels, getDiscoveryStatus, triggerModelDiscovery, getSwapPrompts, respondToSwapPrompt` all `pipeCall(SVC_ORCH, ...)`). Ollama list/pull use the orchestrator harness or Ollama HTTP directly. | **DROP** | (a) No callers. (b) Half of it (registry/swap) targets `wylde-orchestrator` which the WYLDE_OPEN_ITEMS doc explicitly calls "DEAD — orchestrator is gone". (c) Ollama is a local daemon — direct call. | Orchestrator-side endpoints will move to harness when the orchestrator itself is reorganised; Ollama list/pull either go through harness or are exposed by fletch-web for browser-only mode. |
| `push.py` | Peer push subscribe/unsubscribe/pending. Forwards to wylde-vpn pipe (`peers.push` storage). | `linkPushSubscriptions` / `linkPushNotifyInternal` in `api.js:366,380` use the VPN pipe directly, **not** `/api/push`. The only HTTP reference to `/api/push/subscribe` is a docstring/UI snippet in `RemoteAccess.svelte:629`. | **NEEDS-DISCUSSION** | Same question as `link.py`: is there a non-GUI mobile/native-app caller that needs HTTP? The note in `push.py` ("the mobile client sends `public_key` explicitly because the tunnel is the proof") is consistent with a native mobile app. | If no native mobile client exists: DROP — the GUI already drives push subscriptions via the VPN pipe. If yes: KEEP this single route as the mobile-app push surface; no other Gateway routes need to come back. |
| `rag.py` | wylde-rag query/ingest/collections proxy over pipe. | None. RAG is now a module (`Core/harness/memory/rag.py`), not a separate service. The wylde-rag pipe target most likely doesn't even exist in the new arch. | **DROP** | (a) No callers. (b) Target service is dead — RAG moved into harness/memory. WYLDE_OPEN_ITEMS F3 confirms: "rag → repoint to harness/memory/rag.py direct calls". | Callers (currently none in active code) call `harness.memory.rag` in-process. |
| `services.py` | Manifest-driven service status. | (Dropped in parallel work — not re-evaluated.) | **DROP** | Per task brief: parallel "GUI service-lookup rewire" is already removing this. | (Per parallel task.) |
| `settings.py` | Ollama JSON settings (filesystem read/write of `data/settings/ollama.json`) + sysmon hardware proxy. | None. GUI's `ollamaSettings.js` stores settings *client-side* — no Gateway call. Hardware: GUI uses `sysmonCall` (pipe). | **DROP** | (a) No callers. (b) Reading/writing a JSON file on disk over HTTP from a GUI that has filesystem access is exactly the overengineering the new model rules out. (c) Sysmon hardware proxy is just a pipe re-wrap. | Tauri Rust side does direct filesystem read/write of `data/settings/ollama.json`; sysmon hardware via pipe. |
| `system.py` | wylde-sysmon metrics + hardware + VRAM proxy (per-resource panels). | None. GUI uses `sysmonCall` / `resMonCall` (`api.js:495-499`) → pipe directly. | **DROP** | No callers; pure pipe re-wrap. Per WYLDE_OPEN_ITEMS F3: sysmon already folded into `Core/Resource Monitor/`. | GUI continues to call sysmon pipe directly. |
| `tool_registry.py` | Tool catalog read-only. | (Skipped per instructions.) | **KEEP** | Core Gateway. | — |
| `training.py` | wylde-trainer jobs / datasets / VRAM mode / eval / register pipe proxy. | None over HTTP. GUI uses `trainerTool` pipe wrapper (`api.js:393`). The one HTTP holdout — `Training.svelte:373` `EventSource(\`${TRAINER_URL}/api/training/${jobId}/stream\`)` — points at **trainer-direct (port 8013)**, not Gateway. | **DROP** | No callers. Long timeouts on these proxies (eval=900s, generate=600s) are also exactly the kind of stream-over-HTTP that should not transit a gateway. | GUI continues to use trainer pipe directly; SSE stream goes direct to trainer's HTTP listener (already does). |
| `voice.py` | voice-assistant pipe proxy (command/speak/transcribe/health). | None. GUI calls `pipeCall(SVC_VOICE, ...)` directly: `api.js:478-486`. | **DROP** | No callers; pure pipe re-wrap. | GUI continues to use voice pipe directly. |
| `workflows.py` | Orchestrator catalog/exec/versioning/traces/optimizer/autotuner + n8n bridge. By far the largest file (308 LOC). | None over HTTP. The entire orchestrator surface in `api.js` (workflows / autotuner / optimizer / agent turn / traces / lint) is `pipeCall(SVC_ORCH, ...)`. SSE streams go to fletch-web at `${FLETCH_WEB_URL}/orch/...`, **not Gateway**. The n8n bridge in this file uses Gateway, but n8n's own HTTP API is on `127.0.0.1:5678` and could be hit directly. | **DROP** | (a) No callers. (b) WYLDE_OPEN_ITEMS F3 explicitly says: "wylde-orchestrator → strip from workflows route entirely (orchestrator is dead)". (c) n8n bridge is a 4-method shim over n8n's own REST API — caller can hit n8n directly. | Orchestrator surface dies with the orchestrator. n8n REST API is reachable directly from the GUI / harness; any wrapping needed lives wherever the n8n integration lives. |

---

## 3. Suggested final Gateway shape

If both NEEDS-DISCUSSION routes drop, Gateway becomes a **4-route service**:

| Route | One-line justification |
|---|---|
| `health.py` | Liveness/readiness for the launcher and external monitors. |
| `tool_registry.py` | Read-only catalog endpoint — single source of truth for tools, used by other in-process callers. |
| `egress.py` | The actual gateway: outbound to the public internet with allowlist + kill switch + audit. **This is the entire reason Gateway exists.** |
| `extensions.py` | Routed dispatch into the Extensions tree. Keeps a stable `/extensions/<name>/<endpoint>` URL even as extensions come and go. |

If `link.py` and/or `push.py` survive (native mobile app case), they sit alongside as the **only** mobile-app HTTP ingress points — a clean, minimal "external client tier" instead of the current 13-file sprawl.

The Phase-9 docstring in `routes/__init__.py` (about hoisting in the ex-VPN mobile-bridge family) becomes incorrect after these drops and should be rewritten to state Gateway's now-narrower scope.

---

## 4. Dead helpers / Pydantic models

The proxy routes don't define Pydantic request models — they all use `await request.json()` and pass dicts through. So there are no per-route request schemas to clean up. However, two **shared helpers go fully dead** when the proxy routes drop:

- **`Gateway/proxy_core.py` (228 LOC)** — provides `pipe_call`, `http_call`, `ok`, `error`. Grep confirms it is imported **only** from the route files: `chat.py, devices.py, images.py, link.py, models.py, push.py, rag.py, services.py, settings.py, system.py, training.py, voice.py, workflows.py`. (Plus its own self-import for `proxy_core.ok` / `proxy_core.error`.) If all DROP routes go and `link.py + push.py` stay, `proxy_core` stays — but if both NEEDS-DISCUSSION routes also drop, `proxy_core.py` is fully dead.
- **`Gateway/streaming.py` (191 LOC)** — provides `ndjson_to_sse` and `passthrough_sse`. Imported only by `chat.py`, `models.py`, `workflows.py` (all DROP). Goes fully dead in every scenario above.
- The `_read_body` private helper duplicated across 8+ route files dies with them.
- The hard-coded URL/pipe constants (`OLLAMA_URL`, `IMAGE_GEN_URL`, `VPN_HTTP`, `ORCH_PIPE`, `RAG_PIPE`, `TRAINER_PIPE`, `VOICE_PIPE`, `SYSMON_PIPE`, `N8N_HTTP`, `LIBRARY_DIR`) are local to each file and die with them.

**No external module imports anything out of these route files** (verified by grep for `from Gateway.routes`).

---

## 5. Mis-categorized concerns / what would be lost by a naive delete

This was the hardest section to fill — the routes are so thin (almost all are 5-line proxy wrappers) that there is **very little load-bearing code** to lose. Specifically:

- **No auth checks beyond `Depends(require_local)`.** Every Phase-9 route gates on the same single dependency — there is no per-route auth, no rate-limit-by-user, no audit hook that's specific to a route. The audit log is wired at the **middleware** layer (`Gateway/middleware/audit_log.py`) and is route-agnostic; it survives untouched.
- **No business logic.** Even the routes that look stateful aren't:
    - `images.py` library list/get/delete is pure filesystem manipulation — but the GUI has filesystem access, so this logic moves *out* of Gateway, not into it.
    - `settings.py` Ollama-defaults read/write is pure filesystem — same.
    - All the rest are 1:1 pipe/HTTP forwards.
- **One actual concern that's worth surfacing:** `link.py`'s docstring contains the *only* explicit articulation of the "single auth boundary" principle for mobile clients. If you drop `link.py` you lose that comment, and the next person to wonder why mobile peers don't authenticate per-request will have to rediscover it. **Move that paragraph into `auth/__init__.py`'s docstring before deleting `link.py`.**
- **Mis-categorized item:** `training.py` exposes `/training/vram-mode` — that's a sysmon concern (`pipe_call("wylde-sysmon", "/vram/...")`). The system.py file *also* has `/system/vram/status`. If sysmon ever does come back behind Gateway, these need to live together, not in the trainer surface.
- **The n8n bridge inside `workflows.py`** (`/api/workflows/n8n*`) is the only piece of `workflows.py` that doesn't touch the dead orchestrator. If for some reason n8n needs a Gateway-fronted CRUD URL (it doesn't — it's a localhost daemon already), pull just those four methods into a new `n8n.py` rather than keeping the whole file alive. Most likely answer: drop them too.
- **Phase 9 docstring drift.** `routes/__init__.py` claims an "ex-VPN ingress family" and a Phase-9 hoist. After the drops the comment is wrong. Update it as part of the same PR.

**Naive-delete risk summary:** very low. The only code worth pulling out before deletion is the auth-boundary paragraph in `link.py`'s docstring.

---

## 6. Open questions the Wylde user needs to answer

1. **Native mobile-app client — does it exist or not?**
   This is the single decision that gates `link.py` and `push.py`. The route file docstrings assume a mobile client that hits Gateway over HTTP (via the WyldeLink tunnel). If the only "remote" surface in the new model is "remote browser tunnels in and renders the GUI", both files DROP. If a separate native mobile app is still planned, both stay (and `link.py` may need to grow back to cover the rest of `api/link/*` that the GUI uses on the pipe today: `register`, `connect`, `services`, `push/notify`, `push/subscriptions`, `config GET/PATCH`, `restart`).

2. **`workflows.py` n8n bridge — kill it or move it?**
   The orchestrator pieces of `workflows.py` are dead-on-delete (orchestrator is gone). The four n8n CRUD methods (`/api/workflows/n8n`, `/api/workflows/n8n`, `/api/workflows/n8n/{id}/execute`, `/api/workflows/n8n/{id}`) are 1:1 forwards to `127.0.0.1:5678/api/v1/workflows/...`. Should they (a) die entirely (caller hits n8n directly), or (b) survive as their own tiny `n8n.py` route? Recommend (a).

3. **`images.py` — is there ever going to be an Images page that needs network access?**
   Today there isn't even a caller. The library logic reads filesystem; the generate logic proxies to ComfyUI. If a future remote-browser flow needs to browse the image library *through the tunneled GUI*, the GUI handles it locally and no route is needed. Confirming "yes drop, future Images surface lives in GUI" lets us delete cleanly.

4. **fletch-web overlap with Gateway.** fletch-web is *itself* an HTTP gateway — Flask-based, with its own prefix-map for the same set of services. After Gateway shrinks to 4 routes, what's the relationship with fletch-web? Is fletch-web still the browser-only-mode entry, with Gateway being purely the egress + extensions tier? (This is bigger than the route audit, but the audit surfaced it.)

5. **Phase-9 routes/__init__.py comment block.** The doc text in `Gateway/routes/__init__.py` describes an ex-VPN mobile-bridge family that is being torn out. Confirm the rewrite: Gateway is now (egress, extensions, health, tool-registry) plus optional (link, push) for the native-mobile case.

---

## 7. Recommended execution order

Grouped by risk. All assume the parallel `services.py` work has landed first.

### Group A — zero-risk deletes (no callers, dead targets)
Delete in any order, can be one PR:

1. `rag.py` — target service is in harness, no HTTP callers.
2. `system.py` — pure pipe re-wrap, no callers.
3. `voice.py` — pure pipe re-wrap, no callers.
4. `devices.py` — pure pipe re-wrap, no callers, GUI uses pipe.
5. `settings.py` — filesystem + pipe re-wrap, no callers.
6. `chat.py` — Ollama proxy, no callers, harness owns the Ollama path.
7. `images.py` — filesystem + HTTP proxy, no callers.
8. `models.py` — Ollama + dead-orchestrator, no callers.

(Routes 1–8: ~787 LOC removed, plus `routes/__init__.py` register lines, plus update `__init__.py` docstring.)

### Group B — slightly riskier (touches "dead" services)
Sequence after A:

9. `training.py` — confirm wylde-trainer is genuinely either gone or does its own SSE direct (the GUI already EventSources it directly), then delete.
10. `workflows.py` — drop the orchestrator surface (already dead). Decide on n8n bridge (Q2 above) before merging; default recommendation is drop the whole file.

### Group C — needs the Wylde user's decision (Q1)
Sequence after A–B:

11. `link.py` — DROP only after confirming there is no native mobile-app surface. If KEEP, also grow it back to the full `/api/link/*` set the GUI currently uses on the pipe.
12. `push.py` — same condition as `link.py`.

### Group D — janitor pass (do alongside C)

13. Once all DROP routes are gone, delete `Gateway/proxy_core.py` (228 LOC) and `Gateway/streaming.py` (191 LOC) — both have zero callers outside the dropped routes.
14. Rewrite `Gateway/routes/__init__.py` docstring to describe the new shape (egress + extensions + health + tool_registry, plus link/push if those survive).
15. Update or remove `Gateway/manifest.json` description ("All external HTTP traffic in or out … flows through here") — accurate after the cleanup, but worth re-reading.

### Risk summary
- **Safe-delete pile** (A): 8 files, fully dead. Single PR, ~hour of work.
- **Quick-decision pile** (B): 2 files, rests on "orchestrator confirmed dead" and "n8n bridge unwanted". Few hours.
- **the Wylde user-decision pile** (C): 2 files, blocked on Q1 (native mobile app exists or not).
- **Janitor pile** (D): trivial, follows the others.

**Total:** safely doable in one focused day for groups A + B + D, plus whatever time the mobile-app decision takes for C. **Riskiest part:** the mobile-app question — getting that wrong means either keeping ~170 LOC of dead proxies (low harm) or having to add routes back later (medium annoyance, no data risk).
