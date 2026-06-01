# Wylde — Open Items After Refactor

The structural refactor is complete (Phases 1–9, 64 tests green, 16 principles locked). This doc captures everything left to look into. Grouped by priority, then by category.

---

## 🔴 Functional gaps (the app isn't 100% usable until these land)

### F1. Conversations service — LOCKED: subfolder of harness/memory/

**Status:** Pending implementation.
**Decision (locked):** Chat history lives at `Core/harness/memory/conversations/` as a subfolder of memory. NOT its own service.

**What needs to happen:**
- Create `Core/harness/memory/conversations/` with the storage layer (probably JSON-on-disk like the existing `_common.DATA_DIR` pattern)
- Public API: `save_turn(conversation_id, user_msg, assistant_msg)`, `get_conversation(id)`, `list_conversations(limit)`, `delete_conversation(id)`
- Wire it as the backing store for chat history once a chat surface needs it

**Priority:** Medium — needed when chat features land but not blocking structural work.

### F1b. N8N UI embedded in Wylde GUI — NEW

**Status:** Stub being added in current GUI cleanup task.
**What:** the Wylde user wants N8N's actual web UI embedded inside Wylde — user clicks a "Workflows" tab and sees N8N rendered inline. Wylde's GUI acts as a middleman.

**Approach:** webview/iframe pointing at `WYLDE_N8N_ENDPOINT` (default http://127.0.0.1:5678). Tauri uses nested webview, web variant uses iframe.

**Current task (GUI cleanup) adds:**
- Stub `Workflows.svelte` that renders `<iframe src={N8N_URL}>`
- Nav entry for the Workflows tab
- "N8N not connected" placeholder when service is down

**Future work (when SSO is needed):**
- Auth passthrough so users don't have to log into N8N separately
- Maybe a thin Wylde wrapper around N8N's API for triggering workflows from outside the embedded UI

**Priority:** Low — basic embed works; SSO is polish.

---

### F2. GUI references stale `security-api` pipe name

**Status:** Found in 3 files during the Gateway cleanup grep.

```
Core/GUI/src-tauri/src/lib.rs:72       pipe::call("security-api", "GET", "/api/services", ...)
Core/GUI/web/fletch_web.py:76          "registry": "security-api"
Core/GUI/src/lib/api.js:19             const SVC_REGISTRY = 'security-api'
```

These will fail the moment the GUI is exercised against the new Gateway. The Gateway pipe is now `wylde-gateway`.

**To resolve:**
- Update each file to use `wylde-gateway`
- Verify the actions called still match (the Gateway exposes egress.forward, egress.kill_switch, extensions.dispatch, tools.list, tools.get — the GUI's calls need to map to these or new ones added)

**Priority:** High — needed before the GUI works at all.

**Spawning:** Yes, will spawn a small task for this.

---

### F3. Sibling services that Gateway routes assume exist

**Status:** Routes load, but are dark until these come up.

The 13 Gateway routes added in #9 assume sibling services running on the system:
- `services.py` → `wylde-launcher`
- `system.py` → `wylde-sysmon`
- `training.py` → `wylde-trainer`
- `workflows.py` → `wylde-orchestrator` (DEAD — orchestrator is gone) and `n8n` (HTTP)
- `voice.py` → voice-assistant pipe
- `rag.py` → wylde-rag (already in harness/memory/, not a separate service)

**To resolve, per route:**
- `wylde-launcher` → replace with `Core/Lifecycle/launcher.py` calls (already in-process)
- `wylde-sysmon` → fold into `Core/Resource Monitor/` (already done in Phase 5C, just needs route to point at it)
- `wylde-trainer` → wires when Trainer service is built out
- `wylde-orchestrator` → strip from workflows route entirely (orchestrator is dead)
- `n8n` → already exists, point at it
- `voice` → wires when VoiceAssistant integration is finalized
- `rag` → repoint to `harness/memory/rag.py` direct calls

**Priority:** Medium — depends which features you actually use first.

---

## 🟡 Cleanup (operational, not functional)

### C1. Vault-root staging-delete .bats

**Status:** Created during refactor, not yet run.

Files:
- `_phase8_7_delete_caption_staging.bat` (deletes the 24 GB Caption staging — `Trainer/Caption/_wylde_caption/`)
- `_phase8_8_delete_security_api_merge.bat` (deletes `Gateway/_security_api_merge/`)
- `_phase9_delete_wylde_vpn_staging.bat` (deletes `Device Gate/VPN/_wylde_vpn/`)

**To resolve:** Run each via File Explorer double-click. Then delete the .bat files themselves.

**Priority:** Low — staging folders aren't hurting anything except disk space.

**Spawning:** Yes, will spawn a task to run all three.

---

### C2. Empty `_fletch_*/` dirs in `Core/GUI/`

**Status:** Hoisted but the empty parent dirs couldn't be deleted by the agent (permission denied on this VM).

Files:
- `Core/GUI/_fletch_gui/` (empty)
- `Core/GUI/_fletch_web/` (empty)

**To resolve:** `rmdir` from PowerShell or File Explorer.

**Priority:** Low — cosmetic.

**Spawning:** Yes, will fold into the staging cleanup task.

---

### C3. `Wylde/_legacy/` 114 GB production backup

**Status:** Untouched per the Wylde user's locked instruction ("I'll do that myself").

**To resolve:** Delete when confident the new tree is fully working. 114 GB freed.

**Priority:** Low — your call.

---

## 🟢 Inconsistencies (work, but won't break anything)

### I1. VPN api.py uses Flask, Gateway uses FastAPI

**Status:** Functional inconsistency.

`Wylde/Device Gate/VPN/api.py` is still Flask + named-pipe IPC. Gateway is FastAPI. The VPN's HTTP surface is loopback-only (port 8020) and only Gateway proxies hit it.

**To resolve:** Migrate VPN to FastAPI eventually for consistency. Not urgent — works fine via the pipe.

**Priority:** Very low — consistency cleanup.

---

### I2. `Core/installer/startup_windows.py` exists but uncalled

**Status:** Generic launcher hook, no callers yet.

**To resolve:** Wire into `Core/Lifecycle/launcher.py` so services that need a Windows-startup hook can use the generic one rather than each having their own.

**Priority:** Low — wires in when first service needs it.

**Spawning:** No, defer until a real service needs it.

---

## 🔵 System hardware diagnostic

### H1. Intel PPM Provisioning Package error

**Status:** Discovered during the LED crash investigation. The most plausible suspect for the 25 dirty shutdowns since 4/13.

**What:** Intel(R) PPM Provisioning Package shows status=Error in Device Manager. This is the CPU power-management firmware (per-SKU power/voltage/C-state tables). A BIOS change that shifts the PCIe window can leave PPM in a bad state. PPM errors are a textbook fit for "BugcheckCode=0 hard hang" since hard hangs without bugchecks are classic C-state / power-rail problems.

**To resolve:**
1. Identify motherboard model + CPU + current chipset driver version
2. Locate the matching Intel chipset / PPM package from the motherboard vendor's website (NOT the BIOS-bundled version)
3. Download + install
4. Reboot, monitor for 24-48h to see if KP41 events stop

**Priority:** HIGH — this is what's been crashing the system.

**Spawning:** Yes, will spawn a diagnostic task that identifies the system and finds the right driver. Driver install requires user interaction (UAC + EULA).

---

## 🟣 Nice-to-haves surfaced during refactor

### N1. `forward_sync` worker pool tuning

**Status:** Refactored to long-lived event loop (one daemon thread, single asyncio loop). Works fine.

**Future:** If pipe egress traffic gets heavy, the obvious upgrade is a worker pool of multiple loops or a dedicated executor. Not pressing.

---

### N2. MCP server slot

**Status:** Reserved at `Gateway/routes/mcp.py` (slot exists, not implemented).

**Future:** When you want to expose Wylde as an MCP server (so external Claude clients can call into it), drop the implementation here.

---

### N3. Anomaly detection for audit logs

**Status:** Reserved at `Gateway/audit/anomaly.py` (slot exists, not implemented).

**Future:** Background task that consumes `logs/egress.jsonl` + `logs/gateway.jsonl` looking for auth-spike / rate / endpoint-scan patterns. Nice-to-have, not security-critical.

---

### N4. API key minting CLI

**Status:** Moot — API key tier was dropped entirely per principle #16. VPN tunnel handles peer auth at connect time; no per-request keys.

**Future:** If you ever want a user-facing API key for some reason (e.g. CLI from a non-VPN machine), add `auth/api_keys.py` back and a `mint_key.py` CLI. Don't unless there's a real need.

---

## 🟠 Refactor cleanup that came up but wasn't critical

### R1. `Core/GUI/src-tauri/tauri.conf.json` references stale `core/wylde-*` paths

**Status:** Pre-existing breakage from the refactor, flagged during #5 GUI merge.

41 bundle-resource entries reference lowercase `core/`, `tools/`, `scripts/install/` paths that don't exist in the current repo. The next `tauri build` will fail when copying resources.

**To resolve:** Audit each entry; either remove (if the resource isn't needed) or repoint at the new location (`Wylde/Core/...`).

**Priority:** Medium — blocks production builds. Not blocking dev iteration.

---

### R2. Docs in `Core/GUI/docs/` reference fletch-* names

**Status:** 3 hits across 2 files (`inference-bar-audit.md`, `inference-bar-migration-plan.md`).

**To resolve:** Find/replace fletch-gui, fletch-web → GUI.

**Priority:** Low — internal docs.

---

### R3. `Core/GUI/web/` vendored copies of `Core/shared/` modules

**Status:** Files like `ipc.py`, `errors.py`, etc. exist locally in `Core/GUI/web/` AND canonically in `Core/shared/`. Sibling-relative imports may shadow the canonical ones.

**To resolve:** Audit + decide: keep web's local copies (and ignore Core/shared) or delete them and import from Core/shared.

**Priority:** Low — works fine until imports collide.

---

## Summary: what to spawn

I'll spawn these now:

1. **F2 — GUI security-api → wylde-gateway rename** (3 file edits)
2. **C1 + C2 — Vault-root staging cleanup** (run the 3 .bats + rmdir the empty fletch dirs)
3. **H1 — Intel PPM diagnostic + driver locate** (identify system, find correct driver, surface install steps for the Wylde user)

Holding off on:

- **F1 Conversations** — needs design discussion before spawning
- **F3 Sibling routes** — needs to be done per-feature as you use them
- **I1 / I2 / R1 / R2 / R3** — low priority, defer

The structural refactor is done. The list above is what's left between "done" and "fully shipped."
