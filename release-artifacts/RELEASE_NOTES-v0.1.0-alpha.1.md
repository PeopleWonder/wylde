Wylde **v0.1.0-alpha.1** — the first tagged alpha. Pre-release/beta channel.

> Archived 2026-07-26. These are the release notes for the `v0.1.0-alpha.1`
> tag (2026-06-04). They lived as an untracked file in the (gitignored)
> `release-artifacts/` staging dir; they are preserved here, tracked, now that
> `RELEASE_NOTES.md` is a versioned file. The current release's notes are in
> `RELEASE_NOTES.md`.

This is an early build for testing. Expect rough edges; the privacy-first
defaults mean nothing phones home unless you turn it on.

## Headline changes

- **gpui-native rewrite (shipped).** The entire desktop UI was rebuilt on
  gpui, replacing the old Tauri + Svelte alpha. All panels — Chat, Models,
  Memory, Dashboard, Devices, Workspaces, Tools, Settings, Images,
  RemoteAccess — are wired through the in-process harness.
- **Voice cutover.** The Python voice stack was retired and STT/TTS now run
  in-process (ONNX Whisper + Kokoro). Settings gains a Voice section
  (input-device pick, mic test) and a live push-to-talk hotkey.
- **In-app self-updater.** Opt-in updates pulled from this repo's GitHub
  Releases, verified against one embedded minisign/Ed25519 key (fail-closed —
  an unsigned or mis-signed binary is never installed). Stable / Beta channels,
  a manual "Check now", and an optional background check on your cadence. No
  telemetry, no identity sent.
- **Per-user installer.** A no-UAC NSIS installer (`WyldeSetup`) that installs
  to `%LOCALAPPDATA%\Programs\Wylde`, with Start-menu / desktop shortcuts and
  optional sign-in autostart, all daemon-first.
- **Conversation switching.** Per-conversation memory with a conversation
  switcher and a cross-panel nav-bus, so the Memory panel mirrors the active
  chat's working-memory buffer.

## Assets

- `wylde-gui-x86_64-pc-windows-msvc.exe` (+ `.minisig`) — the bare signed GUI
  binary the in-app self-updater consumes.
- `WyldeSetup-0.1.0-alpha.1.exe` (+ `.minisig`) — the per-user installer for a
  first-time install.

Both assets are signed with the project's production minisign key
(ID `DA7E13F4E9F2ACB6`).
