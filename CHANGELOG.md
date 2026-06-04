# Changelog

All notable changes to Wylde are recorded here. Versions follow
[SemVer](https://semver.org/); pre-1.0 alphas may break between builds.

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
