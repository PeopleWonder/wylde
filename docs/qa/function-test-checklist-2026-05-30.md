# Wylde Function Test — Hands-On Checklist (2026-05-30)

Run **after** `tools\preflight-function-test.ps1` comes back with no red rows and the
GUI is launched (Desktop `Wylde` shortcut → `wylde-gui.exe`).

Each item: tick the box once it behaves as described. The *"broken if"* note is the
fast tell that something regressed.

> Scope: exercises every shipped GPUI panel (Chat, Dashboard, Devices, Images,
> Memory, Models, RemoteAccess, Settings, Tools, Training, Workspaces) plus the
> tray and the cold-start consent reconnect. No live training job or VPN peer needed.

---

## 0. Launch / first paint

- [ ] **Window opens** — `wylde-gui.exe` paints the shell (sidebar + content slot) within a few seconds.
  *Broken if:* blank window, immediate crash, or it can't find its icon/assets (means WorkingDirectory is wrong — re-run the shortcut script).
- [ ] **Sidebar lists all panels** — Chat, Dashboard, Devices, Images, Memory, Models, RemoteAccess, Settings, Tools, Training, Workspaces.
  *Broken if:* a panel is missing → its manifest didn't register (panel-registry aggregator).
- [ ] **No "harness unreachable" banner on a healthy stack.**
  *Broken if:* banner persists → `\\.\pipe\wylde-harness` isn't being served (check preflight pipe row).

---

## 1. Settings

Verify each control toggles **and persists** (the Settings-writes slice wired these to real verbs).

- [ ] **Consent toggles (`consent.*`)** flip and show the new state immediately.
  *Broken if:* a toggle snaps back → write verb failed or is non-interactive.
- [ ] **Autostart toggle** flips.
  *Broken if:* no effect / error toast.
- [ ] **Updater settings (`updater.*`)** — change a channel/cadence control.
  *Broken if:* control is inert (the pre-QA "Settings toggles non-interactive" bug — should be fixed).
- [ ] **Persistence check:** change ≥2 settings → **Quit via tray** → relaunch → reopen Settings.
  *Broken if:* values reverted → writes aren't durable (not hitting consent/updater store).

---

## 2. Chat — InferenceBar

- [ ] **Send a short message** → assistant reply **streams token-by-token**.
  *Broken if:* reply arrives all at once, never arrives, or tokens bleed into the wrong bubble.
- [ ] **Stop mid-stream** — hit Stop while tokens are flowing → stream halts promptly, Stop returns to Send.
  *Broken if:* tokens keep coming after Stop (dead `active_stream`), or the button state desyncs.
- [ ] **Send again after Stop** — next turn works normally.
  *Broken if:* double-turn race (the rapid-fire bug) — two replies, or the bar locks up.
- [ ] **10k-char paste** — paste the fixture below into the InferenceBar.
  *Broken if:* UI wraps badly, freezes, loses focus, or won't resize. Should wrap, stay responsive, keep caret focus.
- [ ] **Enter sends; Shift+Enter newlines** (Enter-chord).
  *Broken if:* Enter inserts a newline or Shift+Enter sends.
- [ ] **Model dropdown** opens, lists models, selects one, closes.
  *Broken if:* empty list (ollama unreachable — see preflight) or won't close.
- [ ] **Workspace dropdown** opens, lists workspaces, closes.
  *Broken if:* empty or stale.
- [ ] **Tool-call consent** — ask something that triggers a tool (e.g. "run a quick bash echo" / "what files are here"). An inline consent card appears; **Approve** → tool runs and result streams back; try **Deny** on a second call → tool is refused gracefully.
  *Broken if:* tool runs with no consent card, or Approve/Deny does nothing.

<details><summary>10k-char paste fixture (expand, copy the whole block)</summary>

```
WYLDE-PASTE-FIXTURE Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Repeat this paragraph until you have roughly ten thousand characters of body text. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.
```
*(If you need exactly ~10k chars, paste the line above ~18×, or generate with PowerShell:
`Set-Clipboard ("WYLDE-PASTE-FIXTURE " + ("x" * 10000))` and paste that.)*
</details>

---

## 3. Memory

- [ ] **Open the Memory panel** — current workspace's records render.
  *Broken if:* empty when records exist, or spinner that never resolves.
- [ ] **Live search** — type a query; results filter after the ~300 ms debounce.
  *Broken if:* no filtering, or a request fires on every keystroke (debounce broken).
- [ ] **Clear the search** — full list returns.
  *Broken if:* list stays filtered.

---

## 4. Workspaces

- [ ] **See the workspace list** with the active one marked.
- [ ] **Switch the active workspace** → selection updates; Chat/Memory now scope to the new workspace.
  *Broken if:* switch has no effect, or Chat still uses the old workspace.

---

## 5. Tools

- [ ] **Tool list renders** (built-in tools — fs, code, search, etc.).
  *Broken if:* empty list → tool registry/manifest aggregation failed.
- [ ] **Run a built-in tool** from the panel (or trigger one via Chat per §2).
  *Broken if:* invocation errors out or hangs with no result.

---

## 6. Models

- [ ] **Model list renders**; the default-model **star** is shown on one model.
- [ ] **Set a different model as default** (the `models.set_default` verb) → star moves.
- [ ] **Persistence:** Quit via tray → relaunch → open Models.
  *Broken if:* star reverted → `models.set_default` / `models.get_default` not persisting.

---

## 7. Devices

- [ ] **Paired devices list renders** (from device_gate).
  *Broken if:* empty when devices are paired, or "device-gate unreachable".
- [ ] **Per-device "Recent activity" strip** populates for a device with history (`device_gate.recent_actions`).
  *Broken if:* strip is blank for a device that has acted, or shows another device's actions.

---

## 8. Training

- [ ] **Panel renders cleanly** — active/past sections show (empty state is fine; no live job needed).
  *Broken if:* panel errors, blank-screens, or can't reach `\\.\pipe\wylde-trainer`.

---

## 9. Images

- [ ] **Panel renders** — gallery + filters show (empty gallery is fine).
  *Broken if:* panel errors or can't reach the Gateway `/api/images/*` routes.

---

## 10. Dashboard / RemoteAccess (sanity)

- [ ] **Dashboard** renders service tiles with live status colors.
  *Broken if:* all tiles grey/dead while services are up.
- [ ] **RemoteAccess** panel renders (QR / pairing surface).
  *Broken if:* errors on open. (AdGuard rewrite sync is known-unwired — not a regression.)

---

## 11. Tray → graceful shutdown

- [ ] **Right-click the Wylde tray icon** → context menu appears with **Quit**.
- [ ] **Quit** → window closes **and** the service mesh drains in `shutdown_order`
  (GUI=10 → Gateway=20 → Voice=30 → infra). After quit, re-run
  `tools\preflight-function-test.ps1` — the **pipe section should be clean** (no
  lingering `\\.\pipe\wylde-*`).
  *Broken if:* Quit just hides the window, or pipes/processes survive (orphan — the
  reaper didn't run).

---

## 12. Cold-start consent reconnect

Validates the Cleanup-C polish fix (reconnect-with-backoff catches a pending consent
raised before the GUI's harness connection is live).

- [ ] **Kill the harness** out from under a running GUI (Task Manager → end
  `wylde-harness.exe`, or stop it however the stack does).
- [ ] **Relaunch the harness** (or let the lifecycle daemon respawn it).
- [ ] **Trigger a tool call from Chat** that requires consent.
  *Broken if:* the consent card never appears / the turn hangs → the reconnect
  backoff isn't catching the pending consent.
  *Working:* GUI reconnects after a short backoff and the consent card surfaces; Approve runs the tool.

---

### Sign-off

- [ ] All panels render. No red preflight rows.
- [ ] Settings + Models default persist across a quit/relaunch.
- [ ] Stop / re-send / paste behave in Chat.
- [ ] Tray Quit drains cleanly (pipes clear).
- [ ] Cold-start consent reconnect works.

Notes / regressions found:

```
(record anything that tripped a "broken if" here)
```
