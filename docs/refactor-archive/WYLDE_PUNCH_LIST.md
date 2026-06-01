# Wylde — Refactor Punch List

The structural rebuild is **done**. Everything that's left is folding the staged "_*_merge/" / "_*_legacy/" subfolders into clean, in-process modules using the Phase 6 pattern.

## At a glance

| Tier | Items | Total effort | Why this tier |
|---|---|---|---|
| 🟢 Quick wins | 1, 2, 3 | ~½ day each | Small surface, mechanical, no design decisions |
| 🟡 Medium | 4, 5 | ~1 day each | Strip + restructure substantial code (visual assistant slim-down, GUI fletch merge) |
| 🔴 Heavy | 6, 7, 8 | multi-day | Real design decisions or huge surface area |

## Suggested order (one user-facing thread)

1. **#1 Webcrawler** — already wired into Phase 7, just needs Flask-shell cleanup. Smallest. Verifies the pattern.
2. **#2 Device Gate** — same pattern as #1, small.
3. **#3 N8N** — small Flask-to-in-process conversion.
4. **#4 VoiceAssistant** — slim-down per "voice = I/O only" principle. Strips ~165 of 197 files.
5. **#5 GUI** — merge fletch-gui + fletch-web into one Core/GUI/. Real design call but bounded.
6. **#6 Voice** — model-heavy, strip non-I/O code.
7. **#7 Caption** — model-heavy (24 GB Florence-2 weights).
8. **#8 Gateway** — biggest. Pick framework, build server, mount Phase 7 routes.
9. **#9 VPN** — biggest surface area + open question on which routes belong to Gateway.

After #1–#3: pause and decide if Phase 7's extensions actually need the Gateway HTTP layer to be real (i.e. for Wylde_Study's browser extension to talk over the wire). If yes, jump to #8.

---

## 🟢 Quick wins

### #1 — Webcrawler cleanup

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Extensions/Webcrawler/_webcrawler_service/` |
| **Effort** | 🟢 quick |
| **Blocks** | nothing |
| **Why** | Phase 7's extension handler already imports from here — path is live, but the Flask service shell around it is dead weight. |

**What gets done:** Strip the Flask service shell. Promote the live files up one level so the path stops looking like staging.

**Steps:**
1. `grep -r "_webcrawler_service"` to find all importers (should only be `Extensions/Webcrawler/handler.py`).
2. Delete the dead shell: `run.py`, `startup.py`, `consul_client.py`, `discovery.py`, `ipc.py`, `manifest.py`, `errors.py`, `tool_interface.py`, `start_webcrawler.bat`.
3. Keep only what `handler.py` actually uses: `scraper.py`, `extractor.py`, `tools/{fetch,scrape,extract}.py`, `config.py`.
4. Rename `_webcrawler_service/` → either flatten to `Extensions/Webcrawler/core/` or hoist its files up to `Extensions/Webcrawler/`.
5. Update `handler.py`'s imports.
6. Run `phase7_smoke.bat` to verify.

**Done when:** Smoke test still passes, no `_webcrawler_service` folder exists, only what's actually used remains.

---

### #2 — Device Gate cleanup

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Device Gate/_device_gate_merge/` |
| **Effort** | 🟢 quick |
| **Blocks** | nothing |
| **Why** | Same pattern as #1 — small Flask service that should be an in-process module. |

**What gets done:** Promote `device_gate.py` up to `Device Gate/`, drop the Flask shell.

**Steps:**
1. Move `device_gate.py` → `Device Gate/device_gate.py`.
2. Move `data/htpasswd` → `Device Gate/data/htpasswd` (add a "user must populate" note).
3. Delete the Flask shell + `start_device_gate.bat`.
4. Delete `_device_gate_merge/`.

**Done when:** `Device Gate/device_gate.py` is the canonical location, no Flask shell remains.

---

### #3 — N8N service folding

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `N8N/_n8n_service_merge/` |
| **Effort** | 🟢 quick |
| **Blocks** | nothing |
| **Why** | Wrapper around n8n's REST API. Should be a thin in-process client, not a Flask app. |

**What gets done:** Convert the wrapper to a plain client; integrate its tools using filesystem-as-registry.

**Steps:**
1. `client.py` (the n8n REST wrapper) → `N8N/client.py`.
2. Each `tools/*.py` → `Core/harness/tooling/tools/n8n/<tool_id>/{__init__.py, <tool>.py, manifest.json}`.
3. `templates/agent-orchestra.json` → `N8N/templates/agent-orchestra.json`.
4. Delete the Flask shell: `n8n_api.py`, `run.py`, `startup.py`, `consul_client.py`, `discovery.py`, `ipc.py`, `manifest.py`, `errors.py`, `start_n8n_service.bat`.
5. Run smoke test (catalog should grow by # of n8n tools added).

**Done when:** N8N tools show up in catalog when accessed; no Flask shell remains.

---

## 🟡 Medium

### #4 — VoiceAssistant slim-down

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `VoiceAssistant/_wylde_voice_assistant/` |
| **Effort** | 🟡 medium (half day) |
| **Blocks** | nothing |
| **Why** | Per "voice = I/O only" principle, the assistant should be lightweight. Currently it bundles intent parsing (LLM does that), tray UI (frontend concern), app automation (different concern), and its own Flask bridge. Target: ~30 files instead of 197. |

**What gets done:** Strip everything that isn't audio I/O + wake-word. Keep only capture/STT/TTS/VAD/playback. Replace the Flask bridge with a thin pipe to harness.

**Strip (delete entirely):**
- `intent/` — LLM does intent now
- `tray/` — frontend concern, not voice
- `executor/` — app automation belongs elsewhere
- `wylde_bridge.py`, `api.py` — harness handles the LLM call
- `train_nlu.py`, `scripts/train_nlu.py`, `npu_compile.py`, `benchmark_npu.py` — keep in `_legacy/` if you want them
- `install_service.bat`, `uninstall_service.bat`, `start_voice_assistant.bat` — Phase 6 lifecycle handles startup

**Keep (target shape):**
```
VoiceAssistant/
├── audio/         capture, vad, sfx, bargein
├── wake_word/     engine + record_samples + trainer (optional)
├── stt/engine.py
├── tts/engine.py
├── device_manager.py
├── download_models.py
├── config.py
└── run.py         entry point as a Python module, not a Flask service
```

**Done when:** New `VoiceAssistant/` exists alongside the staged folder, smoke test for wake-word → STT → handoff passes, then delete `_wylde_voice_assistant/`.

---

### #5 — GUI: merge fletch-gui + fletch-web

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Core/GUI/_fletch_gui/` + `Core/GUI/_fletch_web/` |
| **Effort** | 🟡 medium (half-to-full day) |
| **Blocks** | nothing |
| **Why** | the Wylde user's intent: one unified `Core/GUI/` without the fletch-* names. Both folders ship `run.py`, `start_*.bat`, `config.yaml` — file collisions need resolving. |

**What gets done:** Pick canonical entry point (likely the desktop Tauri launcher), fold web surface in as a sub-area, drop fletch-* names everywhere.

**Steps:**
1. Sketch target tree on paper first. Two reasonable shapes:
   - **Flat:** `Core/GUI/{src/, src-tauri/, web/}` — desktop is primary, web is alongside
   - **Nested:** `Core/GUI/{desktop/, web/}` — both peers
2. Resolve file collisions: pick one `run.py`, one `config.yaml`, one launcher.
3. Move keepers up one level, drop the `_fletch_*/` prefixes.
4. Clean imports referencing `wylde-launcher` (now in `Core/Lifecycle/`).
5. Build the desktop app — if it builds and runs, you're done.

**Done when:** A developer asking "is this fletch-gui or fletch-web?" gets the answer "neither, it's just GUI."

---

## 🔴 Heavy

### #6 — Voice slim-down

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Voice/_wylde_voice/` (1.4 GB, model weights dominate) |
| **Effort** | 🔴 heavy (model decisions + size) |
| **Blocks** | nothing |
| **Why** | Voice = I/O only per the Wylde user. Models need stable home; Flask shell needs to die. |

**What gets done:** Strip Flask. Move models to `Voice/models/`. Decide Whisper NPU vs CPU as default.

**Steps:**
1. Inventory model variants: which are actually used at runtime vs dev-only?
2. Move keepers to `Voice/models/`.
3. Pick Whisper NPU OR CPU as install-time default.
4. Replace Flask shell with thin in-process module exposing `record()`, `transcribe()`, `synthesize()`.
5. Update consumers (mostly VoiceAssistant after #4).

**Done when:** `Voice/` exposes a clean Python API; old service shell gone; models in stable location.

---

### #7 — Caption integration

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Trainer/Caption/_wylde_caption/` (24 GB Florence-2 weights) |
| **Effort** | 🔴 heavy (size + model handling) |
| **Blocks** | nothing |
| **Why** | Florence-2 caption service. Same pattern as Voice — models to stable location, Flask shell dies, tools register through harness. |

**What gets done:** Move HF weights to `Trainer/Caption/models/`. Replace Flask shell with in-process module + CLI. Promote tools to `Core/harness/tooling/tools/caption/`.

**Steps:**
1. Check `download_models.py` to confirm where weights *should* live (HF cache convention?).
2. Move weights there.
3. Promote `captioner.py`, `batch.py`, `video.py`, `cli.py` up to `Trainer/Caption/`.
4. Each callable capability → harness tool: `caption_image`, `caption_video`, `caption_batch`.
5. Delete Flask shell.

**Done when:** Caption tools show up in catalog; CLI works; weights in stable location.

---

### #8 — Gateway server-side build-out

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Gateway/_security_api_merge/` |
| **Effort** | 🔴 heavy (framework decision + integration) |
| **Blocks** | Phase 7 extensions over HTTP, future remote-access features |
| **Why** | Currently `Gateway/` only has `client.py` (egress). The server-side (auth, rate-limit, TLS, route mounting) is all staged. Phase 7's extension routes need this to actually work over HTTP. |

**Open design questions:**
1. **Server framework.** Staged code uses Flask. FastAPI/Starlette is on the table. Pick one.
2. **In-process vs separate process.** Phase 6 proved we can do everything in-process for tooling. Same question for Gateway?

**What gets done:** Pick framework. Promote `app.py`, route groups (`gateway_routes`, `tool_registry_routes`, `tool_discovery_routes`, `egress_routes`), middleware (auth, rate-limit, trace logging) to `Gateway/` proper. Mount `extension_routes.py` (Phase 7 placeholder) as a real route group.

**Steps:**
1. Decision sit-down: Flask vs FastAPI, in-process vs separate process.
2. Move `app.py` + middleware + route groups up one level.
3. Wire `extension_routes.handle_extension_request` into the real HTTP layer.
4. Test via Wylde_Study browser extension — actual end-to-end HTTP call should land on the right handler.

**Done when:** Browser extension can call Wylde_Study endpoints over HTTP and get a real response.

---

### #9 — VPN integration

| | |
|---|---|
| **Status** | ⬜ todo |
| **Path** | `Device Gate/VPN/_wylde_vpn/` |
| **Effort** | 🔴 heavy (large surface + open architectural question) |
| **Blocks** | nothing right now |
| **Why** | Big surface area: gateway proxy + many gateway routes (chat, conversations, devices, images, link, models, push, rag, services, settings, system, tools, training, voice, workflows), NAT traversal, STUN/TURN, mDNS, DDNS, monitoring, tools layer. |

**Open design question first (resolve before integration):**
- The big `gateway/routes/` tree overlaps with #8 Gateway. Are these routes "Gateway routes that happen to be tunneled" or "VPN-internal routes"? Answer determines if they migrate to Gateway or stay in VPN.

**What gets done:** Resolve the routes question. Decide install/download story (separately downloadable per the Wylde user — does Wylde ship `download_vpn.py`?). Decide native binary location (WireGuard userspace etc.).

**Steps:**
1. Map every route in `_wylde_vpn/gateway/routes/` and tag: Gateway-shaped (move) vs VPN-internal (stay).
2. Decide installer: bundled vs on-demand download.
3. Decide native binary placement: `Device Gate/VPN/bin/` vs `Wylde/_native/`.
4. Replace `entrypoint.sh` (Linux-only) with a Windows-friendly entry point if needed.
5. Move keepers up to `Device Gate/VPN/`, drop the `_wylde_vpn/` prefix.

**Done when:** Routes are correctly placed (in Gateway or VPN, not duplicated), VPN can be enabled/disabled cleanly, install story is documented.

---

## TODOs in code (not punch-list items, but track them)

| Where | What | Effort |
|---|---|---|
| `tools/meta/graph_query/graph_query.py:46-51` | `TODO(graph-aware-rag)` — hybrid traversal + vector reranking when rag.search gains graph-distance signal | 🟡 medium (rag refactor needed first) |
| `tools/rag/{rag_feedback, rag_misses, rag_chunk_usage}` | `not_implemented` stubs (validation logic preserved) — need `miss_log` layer ported into `harness/memory/` | 🟢 quick (one file port) |
| `tools/rag/rag_ask` | Returns raw search hits — full pipeline (HyDE → hybrid retrieval → cross-encoder rerank → forced-citation generation) not yet ported | 🟡 medium |
| `harness/memory/rag.build_memory_block` | Synchronous (was parallel ThreadPoolExecutor in legacy injection.py) — confirm chat-turn latency or thread it | 🟢 quick (decide + maybe thread) |
| `backend_routing._lookup_profile` | Lazy-imports `models` — string needs updating now that registry moved to `Core/harness/model_registry/` | 🟢 quick (one-line fix) |
| `Extension Bridge/__init__.py:15` | `SyntaxWarning: "\ " is an invalid escape sequence` from a docstring — fix with `r"""..."""` | 🟢 trivial |

---

## What NOT to delete

These all stay (locked decisions):

- `Wylde/_legacy/` — the Wylde user handles this when ready
- `Wylde/Core/Lifecycle/_legacy/wylde_launcher/` — kept for diff/cribbing while Lifecycle reaches parity
- `Wylde/N8N/_legacy/` — orchestra archive for later N8N conversion
- `Wylde/Core/harness/_legacy/orchestrator_api/` — API contract reference + bug-location NOTE.md
- `default/` — production folder external to Wylde/, the Wylde user's call
- All migration map .md docs (audit, inventories, BACKEND/MEMORY/TOOLING_MIGRATION_MAP, PHASE5/6/8 plans)

---

## Pattern reminder

Every item above follows the **Phase 6 pattern**:
1. In-process module, not a Flask service
2. Filesystem-as-registry where applicable (manifest.json per tool)
3. No HTTP between Wylde components
4. No GUI hop for tool calls (LLM → harness/tooling, never through inference bar)
5. Clean break — drop legacy paths, modernize imports

If something doesn't fit this pattern, surface it as a question rather than improvising.
