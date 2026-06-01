# Phase 5 — Open Questions

Pass B services: legacy services from `_legacy/core/` whose new home is unclear. **No moves were performed for any service in this file.** Each entry below is a placement question for the Wylde user to answer; Phase 5 will resume once decisions are recorded.

For each: a one-liner of what the service does, why placement is ambiguous, and 2–3 placement options with trade-offs. Pick one (or a different one) for each.

---

## 1. `_legacy/core/wylde-launcher/`

**What it does:** Service lifecycle manager — starts, stops, and lazy-wakes Wylde services. Pipe `\\.\pipe\wylde-launcher` (port 8012). Per its README, this is the runtime that decides which sidecar processes are alive.

**Why ambiguous:** `Core/Lifecycle/launcher.py` already supersedes it in the new architecture, so the *function* has been ported. Question is what to do with the old service code.

**Options:**
- **A. `Core/Lifecycle/_legacy/wylde_launcher/`** — keeps the old impl next to the new one for diff/reference until lifecycle.py reaches feature parity, then delete.
- **B. `N8N/_legacy/wylde-launcher/`** — treat as already-dead code; bury alongside other dissolved-into-N8N services.
- **C. Don't migrate at all** — leave it in `_legacy/core/` as part of the historical archive; remove from the migration entirely.

**Trade-off:** A is most useful if Lifecycle/ is still being filled in (preserves something to crib from). B is correct if Lifecycle is already complete. C is correct if `_legacy/core/` is itself the long-term archive.

---

## 2. `_legacy/core/wylde-voice-assistant/`

**What it does:** Wake-word detection, STT, and TTS voice control with NPU acceleration; runs as a tray app with its own pipe `\\.\pipe\wylde-voice-assistant`. Per its README, all model calls are routed through `wylde-orchestrator` rather than direct Ollama — the assistant is a *frontend* that consumes the voice + harness services.

**Why ambiguous:** `wylde-voice/` (already migrated to `Voice/_wylde_voice/`) is the audio engine (STT/TTS pipeline + voice_api). `wylde-voice-assistant/` is the higher-level wake-word + intent + tray layer that *uses* voice. So is it a sub-component of Voice/, or the actual top-level Voice product with the engine being a sub-module?

**Options:**
- **A. `Voice/Assistant/_wylde_voice_assistant/`** — sub-component framing: Voice/ has an Engine/ (current `_wylde_voice/`) and an Assistant/ (this). Clean two-layer split.
- **B. `Voice/_wylde_voice_assistant/` (sibling of `_wylde_voice/`)** — defer the layering; both legacy folders sit at Voice/ root and the integrator decides later.
- **C. `Wylde/VoiceAssistant/` as own top-level** — treat the tray app as a distinct product (assistant ≠ voice infrastructure), parallel to GUI.

**Trade-off:** A is the architecturally clean answer if the assistant always depends on the engine. B is safest if the boundary is fuzzy. C makes sense if the assistant ships independently or has its own roadmap.

---

## 3. `_legacy/core/fletch-web/`

**What it does:** Standalone Python launcher that serves the Vite-built `fletch-gui` bundle and reverse-proxies upstream services so the SPA can talk to local pipes without CORS. Effectively the "web variant" of fletch-gui — same UI, different transport.

**Why ambiguous:** Tightly coupled to `fletch-gui` (serves its build output), but architecturally a separate process. Unclear whether the new tree wants one unified GUI module with a `Web/` sub-area, or two siblings.

**Options:**
- **A. `Core/GUI/Web/_fletch_web/`** — sub of GUI: GUI/ owns both the desktop (Tauri) and web variants of the same UI.
- **B. `Core/Web/_fletch_web/`** — sibling of GUI/ at Core/ level: web is a distinct delivery channel.
- **C. Fold both fletch-gui and fletch-web into a single `Core/GUI/`** — collapse the bundle/serve distinction; treat them as one product with two entry points after integration.

**Trade-off:** A is cleanest if web is always paired with the desktop UI (one team, one repo). B if the web variant might grow separately (auth, multi-user, remote access). C is the eventual end-state but premature if integration work is still pending.

---

## 4. `_legacy/core/wylde-vpn/`

**What it does:** WyldeLink — peer-to-peer VPN with WireGuard tunnels and TURN relay for remote access. Pipe `\\.\pipe\wylde-vpn` (port 8020).

**Why ambiguous:** Networking is a foundational concern, but the rest of the new architecture doesn't yet have a networking module. Unclear whether VPN/mesh is part of the supported product going forward, or whether it's been abandoned.

**Options:**
- **A. `Wylde/VPN/_wylde_vpn/` as a new top-level** — survives as a first-class module; matches Gateway/Voice/N8N pattern.
- **B. `Wylde/Gateway/_wylde_vpn/`** — fold into Gateway as the "remote access" path (Gateway already handles auth/proxying; VPN is just another transport).
- **C. `N8N/_legacy/wylde-vpn/`** — treat as deprecated; mesh networking isn't part of the post-Phase-4 architecture.

**Trade-off:** A keeps the option open. B argues VPN is a Gateway-shaped problem and avoids a new top-level. C is correct only if you've decided not to ship remote access at all.

---

## 5. `_legacy/core/wylde-sysmon/`

**What it does:** System resource monitor for GPU, CPU, RAM, and process health, plus the VRAM broker. Pipe `\\.\pipe\wylde-sysmon` (port 9100). Note: `vram_broker.py` already lives in `_legacy/core/shared/` (now `Core/shared/`), so the broker logic is partly in shared/ and partly here.

**Why ambiguous:** "Resource Monitor" already exists as a concept in the vault. Sysmon is the runtime daemon that *publishes* the resource data the monitor reads — but the boundary isn't drawn yet.

**Options:**
- **A. `Core/Resource Monitor/_wylde_sysmon/`** — folds the daemon into the existing Resource Monitor area; one place for all GPU/CPU/RAM concerns.
- **B. `Wylde/SysMon/_wylde_sysmon/` as new top-level** — keeps the runtime daemon distinct from any UI/visualization in Core/Resource Monitor/.
- **C. Split — daemon to `Core/Resource Monitor/`, VRAM broker bits stay in `Core/shared/vram_broker.py`** — accept the existing split and just document it.

**Trade-off:** A is the simplest mental model. B is right if there will be a meaningful "monitor consumer" / "monitor producer" split that wants different lifecycles. C just acknowledges the existing layout — minimal disruption.

---

## 6. `_legacy/core/device-gate/`

**What it does:** Device approval gate — maintains pending/approved device lists and re-authenticates new IPs. Pipe `\\.\pipe\wylde-device-gate` (port 7000). It's an authentication/access-control layer that gates who can talk to the rest of the stack.

**Why ambiguous:** Strong overlap with Gateway/ (also auth-shaped). Device gate is *device-level* (per-machine approval); Gateway is presumably more application-level (per-request auth, routing). Two different layers of the same problem domain.

**Options:**
- **A. `Wylde/Gateway/_device_gate_merge/`** — fold into Gateway; device-level approval is the outer ring of the same auth story.
- **B. `Wylde/DeviceGate/_device_gate/` as own top-level** — keeps the perimeter (device approval) physically separate from in-stack auth (Gateway).
- **C. `Wylde/Gateway/Devices/_device_gate/`** — sub-area of Gateway, keeps the merge but also keeps the lineage explicit.

**Trade-off:** A collapses cleanly but loses the perimeter-vs-in-stack distinction. B is over-structured if there's only ever one device-gate process. C is the middle path.

---

## 7. `_legacy/core/webcrawler-service/`

**What it does:** Web crawler and scraper with headless-browser support, exposed as a Wylde tool. Pipe `\\.\pipe\wylde-webcrawler-service` (port 8003). Two halves: a long-running service, plus tool definitions consumed by the harness.

**Why ambiguous:** The tool definitions clearly belong in `Core/harness/tooling/tools/web/`. The question is the *service* half: does the architecture still want a dedicated crawler daemon, does the crawl get folded into an N8N workflow, or does the service disappear and tools become in-process?

**Options:**
- **A. Tools → `Core/harness/tooling/tools/web/`; service → `Core/harness/tooling/services/_webcrawler/`** — split the two halves; the service stays alive but co-located with its tool surface.
- **B. Tools → `Core/harness/tooling/tools/web/`; service dies, tools become in-process (no daemon)** — simplest; fits if crawls are short-lived and don't need their own process.
- **C. Tools → `Core/harness/tooling/tools/web/`; service → N8N workflow** — convert crawl orchestration into an N8N workflow rather than a standalone daemon.

**Trade-off:** A preserves the existing topology with minimal rework. B is the cleanest if you can drop the daemon (memory-resident crawler isn't always needed). C aligns with the N8N-replaces-orchestrator direction but is the most work.

---

## 8. `_legacy/core/wylde-orchestrator/` — loose root files

**What it lives as:** The top-level Python files at the orchestrator root (`harness_api.py`, `orchestrator_api.py`, `run.py`, `models/`, `startup.py`, `tools/*.py`, etc.) — *excluding* the sub-packages (planner/, optimizer/, autotuner/, gates/, guards/, graph/, etc.) that have already been absorbed into `N8N/_legacy/` in Phases 4a/4b/4c.

**Why ambiguous:** The orchestrator was the old central nervous system. Most of it has dissolved (graph engine, planner, gates → N8N). What remains at the root is the HTTP/pipe API surface (`harness_api.py`, `orchestrator_api.py`, the runner, the model registry). The question is: are any of these root files still part of the new architecture, or has the entire orchestrator process been replaced by something else (Lifecycle + N8N + Gateway combined)?

Per the README, the harness API is pipe-only and is the *only* surface the GUI talks to for chat — that's a meaningful piece of API contract. `harness_api.py` is a non-trivial implementation that may or may not have a successor in the new tree.

**Options:**
- **A. `N8N/_legacy/wylde-orchestrator/` (whole orchestrator root)** — bury everything left at the root alongside the already-migrated sub-packages. Treat the orchestrator process as fully replaced.
- **B. `Core/harness/_legacy/orchestrator_api/` (just the API surface files: `harness_api.py`, `orchestrator_api.py`, `run.py`, `startup.py`)** — preserve the API contract files in Core/harness/ for reference while the new harness API takes shape; rest goes to N8N/_legacy/.
- **C. Inventory and split per-file** — `models/` may belong to a model-registry module; `tools/*.py` are small CLI shims that may already have replacements; `harness_api.py` should be diffed against the new harness wiring before deciding. Don't bulk-move; classify file-by-file.

**Trade-off:** A is the fastest and assumes the new architecture has all of this covered. B preserves the most-likely-needed pieces (the API contract) without keeping the whole thing. C is the most thorough but the most expensive — appropriate if uncertainty is high.

---

## How to record decisions

For each numbered question above, append your choice (e.g. "1. B", "2. A", "3. C") plus any caveats. Once all 8 are decided, Phase 5 can resume with a second batch of robocopies for these services.
