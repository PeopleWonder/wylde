# Changelog

All notable changes to Wylde are recorded here. Versions follow
[SemVer](https://semver.org/); pre-1.0 alphas may break between builds.

## [Unreleased]

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

### Fixed

- **Short-term memory store now honours encryption-at-rest (OI-14).** It
  used plain file IO on the same conversation documents the conversations
  store reads/writes encrypted; a lazy-migration read could flip a document
  to ciphertext mid-flow, after which the short-term store's plain reads
  saw an unreadable file and silently minted a stub over live data (losing
  the workspace binding and the working-memory list). Both stores now route
  through the same `wylde_shared::encryption` read/write path.

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
