# Building the Wylde installer (gpui era)

This directory is documentation-only. It explains how to turn the
`wylde-gui` binary into a shippable, per-user Windows installer, and what
is left to do before the alpha can be handed to a non-developer.

> **History.** This README was ported at the slice-11 cutover
> (2026-05-29) from the deleted `Core/GUI/src-tauri/installer/README.md`.
> The Tauri bundler (`cargo tauri build`, `tauri.conf.json`,
> `nsis`/`msi` targets, the `tauri-plugin-updater`) died with the
> `src-tauri/` tree. The durable parts — per-user install location,
> Authenticode signing, the updater-keypair flow — carry forward below,
> retargeted at the gpui binary.

## What ships

The product is a single native binary, **`wylde-gui.exe`**, built out of
the standalone `Core/GUI/` gpui workspace:

```sh
# From Core/GUI/ (NOT rust/ — the gpui workspace is deliberately separate
# so its heavy graphics deps don't ripple into the backend lock file):
cargo build --release -p wylde-gui
# → Core/GUI/target/release/wylde-gui.exe
```

> **Do not run `cargo` from PowerShell on the Wylde user's dev box.** `cargo` +
> PowerShell has had file-lock issues in this repo's history. Use the
> Bash shell or the build tooling under `Core/harness/dev/`.

The binary is self-contained for the UI, but Wylde is a *mesh*: at
runtime `wylde-gui.exe` is launched by `launch_wylde.ps1` **after** the
Lifecycle daemon has brought the backend services up. The installer's
job is therefore to lay down the binary **plus** the Python/Rust service
trees the daemon supervises (see "Bundled resources" below).

## Install location (per-user, no admin)

The alpha installs **per-user, without elevation** — admin prompts are a
friction point the "average middle-aged person's PC" persona doesn't
need. Target:

```
%LOCALAPPDATA%\Programs\Wylde\
```

Everything (the `wylde-gui.exe`, the service trees, `docs/`) lands under
that root. Runtime state the OS-level Task Scheduler must touch goes to
`C:\ProgramData\Wylde\` instead — a freshly-created `%LOCALAPPDATA%`
subdir rejects writes from Scheduler-spawned processes on this box (a
known Windows quirk; see the launcher notes).

## Picking a bundler (TODO)

> **Resolved — the bundler is NSIS.** Implemented at
> `tools/installer/wylde-installer.nsi` + `tools/installer/build-installer.ps1`;
> the operational runbook is **[docs/installer.md](../../../docs/installer.md)**.
> The per-user location, daemon-first shortcut, `version.txt`, and updater
> interaction documented here all carried into that implementation. NSIS was
> picked over `cargo-packager` for direct control of the mesh resource layout
> (the staging step overlays built binaries onto a `git archive` of the tree).
> The notes below are kept for the alternatives considered and the still-open
> signing / endpoint work.

The candidates considered, in rough order of preference at the time:

* **[`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager)**
  — successor-in-spirit to the Tauri bundler; emits NSIS + MSI from a
  small `Packager.toml`, supports per-user NSIS (`installMode =
  "currentUser"`), resource bundling, and Authenticode `signCommand`.
  Closest migration path from the old Tauri config.
* **NSIS directly** — hand-written `.nsi` script. Maximum control, most
  work. Use only if `cargo-packager` can't express the resource layout.
* **WiX / MSI** — keep as a secondary target so corporate deployment
  paths (Intune, SCCM, Group Policy) work. MSI is per-machine by default
  in WiX; per-user MSI needs extra plumbing we add only on request.

Whichever is chosen, the bundler must: (1) place `wylde-gui.exe` +
service trees under the per-user root above, (2) create a Start-menu /
desktop shortcut that points at `launch_wylde.ps1` (so the daemon comes
up before the GUI), and (3) accept the Authenticode `signCommand` below.

## Code signing

The alpha installer is **unsigned**. SmartScreen will warn users until
an Authenticode signature is added.

### What to buy
* An Authenticode code-signing certificate from a CA Microsoft trusts
  (Sectigo, DigiCert, SSL.com, etc.). EV certs clear SmartScreen
  reputation dramatically faster but cost more.
* A hardware token (most CAs now require HSM-backed keys for new certs).

### Where signing slots in

Sign **both** the `wylde-gui.exe` binary and the produced installer.
With `signtool` on PATH (Windows SDK):

```sh
signtool sign /tr http://timestamp.digicert.com /td sha256 /fd sha256 /a ^
  "Core\GUI\target\release\wylde-gui.exe"
```

If using `cargo-packager`, set its `windows.sign-command` to the same
`signtool` invocation (`%1` is substituted with the artefact path) so the
installer is signed at bundle time. The certificate work is decoupled
from everything else here — the purchase can happen whenever.

## Self-update pipeline

The gpui binary updates itself via the
[`self_update`](https://crates.io/crates/self_update) +
[`self-replace`](https://crates.io/crates/self-replace) crates, already
declared in `Core/GUI/Cargo.toml`'s `[workspace.dependencies]` (the
Tauri updater plugin is gone). Neither is wired into the Settings panel
yet — that's a post-alpha item — but the release-side flow is unchanged
in spirit from the Tauri era and is documented here so the keypair work
can proceed independently.

### Signing keypair

Releases are signed with a long-lived keypair the updater verifies
against. Generate it once, locally:

* Keep the **private** key password-protected, off the repo, backed up to
  an offline medium. It never leaves the Wylde user's machine unencrypted.
* The **public** key is baked into the shipped binary as a constant (the
  updater verifies each downloaded release against it).

Rotating the key is a **flag day**: every already-installed client
verifies against the *old* public key baked into its binary, so a
keypair change requires a fresh, manually-distributed installer before
any subsequent self-update is accepted. Treat the key as long-lived.

### Update endpoint

The updater polls an HTTPS endpoint for a release manifest. Placeholder
host (from the Tauri era, reusable):

```
https://wyldebot.com/check/{target}/{arch}/{current_version}
```

**TODO: stand up this endpoint.** Any static host works (GitHub Pages,
S3 + CloudFront, an nginx behind WyldeLink). It returns a JSON manifest
naming the latest version, its download URL, and the release signature;
the client compares versions and upgrades if newer. Wylde is
Windows-only today, so a single `windows-x86_64` entry is all that
matters; other platforms can be added later without touching the client.

### Cutover steps (when cert, key, and endpoint are ready)

1. Generate the production keypair; bake the public key into the binary.
2. Stand up the endpoint and host an initial manifest for the current
   shipped version (so existing installs don't see a phantom update).
3. Wire the Settings panel's "Check for updates" to `self_update`.
4. Provision the signing key + Authenticode cert in the release
   environment.
5. Cut a fresh signed installer with the new public key baked in and
   distribute it manually one last time. Subsequent micro-fixes
   self-update.

## Bundled resources

The installer must lay down, alongside `wylde-gui.exe`, every tree the
Lifecycle daemon supervises or the GUI reads at runtime:

* Every Python service tree the supervisor launches: `Core/Lifecycle`,
  `Core/harness`, `Core/Memgraph`, `Core/Network`, `Core/Config`,
  `Core/resource_monitor`, `Core/shared`.
* The non-Core service trees: `device_gate`, `Extensions`, `Gateway`,
  `N8N`, `Trainer`, `Voice`, `VPN`.
* The Rust service binaries the daemon spawns in rust mode (built from
  `rust/`): `wylde-lifecycle`, `wylde-gateway`, `wylde-device-gate`,
  `wylde-vram-broker`, `wylde-ollama`, `wylde-harness`, etc.
* **`docs/`** — the first-run LLM bootstrap reads
  `docs/first-run-bootstrap.md` at runtime.
* `launch_wylde.ps1` — the shortcut target that boots the daemon then
  the GUI.

The canonical service list is the set of `manifest.json` files the
Lifecycle launcher discovers (see `Core/Lifecycle/discovery.py` and the
`wylde_check` rules `every_service_has_manifest` / `service_manifest_schema`).
If a new top-level service or runtime-readable doc tree is added, the
installer's resource list must be updated and a packaged build re-run to
verify it lands where the supervisor and harness expect it.

## Punch-list before "build and ship" works end-to-end

* [x] **Choose + wire a bundler** — done: NSIS at `tools/installer/`
      (`build-installer.ps1` + `wylde-installer.nsi`); runbook in
      `docs/installer.md`.
* [ ] **Buy + install an Authenticode cert**; add the `signCommand`.
* [ ] **Add `LICENSE`** at the repo root (Cargo manifests already declare
      `GPL-3.0-or-later`; the file itself is the missing piece) and
      reference it from the bundler config.
* [ ] **Wire self-update** into the Settings panel + provision the
      signing key.
* [ ] **Verify resource paths on a built install** — install into a
      tempdir and confirm `Core/Lifecycle/...`,
      `docs/first-run-bootstrap.md`, the service binaries, etc. all land
      where the supervisor and harness expect them.
* [ ] **Confirm tray + autostart survive a packaged build** — both work
      from a `cargo build` binary today; the packaged binary is the real
      test.
