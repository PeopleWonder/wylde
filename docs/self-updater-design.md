# Wylde self-updater — design (Phase 12.5)

Status: **landed** (slice 1 — `wylde-updater` crate + Settings wire-up + key
procedure). Background-on-boot check and the convenience signing CLI are
explicit follow-ups, see [Deferred](#deferred-to-follow-up-slices).

This is the load-bearing decision doc for Wylde's in-app self-updater. It
records *why* the moving parts are shaped the way they are so a future change
doesn't quietly undo a property we cared about (privacy, fail-closed
verification, single shared key).

---

## Goals & non-goals

**Goals**

- Let a user on the shipped single-binary build pull a newer `wylde-gui.exe`
  from the project's GitHub Releases, **only after they opt in**.
- Cryptographically verify every downloaded binary against **one shared public
  key** baked into the updater before it is ever written over the running exe.
- Offer a **Stable** and a **Beta** channel, where Beta additionally surfaces
  GitHub *pre-releases*.
- A manual **"Check now"** path (Settings) and a background check on the user's
  chosen cadence.

**Non-goals (this slice and by design)**

- **No per-user license.** Wylde is GPL-3.0; everyone runs the same bits. The
  updater never sends an identity, a token, or a machine fingerprint. The only
  outbound call is an unauthenticated `GET` to the public GitHub REST API.
- **No installer / MSI wrapping.** Single-binary swap only. Wrapping the
  updated binary in a WiX/MSI installer is a separate post-alpha slice.
- **No silent auto-install.** We always prompt before replacing the binary.
- **No telemetry.** A check is a single REST call and nothing else.

---

## Update source — GitHub Releases

Source repo: **`PeopleWonder/wylde`** (public, GPL-3.0).

We query the public REST endpoint (no auth, no token):

```
GET https://api.github.com/repos/PeopleWonder/wylde/releases?per_page=30
Accept: application/vnd.github+json
User-Agent: wylde-updater/<version>     # GitHub requires a UA
```

The response is an array of releases, newest first. Each carries the fields we
care about:

| field                 | use                                                        |
| --------------------- | ---------------------------------------------------------- |
| `tag_name`            | the version, e.g. `v0.2.0` or `v0.2.0-beta.1`              |
| `draft`               | drafts are **always** ignored (never an update candidate)  |
| `prerelease`          | Stable ignores these; Beta includes them                   |
| `assets[]`            | `{ name, browser_download_url, size }` — the binary + sig  |
| `body`                | release notes (markdown), surfaced in the prompt           |
| `html_url`            | "view release" link                                        |

We deliberately do **not** use the `/releases/latest` endpoint: it only ever
returns the latest *stable* full release, which can't serve the Beta channel,
and it hides the asset list shape we need for both channels. One list call
covers both channels and keeps the logic in one place.

### Asset naming convention

Each release that the updater can consume must publish, at minimum:

```
wylde-gui-<target>.exe            # the binary, e.g. wylde-gui-x86_64-pc-windows-msvc.exe
wylde-gui-<target>.exe.minisig    # its detached minisign signature
```

The updater picks the first asset whose name matches the running platform's
binary pattern and is **not** itself a `.minisig`, then looks for a sibling
asset named `<that-asset>.minisig`. A release missing the `.minisig` sibling is
treated as *not updatable* (fail closed) rather than installed unsigned.

---

## Channel selection

`Channel` is a two-variant enum: `Stable | Beta`, persisted in the user's
update preferences (see [Persistence](#persistence)).

Selection is a pure function over the parsed release list (`select_release`),
unit-tested independently of the network:

1. Drop every `draft` release.
2. **Stable:** keep only releases where `prerelease == false`.
   **Beta:** keep all (stable + pre-release).
3. Parse each surviving `tag_name` as semver (leading `v` stripped). Releases
   whose tag isn't valid semver are skipped with a warning.
4. Pick the **highest** semver. semver prerelease ordering means `0.2.0` >
   `0.2.0-beta.1`, so a Beta user who is on a pre-release correctly sees the
   final stable release as an upgrade once it ships.

An update is *available* iff the selected release's version is **strictly
greater** than the running binary's version (also parsed as semver). Equal or
older ⇒ "you're up to date".

> Beta is a superset of Stable. A user flipping Stable → Beta may immediately be
> offered a newer pre-release; flipping back to Stable will not *downgrade* them
> (we never offer an older version), it just stops offering pre-releases going
> forward.

---

## Signature verification — minisign / Ed25519

Every binary is signed with **minisign** (Frank Denis' Ed25519 signature
format). We chose minisign over hand-rolled `ed25519-dalek` framing because:

- The `.minisig` container is a well-specified, widely-tooled format. Aaron can
  sign with the standard `rsign2` CLI (pure Rust) on any machine without our
  code present.
- `minisign-verify` is a **zero-dependency** verifier crate — the smallest
  possible trusted-verification surface to ship to end users.
- It bakes in a trusted-comment + global signature so the comment itself is
  signed (can't be swapped).

### Flow (fail-closed)

```
download binary bytes  ──┐
download .minisig text ──┤
                         ▼
   verify_signature(bytes, minisig, EMBEDDED_PUBLIC_KEY)
                         │  minisign_verify::PublicKey::verify
            ┌────────────┴────────────┐
          Ok(())                   Err(_)
            │                         │
   write over running exe       abort, surface error,
   (self-replace), prompt       NEVER touch the binary
   restart
```

Verification happens **before** a single byte is written over the running
executable. There is no code path that installs an unverified or
failed-verification binary. If the embedded key is still the dev placeholder
(see below) `verify_signature` returns `NoSigningKey` and the install is
refused — so an un-keyed build can never be tricked into installing anything.

### The embedded public key

The public key is a compile-time constant in
`rust/crates/wylde-updater/src/pubkey.rs`:

```rust
pub const PUBLIC_KEY: &str = "<base64 minisign public key — one line>";
```

A minisign public key is **public by design** — it is meant to be embedded and
committed. What must *never* be committed is the **private** signing key. The
repo therefore:

- commits `pubkey.rs` with a clearly-labelled **dev placeholder** key string,
- ships `keys/pubkey.pub.example` documenting the on-disk `.pub` format,
- gitignores the private key material (`rust/crates/wylde-updater/keys/*.key`,
  `*.sec`, and any `.wylde-release/` dir) so a real signing key can sit beside
  the source on Aaron's machine without ever being staged.

See [Key management](#key-management--release-runbook) for how Aaron generates
the real key and swaps the placeholder.

---

## Install — replacing a running binary on Windows

Windows holds an exclusive sharing lock on a running `.exe`, so we cannot
overwrite it in place. We use the [`self-replace`] crate, which performs the
standard Windows trick: rename the running image aside, drop the new bytes at
the original path, and let the OS clean up the renamed stub on next launch. The
new binary takes effect on the **next start**, so after a successful install the
UI prompts the user to restart.

`install_update` is intentionally *not* unit-tested against a live swap (a unit
test cannot safely replace its own test runner). The testable seam is
`stage_update`, which writes + sanity-checks the staged file; the `self-replace`
call itself is a thin, documented wrapper.

[`self-replace`]: https://crates.io/crates/self-replace

---

## Persistence

Update preferences live in the lifecycle daemon's
`<wylde_root>/data/preferences/updater.json`, read/written by the Settings panel
through the existing `updater.get_prefs` / `updater.set_prefs` pipe verbs
(`Core/Lifecycle/updater_prefs.py`). Shape:

```jsonc
{
  "enabled":      false,      // master: when false, ZERO network calls
  "auto_check":   false,      // background check on the cadence below
  "frequency":    "weekly",   // daily | weekly | monthly
  "channel":      "stable",   // stable | beta   ← added this slice
  "last_checked": null        // unix epoch (seconds) of last check
}
```

`channel` was added to the daemon's validation in this slice (the only edit to
existing Python — one accepted key + a test). Everything else was already wired
by the gpui Settings port.

The privacy-first default is **everything off**: a fresh install makes no
network call until the user flips "Check for updates" on.

---

## Settings UI (this slice)

The Updates section gains, when checking is enabled:

- a **Stable / Beta** channel pill (cycles, persists via `set_prefs`),
- a **Check now** button → `wylde_updater::check_for_update(channel, version)`,
- a status line: *checking… / up to date / update available `vX.Y.Z` / error*,
- an **Install update** button (shown only when an update is available) →
  download + verify + install, then a "restart to apply" line.

The updater is a **blocking** API. The GUI lives on gpui's executor (no tokio
reactor), so it calls the updater through the Pipe crate's existing
`bridged_spawn_blocking`, which hops the work onto the shared tokio runtime's
blocking pool — the same bridge the named-pipe IO already uses.

---

## Key management — release runbook

> **Status: the production key has been generated and baked in (2026-06-04).**
> Key ID `DA7E13F4E9F2ACB6`, base64
> `RWS2rPLp9BN+2obJk6h80IJAlurEyac8bz7REt0ea7v6uLG2AoppP0kb`. The private key
> lives **only** on Aaron's dev host at
> `rust/crates/wylde-updater/keys/wylde-signing.key` (gitignored, never
> committed). The one-time procedure below is retained for **key rotation**;
> for a normal cut skip to [per release](#per-release-build--sign--publish).

Aaron generated the key once, on his dev machine, and it never enters the repo.
We standardise on `rsign2`, the pure-Rust minisign CLI (honours the
everything-Rust rule).

### One-time: generate (or rotate) the signing key

```powershell
cargo install rsign2
# -W = passwordless secret key (what we used: avoids the interactive prompt and
#       keeps unattended release builds simple). Drop -W to encrypt the secret
#       key with a passphrase instead — rsign then prompts on every
#       generate/sign.
rsign generate -W `
  -p rust/crates/wylde-updater/keys/wylde-signing.pub `
  -s rust/crates/wylde-updater/keys/wylde-signing.key
```

`rsign generate` prints the public key and writes two files into
`rust/crates/wylde-updater/keys/` (gitignored except for `pubkey.pub.example`):

- `wylde-signing.key` — **PRIVATE. Never commit. Back up offline.** Matched by
  the `*.key` rule in `keys/.gitignore`.
- `wylde-signing.pub` — public; its second line is the base64 key. Also
  gitignored: the base64 is already baked into `pubkey.rs`, so the loose file
  is redundant in the repo.

The key we generated is **passwordless** (`-W`); there is no signing passphrase.
To add one later, regenerate without `-W` (interactive prompt) and re-bake the
new public key. Local-only notes about the key — including any future
passphrase — live in `keys/KEY_NOTES.md` (gitignored), never in the repo.

Swap/rotate the embedded key: copy the base64 line from `wylde-signing.pub`
into `PUBLIC_KEY` in `rust/crates/wylde-updater/src/pubkey.rs`, commit that one
line, cut a release built from it. From then on the shipped binary trusts only
binaries signed by `wylde-signing.key`. The `tests/embedded_key_roundtrip.rs`
integration test guards a botched swap: it verifies a committed fixture
signature against the embedded key, so a typo'd base64 or a key/private-key
mismatch fails the test suite before it can ship.

### Per release: build → sign → publish

```powershell
# 1. Build the release binary
cargo build --release --manifest-path Core/GUI/Cargo.toml -p wylde-gui
$bin = "Core/GUI/target/release/wylde-gui.exe"
$named = "wylde-gui-x86_64-pc-windows-msvc.exe"
Copy-Item $bin $named

# 2. Sign it (produces <name>.minisig). -W because the key is passwordless;
#    drop -W and supply the passphrase if the key was ever rotated to one.
rsign sign -W -s rust/crates/wylde-updater/keys/wylde-signing.key $named

# 3. Publish to GitHub Releases (gh CLI).
#    --prerelease => Beta channel; omit it for Stable.
gh release create v0.2.0 $named "$named.minisig" `
    --repo PeopleWonder/wylde `
    --title "Wylde v0.2.0" `
    --notes-file RELEASE_NOTES.md
# Beta:
# gh release create v0.2.0-beta.1 $named "$named.minisig" --prerelease ...
```

The updater keys channel off GitHub's `prerelease` flag, so the only difference
between cutting a Stable and a Beta release is the `--prerelease` switch (and a
`-beta.N` semver suffix in the tag).

---

## Deferred to follow-up slices

These were consciously scoped out of slice 1 to keep the blast radius to one
reviewable change. Neither blocks shipping the manual update path.

- **`wylde-release` CLI (3b).** A convenience binary wrapping
  `generate-key` / `sign` / `publish`. The runbook above already does all of it
  with `rsign2` + `gh`; the CLI is ergonomics, not capability. Deferred until
  the release cadence justifies the wrapper.
- **Background check on boot (3d).** Firing `check_for_update` once at Shell
  startup (when `enabled && auto_check`, respecting `frequency` vs.
  `last_checked`) and surfacing the result as a lightweight tray/notification.
  This is a Shell-crate change; kept separate so the updater core lands and is
  verified on its own. The preference fields it needs (`auto_check`,
  `frequency`, `last_checked`) already exist and persist.
