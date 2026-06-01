# Phase 8 — Integration Punch List

The vault-root and superseded-legacy cleanup is done (see `phase8_cleanup.bat`).
What's left for Phase 8 (and any 8.x follow-ups) is folding each remaining
`_*_merge/` and similar staging subfolder into its parent service. Each
item below names the staging path, where the code came from, what's
already wired into the new service, what's outstanding, and a suggested
next step.

Service-by-service checklist — work top-down or by priority. Items 1, 7,
and 9 are the ones blocking Phase 7's "extensions actually do things over
HTTP" story; the rest are independent.

---

## 1. Gateway — `Gateway/_security_api_merge/`

- **Source.** Lifted from legacy `core/security-api/` during Phase 5A.
  Flask app + auth, rate-limit, TLS, secrets backends (file/Vault),
  service discovery, anomaly detector, log analyzer, MCP server/client,
  and a handful of route groups (`gateway_routes.py`,
  `tool_registry_routes.py`, `tool_discovery_routes.py`,
  `egress_routes.py`).
- **Wired.** `Gateway/client.py` exists at the top level. Phase 7 added
  `Gateway/extension_routes.py` as a *contract spec / in-process shim*
  for `POST /extensions/<name>/<endpoint>` — it documents the route
  group the real Gateway must mount but does not stand up an HTTP
  server.
- **Outstanding.**
  1. Pick a server framework (Flask is what the staged code uses;
     swapping to FastAPI/Starlette is on the table — make the call once).
  2. Promote `app.py`, route groups, and middleware (auth, rate-limit,
     trace logging) into `Gateway/` proper, dropping the
     `_security_api_merge/` prefix.
  3. Mount `extension_routes.py` (or its successor) as a real route
     group on the chosen framework.
  4. Decide what stays in-process vs what runs as a separate process —
     the staged code assumes a Flask service on a named pipe; Phase 6
     proved we can do everything in-process for tooling, so the
     same question applies here.
- **Next step.** One sit-down to lock the framework + transport choice,
  then a single integration commit that moves the live files up one
  level and wires them into a `Gateway.app:create_app()` factory.

## 2. N8N — `N8N/_n8n_service_merge/`

- **Source.** Legacy `core/n8n-service/` — Flask wrapper around n8n's
  REST API. Tools for list/get/create/edit/delete/execute workflows,
  plus an `agent-orchestra.json` template.
- **Wired.** Nothing yet at `N8N/` top level beyond this staging folder
  and the `_legacy/` archive the Wylde user is keeping.
- **Outstanding.**
  1. The "in-Wylde control surface" for n8n needs to be a thin
     in-process module, not a Flask service. Convert
     `n8n_api.py` to a plain client.
  2. Fold the `tools/*.py` files into harness tooling using the
     filesystem-as-registry convention used in
     `Core/harness/tooling/tools/` — each tool gets its own folder with
     a `manifest.json`.
  3. Keep `templates/` as a real template directory at `N8N/templates/`.
- **Next step.** Move `client.py` → `N8N/client.py`; convert each
  `tools/*.py` into the new tool layout; delete the Flask app shell
  (`run.py`, `startup.py`, `consul_client.py`, `discovery.py`, `ipc.py`,
  `manifest.py`, `errors.py`, `start_n8n_service.bat`).

## 3. Voice — `Voice/_wylde_voice/`

- **Source.** Audio I/O service (~1.4 GB on disk, dominated by Whisper
  Small + Whisper Small NPU + Piper + Kokoro model weights).
- **Wired.** Models present and the OpenVINO encoder/decoder cache is
  warm (lots of `.cl_cache` blobs). `tts_engine.py`, `run.py`,
  `startup.py` exist.
- **Outstanding.**
  1. Per the Wylde user's framing, Voice is **I/O only** — strip anything
     pretending to be more than that.
  2. Move models out of the staging folder into a stable
     `Voice/models/` location so the integration doesn't move
     model paths every time.
  3. Decide whether to keep Whisper NPU alongside Whisper CPU or to
     pick one as default at install-time.
  4. Replace the Flask shell with a thin in-process module exposing
     `record()`, `transcribe()`, `synthesize()` — same shape as the
     refactor pattern used for tool_runner in Phase 6.
- **Next step.** Inventory which model variants are actually needed at
  runtime vs which are dev-only; promote the keepers to `Voice/models/`;
  rewrite `run.py` as a Python module rather than a service entrypoint.

## 4. VoiceAssistant — `VoiceAssistant/_wylde_voice_assistant/`

- **Source.** Wake-word + STT + intent + tray + executor + Flask bridge
  app from legacy `core/wylde-voice-assistant/` (~138 MB, 197 files).
- **Wired.** Nothing at `VoiceAssistant/` top level yet.
- **Outstanding — major slim-down per the Wylde user's locked decision.** Strip
  the following entirely:
  - `intent/` — LLM does intent parsing now, not a separate classifier.
    Delete `classifier.py`, `parser.py`, `slot_extractor.py`,
    `dataset_loader.py`, `custom_loader.py`, `fallback.py`.
  - `tray/` — UI is a frontend concern; the assistant doesn't ship its
    own tray.
  - `executor/` — app automation lives elsewhere (Device Gate / Gateway /
    a future automation service). Drop `apps.py`, `confirm.py`,
    `custom.py`, `dispatch.py`, `files.py`, `models.py`, `shell.py`,
    `system.py`.
  - `wylde_bridge.py`, `api.py`, `install_service.bat`,
    `uninstall_service.bat`, `start_voice_assistant.bat` — the harness
    handles the LLM call; no Flask bridge needed.
  - `train_nlu.py`, `scripts/train_nlu.py`, `npu_compile.py`,
    `benchmark_npu.py` — keep in `_legacy/` if you want them, otherwise
    drop.
- **Target shape (~30 files).** `audio/` (capture, vad, sfx, bargein),
  `wake_word/` (engine + record_samples + trainer if you keep custom
  wake words), `stt/engine.py`, `tts/engine.py`, `device_manager.py`,
  `download_models.py`, `config.py`, `run.py` — and a thin pipe to
  Wylde Core via the harness's normal API instead of an HTTP bridge.
- **Next step.** Stand up a `VoiceAssistant/` (no underscore prefix)
  folder with the target shape, copy in just the keep-files,
  smoke-test wake-word→STT→handoff, then delete
  `_wylde_voice_assistant/`.

## 5. Caption — `Trainer/Caption/_wylde_caption/`

- **Source.** Legacy `core/wylde-caption/` — Florence-2 captioning
  service (~24 GB of model weights under
  `models/hf/models--microsoft--Florence-2-large/`).
- **Wired.** `captioner.py`, `batch.py`, `video.py`, `cli.py`,
  `caption_api.py`, `download_models.py` all present. Florence-2
  weights cached.
- **Outstanding.**
  1. Move the Hugging Face weights to a stable, deduplicatable
     `Trainer/Caption/models/` path (not under a `_*` staging folder
     where they look temporary).
  2. Replace the Flask shell (`run.py`, `startup.py`, `consul_client.py`,
     `discovery.py`, `ipc.py`, `manifest.py`, `errors.py`,
     `caption.bat`, `start_caption.bat`) with an in-process caption
     module + a CLI entry point.
  3. Turn `tools/`-style tools (caption single image, caption video,
     caption batch) into proper harness tools under
     `Core/harness/tooling/tools/caption/`.
- **Next step.** Check `download_models.py` to confirm where the weights
  *should* live, move them there, then promote the rest.

## 6. Core/GUI — `Core/GUI/_fletch_gui/` + `Core/GUI/_fletch_web/`

- **Source.** Both staged in Phase 5C from legacy `core/fletch-gui/`
  (Tauri + Svelte desktop app) and `core/fletch-web/` (Python web
  surface).
- **Wired.** `Core/GUI/` itself is empty — *only* these two staging
  folders live there.
- **Outstanding — the Wylde user's locked intent: merge both into a unified
  `Core/GUI/` without the `fletch-*` names.**
  1. Resolve file collisions: both folders ship a `run.py`, both ship
     a `start_*.bat`, both ship a `config.yaml`. Pick which is the
     canonical entry point (likely the desktop app's launcher) and
     fold the web surface into it as a sub-route or a separate
     `Core/GUI/web/` subfolder.
  2. Decide on the directory shape — flat (`src/`, `src-tauri/`,
     `web/`) vs nested (`desktop/`, `web/` with their own internal
     `src/`).
  3. Pull out anything that's "just a Tauri scaffold" (icons,
     `tsconfig.json`, `eslint.config.js`, `vitest.config.js`) and
     keep it; pull out anything legacy-specific (e.g. references to
     wylde-launcher) and clean those imports.
  4. Settle on one consolidated UI code path so a developer doesn't
     have to ask "is this in fletch-gui or fletch-web?".
- **Next step.** This is the largest design decision in the punch list.
  Sketch the target tree on paper first, then move files in one
  commit per top-level subfolder so you can bisect if the build
  breaks.

## 7. Device Gate — `Device Gate/_device_gate_merge/`

- **Source.** Phase 5C staging from legacy `core/device-gate/`. Has
  `device_gate.py`, `run.py`, `startup.py`, `consul_client.py`,
  `discovery.py`, `ipc.py`, `manifest.py`, `errors.py`,
  `data/htpasswd`, `start_device_gate.bat`.
- **Wired.** Nothing at `Device Gate/` top level yet.
- **Outstanding.**
  1. Promote `device_gate.py` to `Device Gate/device_gate.py`.
  2. Drop the Flask service shell and the `start_*.bat` (per the
     in-process pattern Phase 6 established).
  3. Decide where `data/htpasswd` lives — likely
     `Device Gate/data/htpasswd` with a documented "user must
     populate" note.
- **Next step.** Pair this with item 9 (Webcrawler) since both are
  small and follow the same in-process collapse pattern; one PR
  can cover both.

## 8. Device Gate / VPN — `Device Gate/VPN/_wylde_vpn/`

- **Source.** Phase 5C staging from legacy `core/wylde-vpn/`. Big
  surface area: gateway proxy + many `gateway/routes/` (chat,
  conversations, devices, images, link, models, push, rag, services,
  settings, system, tools, training, voice, workflows), NAT
  traversal (hole_puncher, stun_prober, endpoint_updater),
  STUN/TURN clients, mDNS advertiser, DDNS client, peer/push
  stores, mobile_proxy, monitoring (metrics_collector,
  tunnel_health), and a tools layer (`vpn_control.py`,
  `wylde_link.py`).
- **Wired.** Nothing at `Device Gate/VPN/` top level yet.
- **Outstanding.**
  1. Per the Wylde user's locked decision, **VPN is a sub-component, separately
     downloadable**. So the integration target is
     `Device Gate/VPN/<flat layout>` (no `_wylde_vpn/` prefix), not
     "fold into Device Gate proper".
  2. Decide the install/download story — does Wylde ship a
     `download_vpn.py` script that pulls binaries on demand? Where
     do native artifacts (WireGuard userspace binaries, etc.) go?
  3. The big `gateway/routes/` tree overlaps semantically with the
     Gateway item (#1) — figure out whether these routes are
     *Gateway routes that happen to be tunneled* or are
     *VPN-internal routes* and rename accordingly.
  4. Replace `entrypoint.sh` (Linux) with a Windows-friendly entry
     point if the rest of Wylde is Windows-first, or keep both and
     document.
- **Next step.** Resolve question 3 first — until you know whether
  these routes belong to the Gateway or the VPN, the rest of the
  cleanup is premature.

## 9. Webcrawler — `Extensions/Webcrawler/_webcrawler_service/`

- **Source.** Phase 5C staging from legacy `core/webcrawler-service/`.
  Has `scraper.py`, `extractor.py`, `webcrawler_api.py`, `config.py`,
  Flask service shell, plus `tools/fetch.py`, `tools/scrape.py`,
  `tools/extract.py`.
- **Wired.** Phase 7's extension `handler.py` imports from this
  staging folder — so the path is *live*, not just staged. The
  Extension Bridge dispatcher calls into here.
- **Outstanding.**
  1. Clean up the Flask service shell — delete `run.py`,
     `startup.py`, `consul_client.py`, `discovery.py`, `ipc.py`,
     `manifest.py`, `errors.py`, `tool_interface.py`,
     `start_webcrawler.bat`. The extension bridge is the only
     caller; it doesn't need a service.
  2. Keep only what `handler.py` actually uses — `scraper.py`,
     `extractor.py`, `tools/fetch.py`, `tools/scrape.py`,
     `tools/extract.py`, `config.py`. Verify by grep before deleting.
  3. Rename the folder from `_webcrawler_service/` to a
     normal name (e.g. `core/` or just promote files up to
     `Extensions/Webcrawler/`) — the `_` prefix signals "staging" and
     this code is no longer staging.
- **Next step.** `grep -r "_webcrawler_service"` to find all imports,
  rename folder, fix imports, drop the Flask shell, smoke-test via
  `phase7_smoke.bat`.

---

## Cross-cutting notes

- **Pattern to follow.** Phase 6's tool_registry/tool_runner refactor
  is the template: in-process module + filesystem-as-registry +
  no Flask shell + no service discovery layer. Apply it consistently.
- **What to keep around.** The `_*_legacy` folders the Wylde user flagged
  (`Core/Lifecycle/_legacy/wylde_launcher/`, `N8N/_legacy/`,
  `Core/harness/_legacy/orchestrator_api/`, plus everything in
  `Wylde/_legacy/` and `Wylde/default/`) are reference material —
  don't promote them, don't delete them.
- **Suggested order.** 9 → 7 → 1 (unblocks Phase 7 end-to-end story);
  then 2 (small); then 6 (large design decision); then 3, 4, 5
  (model-heavy services); then 8 (largest surface area, biggest
  design question).
