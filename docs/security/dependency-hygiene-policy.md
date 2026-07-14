# Dependency hygiene policy

_Owner: Aaron. Established 2026-07-14. Next scheduled review: **2026-10-14** (quarterly)._

This is the standing policy for keeping Wylde's dependencies current and its
advisory posture honest, so updates flow in as small reviewable PRs instead of
piling up until a `git push` warning forces a scramble.

## The machinery (what runs, when)

| Piece | File | Trigger | What it does |
|-------|------|---------|--------------|
| **Dependabot** | `.github/dependabot.yml` | Weekly (Mon 06:00 UTC) + on new advisories | Opens grouped patch/minor bump PRs per Cargo directory; individual PRs for majors and security updates; keeps GitHub Actions pinned. |
| **CI** | `.github/workflows/ci.yml` | Every PR + push to trunk/main | `cargo build` + `cargo test` the `rust/` workspace on Windows; `cargo build` the `Core/GUI` and `tools/*` workspaces. Makes a bump PR mergeable with confidence. |
| **Security audit** | `.github/workflows/security-audit.yml` | Every relevant PR + weekly cron (Mon 07:00 UTC) | `cargo deny check advisories` against `rust/` and `Core/GUI`. A newly-published advisory against an already-pinned dep turns this red within days. |
| **Policy (advisories)** | `rust/deny.toml`, `Core/GUI/deny.toml` | — | The allow-list of knowingly-accepted advisories, each with a reason + review date. |

The weekly Dependabot run and the weekly cargo-deny cron are the two halves that
keep the backlog from re-forming: Dependabot pushes *available* updates at you;
cargo-deny fails the build when a *pinned* dep becomes vulnerable and no update
is available yet.

## How to handle a red security-audit check

1. **A fixable advisory** (a patched version exists within the allowed semver
   range): let the Dependabot PR land, or run `cargo update -p <crate>` and open
   a PR. This is the common case and needs no policy exception.
2. **An unfixable advisory** (no compatible patched version, or the fix is
   blocked by an upstream pin): add it to the relevant `deny.toml` `ignore`
   list. **Every ignore entry MUST have:**
   - a `reason` explaining *why* it can't be fixed now, and
   - a **`Review by <date>`** (default: the next quarterly review) inside that
     reason, so it gets revisited rather than ignored forever.

   Never ignore an advisory without a reason and a review date. An ignore with no
   expiry is how backlogs are born.

## Currently-accepted advisories (review by 2026-10-14)

These are the advisories the gate is knowingly accepting today. Full technical
triage: [`dependabot-triage-2026-07-11.md`](dependabot-triage-2026-07-11.md).

### Not compiled into the shipped product (Windows-only ship, Linux-only deps)

The product ships Windows-only (`wylde-gui.exe`). A chunk of the GUI's advisory
surface is the Linux GTK3 / X11 / D-Bus binding stack that gpui + wry + tray-icon
pull behind `cfg(target_os = "linux")`. Verified absent from the shipped binary
with `cargo tree -i <crate> --target x86_64-pc-windows-msvc` (empty output).

- **`glib` 0.18.5 — RUSTSEC-2024-0429** (Dependabot #1, GHSA-wrw7-89jp-8q8g).
  `VariantStrIter` unsoundness. Fix ships only in `glib` 0.20 (a full gtk-rs
  0.18→0.20 migration), blocked by `wry` 0.54 / `tray-icon` 0.19 pinning the
  GTK3 bindings at `^0.18`. **Not deletable** — both `wry` (extension-iframe
  host, `wylde-webview`) and `tray-icon` (system tray) are LIVE gpui-era code,
  not dead Tauri deps (see "Investigation notes" below).
- **`quick-xml` 0.30 / 0.39 — RUSTSEC-2026-0194, RUSTSEC-2026-0195.** Two DoS
  advisories. Pulled only by `xcb` (X11) and `zbus_xml` (D-Bus), both Linux-only.
  Fix is `quick-xml` 0.41 (semver-major), pinned by upstream; not reachable via
  `cargo update`.

### Compiled but dormant (no reachable exploit path)

- **`async-tar` 0.5.1 — GHSA-35rm-7j9c-2f7m / CVE-2026-53600** (Dependabot #18).
  PAX-header entry smuggling. Compiled into the Windows build via Zed's
  `http_client` (the pinned gpui rev), but **dormant**: no Wylde code path feeds
  an attacker-controlled tar into `http_client`. The self-updater is a separate
  crate (`wylde-updater`, GitHub Releases + minisign verification, no tar).
  - _Not fixable by advancing the gpui rev:_ the fix is `async-tar` 0.6.1
    (semver-major), and even Zed `main` still pins `async-tar = "0.5.1"` (verified
    2026-07-14). Bumping the rev would not move it, and Zed `main` has also moved
    to edition 2024 — a large breaking jump. Tracked against a future gpui-rev
    bump that itself carries `async-tar` ≥ 0.6.1.
  - _Not in `deny.toml`:_ RustSec has not assigned this a RUSTSEC id yet, so
    cargo-deny can't see it. **Dependabot (GHSA-sourced) is the gate for this
    one.** Re-check whether RustSec has ingested it at the next review; if so, add
    the RUSTSEC id to `Core/GUI/deny.toml`.

### Unmaintained direct dependency (informational, no safe upgrade)

- **`bincode` 1.3.3 — RUSTSEC-2025-0141** (in `rust/deny.toml`). Direct dep of
  `wylde-gateway`. `bincode` 2.x is an incompatible rewrite ("no safe upgrade
  available"). Tracked for a deliberate 2.x port.

The broader set of *transitive* unmaintained crates (the GTK3 stack, `backoff`,
`instant`, `paste`, `rustls-pemfile`, `async-std`, …) is intentionally NOT
enumerated as ignores: `deny.toml` sets `unmaintained = "workspace"`, which flags
only crates a workspace member depends on *directly*. That keeps the gate signal
(a new direct unmaintained dep fails) without drowning in transitive noise we
can't act on.

## The structural blocker: the gpui git-rev pin

`Core/GUI/Cargo.toml` pins `gpui` / `gpui_platform` / `http_client` to a **git
rev** of `zed-industries/zed` (`rev = "b3d93d44"`; gpui is not published to
crates.io). This is deliberate and correct for build stability — but it has a
hard consequence for dependency hygiene:

> **Every transitive dependency reachable only through gpui is frozen at whatever
> version that rev's lockfile selected. Dependabot cannot bump a git rev, and
> `cargo update` cannot cross a semver-major that the pinned rev's manifests
> require.** This is the root cause of `async-tar` being stuck at 0.5.1.

**Policy for the gpui pin:**

1. **Bump the rev deliberately, on a cadence** — target roughly once a quarter,
   folded into the quarterly review. A rev bump is a single `Cargo.toml` change
   but a wide blast radius; treat it as its own PR with a full `Core/GUI` build
   and a smoke-run, never bundled with other work.
2. **When bumping, follow the checklist already in `Core/GUI/Cargo.toml`**:
   verify `gpui-component`'s WebView path still compiles and the Settings panel
   still types after the swap.
3. **Prefer a published crate when one exists.** If/when gpui (or the specific
   sub-crates Wylde uses) is published to crates.io, move off the git rev so
   Dependabot can manage it like everything else. Re-check availability each
   review.
4. **Advisories reachable only through the gpui pin** are accepted in `deny.toml`
   with a review date and cleared opportunistically at the next rev bump — do not
   fork gpui to patch a single transitive advisory.

## Review cadence

Quarterly (next **2026-10-14**). At each review:

- Re-run `cargo audit` / `cargo deny check advisories` against both lockfiles;
  clear anything now fixable.
- Re-read every `deny.toml` ignore whose `Review by` date has passed: is it still
  unfixable? Still not reachable? Update the reason and push the date, or remove
  the ignore.
- Consider a gpui rev bump (see above); check whether gpui has been published.
- Check whether RustSec has assigned `async-tar` a RUSTSEC id (then add it to
  `Core/GUI/deny.toml`).

## Change log

- **2026-07-14** — Policy established. Set up Dependabot, CI, and the cargo-deny
  security-audit gate (none existed before). Fixed the then-current fixable
  advisories in both workspaces via `cargo update` (lock-only, no code change):
  `crossbeam-epoch` 0.9.18 → 0.9.20 (RUSTSEC-2026-0204, DoS) and `anyhow`
  1.0.102 → 1.0.103 (RUSTSEC-2026-0190, unsoundness). Confirmed the two original
  Dependabot alerts (`glib`, `async-tar`) are unfixable-for-now and documented
  them as accepted with review dates.
