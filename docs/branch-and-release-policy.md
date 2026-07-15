# Wylde — Branch, Versioning & Release Policy

**Status:** canonical · **Audience:** maintainer(s) + any external contributor · **Last reviewed:** 2026-07-14

This is the one document that says how code moves from a working idea to something the
self-updater serves to real machines. It exists because, until now, it didn't: `main`
mirrored a feature branch, so "stable" contained everything experimental — the exact risk
this policy removes. If you only read one section, read [§1 The model in one picture](#1-the-model-in-one-picture).

---

## 1. The model in one picture

```
  feat/*  fix/*  chore/*  docs/*        short-lived topic branches
        \    |    /    /                 (branch off develop, --no-ff back, then delete)
         \   |   /    /
          v  v  v    v
   ┌───────────────────────┐
   │  develop              │   the EXPERIMENTAL line. Integration trunk.
   │  (experimental trunk) │   Everything lands here first. Beta channel builds
   └───────────┬───────────┘   are cut from here (0.1.x, GitHub pre-releases).
               │
               │  promotion = a --no-ff merge, gated by CI G1–G7 + local preflight L1–L7,
               │  performed only on the maintainer's explicit say-so.
               v
   ┌───────────────────────┐
   │  main                 │   the STABLE line. Only proven releases land here.
   │  (stable / released)  │   This is what the updater's STABLE channel serves.
   └───────────────────────┘   Tagged 0.2.0, 0.2.1, … (GitHub full releases).
```

Two long-lived branches, nothing more:

| Branch | Role | Who serves it | Version strings | Protected |
|---|---|---|---|---|
| **`main`** | Stable. Only proven, gated releases. | Updater **Stable** channel | `0.2.0`, `0.2.1`, … (full GitHub releases) | Yes — no direct pushes; merges from `develop` + hotfixes only |
| **`develop`** | Experimental integration trunk. Everything lands here first. | Updater **Beta** channel | `0.1.x` (GitHub **pre-releases**) up to the 0.2 gate | Yes — no direct pushes; PRs from topic branches |
| `feat/*`, `fix/*`, `chore/*`, `docs/*`, `test/*`, `refactor/*`, `perf/*` | Short-lived topic branches | — | inherit | No |

> **Naming note.** `develop` *is* "the experimental line" in the maintainer's words. The
> name is the widely-understood convention (tooling and contributors recognise it); if you
> prefer the literal word `experimental`, rename it once — every rule below is identical.

---

## 2. Why this model, and not the alternatives

The requirement is specific: **a stable branch that follows an experimental branch; everything
proves out on experimental before it is promoted to stable, because the updater serves stable
and a broken stable ships to real machines.** Two candidate models were weighed against that.

### Rejected: full git-flow (main + develop + release/* + hotfix/* + feature/*)

Git-flow's `release/*` and `hotfix/*` ceremony exists to let *many developers* stabilise
release N while others keep landing N+1 features on `develop`. Wylde has **one developer**.
There is no concurrent-stabilisation problem to solve, so `release/*` branches buy nothing but
two more long-lived refs to keep in sync. **Overkill — rejected.**

### Rejected: pure trunk-based (single `main`, release from tags, no `develop`)

The lightweight favourite — but it fails the core requirement head-on. If `main` is the trunk,
then `main` *contains* every experimental commit, and the updater's Stable channel would serve
it. That is precisely today's bug (`main == feat/thought-bubble-system`). Trunk-based assumes
`main` is always releasable; Wylde's reality is that `main`-quality is *earned by a live
preflight the CI cannot run*, so unproven work must live somewhere that is **not** the
updater's stable source. **Rejected for a self-updating app.**

### Chosen: trunk-based **with a promoted stable branch** ("git-flow minus the ceremony")

Invert the usual trunk: the trunk is **`develop`** (experimental), and **`main`** is a permanent
*release branch* that only ever receives proven promotions. This is trunk-based development
(one integration line, short-lived topic branches, no `release/*`/`hotfix/*` machinery) with
exactly one addition — a stable pointer that lags the trunk until a release is gated. It is the
minimum structure that encodes "stable follows experimental," and it maps 1:1 onto the
updater's existing two-channel model (§4). **This is the model.**

---

## 3. Versioning

### 3.1 The scheme

Wylde is pre-1.0, so **SemVer's pre-1.0 clause governs**: within `0.y.z`, anything may change
between builds. That is not a disclaimer — it is the versioning contract, and it is why the
experimental line can carry plain `0.1.x` strings and still be legitimately "not stable."

| Line | Version string | GitHub release flag | Updater channel | Meaning |
|---|---|---|---|---|
| **Experimental** (`develop`) | `0.1.x` — `x` increments per shipped experimental milestone | **pre-release** | Beta only | "The 0.1 line, building toward 0.2." Unstable by pre-1.0 definition. |
| **Stable gate** (`main`) | `0.2.0` | full release | Stable **and** Beta | The maintainer opened the gate. First verified release of the modern stack. |
| **Stable patches** (`main`) | `0.2.1`, `0.2.2`, … | full release | Stable and Beta | Post-0.2 fixes promoted through the same gate. |

Why plain `0.1.x` and not `0.2.0-alpha.N`:

- It honours the maintainer's stated scheme literally ("everything building up to it is 0.1.x").
- The **GitHub pre-release flag** — not the version string — is what keeps these off the Stable
  channel (see §4). SemVer pre-1.0 already means "unstable," so the string needs no `-alpha`
  suffix to be honest.
- `0.1.x → 0.2.0` is monotonic (`0.2.0 > 0.1.9 > … > 0.1.0-alpha.1`), so opening the gate is a
  clean forward step and *every* user — Stable and Beta — upgrades to `0.2.0` when it lands.

> **One decision left to the maintainer** (does not change routing, gates, or promotion — only
> the literal string): if you'd rather the experimental strings *read* as the run-up to 0.2, use
> `0.2.0-alpha.N` instead of `0.1.x`. Both are pre-release-flagged, both sort below `0.2.0`, both
> route identically. `0.1.x` is the recommendation because it matches the stated scheme; the
> current shipped tag `v0.1.0-alpha.1` is already below every `0.1.x`, so either path is monotonic.

**Steady state after 0.2:** once `0.2.0` is stable, `develop` carries the run-up to the next
minor as pre-releases (`0.3.0-dev.N`, or `0.2.x` experimental patches pre-release-flagged),
and the next gate drops the pre-release marker to ship `0.3.0` / `0.2.x` stable. Same mechanism,
forever.

### 3.2 Build metadata (traceability)

Release *artifacts* may carry SemVer build metadata identifying the exact commit —
`0.1.3+g<shortsha>`. Build metadata is ignored for precedence (SemVer §10), so it never affects
updater ordering; it exists purely so a bug report names an exact build. Optional but cheap.

### 3.3 Where the version is stamped, and keeping it consistent

The workspace is **two separate Cargo workspaces** (they have separate `Cargo.lock`s and target
dirs), so they cannot share one `[workspace.package] version`. The version literal lives in:

- `rust/Cargo.toml` (`[workspace.package] version`, currently line ~101)
- `Core/GUI/Cargo.toml` (`[workspace.package] version`, currently line ~52)
- `Core/GUI/Frontend/test-support/Cargo.toml`

**Target state:** each workspace declares its version once under `[workspace.package]`, and every
member crate uses `version.workspace = true`, so a bump touches exactly **two files** (one root
per workspace). A stamp helper (`tools/set-version.sh <version>`, or an `xtask set-version`
subcommand) rewrites both roots together — never by hand, never one at a time. A split version
is exactly the silent inconsistency the gate exists to catch.

### 3.4 G7 — the version-consistency gate (real, wired now)

`tools/check-versions.sh` (added by this change, wired into `ci.yml` as the `version-consistency`
job) enforces:

1. **Always:** the version in `rust/Cargo.toml` equals the version in `Core/GUI/Cargo.toml`.
2. **On a tag build** (`GITHUB_REF` = `refs/tags/vX.Y.Z`): the tag (minus the leading `v`) equals
   that workspace version.

The release tool (`wylde-release`) should refuse to publish if this check fails. Until an
`xtask set-version` exists, the two roots are bumped by hand and G7 catches a missed one.

---

## 4. Release channels — how the updater routes stable vs experimental

**No updater code changes are required.** The existing model already fits:

- `wylde-updater`'s `Channel` enum (`rust/crates/wylde-updater/src/release.rs`) has two variants,
  `Stable` (default) and `Beta`. **`Beta = Stable ∪ pre-releases`** — Beta additionally surfaces
  GitHub pre-releases; Stable never does.
- The updater's release filter (`candidates()`, same file) keeps a release iff: not a draft **and**
  (channel is Beta **or** the release's `prerelease` flag is false) **and** the tag parses as SemVer.
  It then picks the highest SemVer.
- The publisher (`tools/wylde-release`) sets GitHub's `prerelease: true` when `--channel beta`.

Therefore the branch model maps onto channels with **tagging discipline alone**:

| You cut a release from… | Tag | `wylde-release --channel` | GitHub flag | Who receives it |
|---|---|---|---|---|
| `develop` (experimental) | `v0.1.x` | `beta` | pre-release | **Beta channel only** |
| `main` (stable gate/patch) | `v0.2.0` | `stable` | full release | **Stable + Beta** (Beta ⊇ Stable) |

A user on the default **Stable** channel is *structurally* protected from experimental builds:
those builds are pre-release-flagged and the Stable filter drops them. A user who wants to run
ahead flips to **Beta** in Settings (the existing stable⇄beta pill; `updater.set_prefs`). Today,
because the only tag is the pre-release `v0.1.0-alpha.1`, the **Stable channel correctly serves
nothing** — no stable release exists yet. That is the right, safe state until `0.2.0` is gated.

> **If a distinct third tier is ever wanted** (e.g. `alpha` ⊄ `beta`): GitHub has only one
> `prerelease` boolean, so all non-stable tiers currently collapse to it. A true third channel
> needs a new `Channel` variant *and* SemVer-pre-release-identifier filtering in `candidates()`
> (parse `-alpha` vs `-beta`). Not needed for the two-line model — noted so it isn't reinvented.

---

## 5. What exactly promotes experimental → stable

Promotion is a **`--no-ff` merge of `develop` into `main`, immediately tagged**, and it happens
**only on the maintainer's explicit say-so.** 0.2 is a gate someone opens, not a date.

**Entry conditions (all must be green on the `develop` commit being promoted):**

- **CI gates G1–G7** green (see `docs/release-checklist.md` and §6 below).
- **Local preflight L1–L7** green on the maintainer's release machine (the launch-and-verify
  smoke + panel-walk that CI structurally cannot run — GPU/Ollama/Memgraph/desktop).

**The promotion itself:**

```bash
# on the gated develop commit, versions already bumped to 0.2.0 and G7-consistent
git checkout main
git merge --no-ff develop          # the merge IS the promotion; preserves the branch topology
git tag -a v0.2.0 -m "Wylde 0.2.0" # the tag IS the release
git push origin main --follow-tags
wylde-release publish --version 0.2.0 --channel stable --binary <path> …
# then: confirm the updater picks it up on a second machine/profile (L on the checklist)
```

The **merge** promotes; the **tag** releases; the **gates** are the entry condition; the
**say-so** is the trigger. Nothing reaches `main` — and therefore the Stable channel — that
hasn't passed all three.

**Hotfixes** (a stable release is broken in the field): branch `fix/*` off `main`, fix, gate,
`--no-ff` into `main`, tag `0.2.1`, then merge `main` back into `develop` so the fix isn't lost.
This is the only path that writes to `main` without going through `develop` first, and it's rare.

---

## 6. Tie-in to the release gates (G1–G7, L1–L7)

The gates are defined in the roadmap (`docs/plans/repo-wide-roadmap-2026-07.md` §3) and operated
via `docs/release-checklist.md`. Branch policy's contribution is *where each gate binds*:

- **G1–G7 bind at merge into `develop` and again at promotion to `main`.** CI runs on every PR
  and on pushes to both long-lived branches (`ci.yml`). A red CI blocks the merge.
- **L1–L7 bind at promotion only** (they need the live stack). They are the difference between
  "it compiles" and "it runs," and between "it launches" and "every page works." No promotion
  without a green preflight.
- **G7** (version-consistency) is live as of this change; **G4 (clippy `-D warnings`)** and
  **G6 (`cargo fmt --check`)** are staged as commented stubs in `ci.yml` — turn them on once the
  tree is clean (roadmap T0.1), because flipping them on a dirty tree red-walls CI for no gain.

---

## 7. Branch naming & lifecycle (kills the current inconsistency)

Topic-branch prefixes mirror Conventional Commit types, so the branch name predicts the commit
type and the changelog section:

| Prefix | For | Example |
|---|---|---|
| `feat/` | a user-facing feature | `feat/temporal-memory-graph` |
| `fix/` | a bug fix | `fix/embed-pool-exhaustion` |
| `chore/` | tooling, deps, repo mechanics (no user-facing behaviour) | `chore/repo-hygiene` |
| `docs/` | docs only | `docs/add-extension-guide` |
| `test/` | tests only | `test/gui-workspaces` |
| `refactor/` / `perf/` | restructure / speed, no behaviour change | `refactor/pipe-split` |

**Rule of thumb for the old `chore/` vs `feat/` confusion:** if a user would notice it in the
app, it's `feat/`; if only the developer would, it's `chore/`. A branch that both adds a feature
and does chores is named for its *headline* change.

**Lifecycle:** branch off `develop` → work → open PR into `develop` → CI green → **`--no-ff`
merge** (preserves the branch as a visible unit in history) → **delete the branch**. Topic
branches are short-lived; a branch that outlives its merge is a smell.

**Retired conventions:**

- **`feeltest/*`** integration branches (`feeltest/all`, `feeltest/organize+tabulate`, …) — these
  were throwaway "merge several branches and feel the result" scratch branches. Keep that practice
  as *local, unpushed* worktrees; don't push feel-test branches to the remote. They are not part
  of the model.
- **A feature branch acting as trunk** (`feat/thought-bubble-system`) — retired by §9.

---

## 8. The two external service repos (`wylde-organize`, `wylde-tabulate`)

These are **separate repos**, single-purpose leaf services that Core spawns via `wylde-lifecycle`.
They do **not** need the two-line model. Rationale: they aren't the updater-served monolith, and a
broken service is **non-fatal** — Core logs a warning and skips the spawn (`start_discovered` in
`wylde-lifecycle`), so the "don't break stable" stakes are far lower than for Core itself.

**Their model — trunk-based, lightest form:**

- Single `main` trunk; short-lived topic branches; tag their own SemVer (`vX.Y.Z`) independently.
- **They version on their own cadence** — no lockstep with Core. Lockstep across separate repos is
  a maintenance trap.
- Each declares a **minimum-Core compatibility floor** in its folder `manifest.json`, enforced by
  Core's loader (§8.1). This is the coupling contract instead of version lockstep.
- Add a `develop` line only if one ever grows enough concurrent work to need it. Until then, `main`
  + tags is sufficient.

### 8.1 The `min_core_version` compatibility floor — concrete spec

**What it is:** a service manifest declares the oldest Core it is compatible with; Core refuses to
spawn a service whose floor it doesn't meet, instead of letting an incompatible service boot and
fail in confusing ways.

**Manifest field** (in each service's folder `manifest.json`, alongside `name`/`enabled`/`version`):

```json
{
  "name": "wylde-organize",
  "enabled": true,
  "version": "1.4.0",
  "min_core_version": "0.2.0"
}
```

- Optional. Absent ⇒ "no floor declared" ⇒ Core spawns it (back-compatible with today's manifests).

**Comparison semantics:** Core parses both its own version (`env!("CARGO_PKG_VERSION")`) and the
floor with the `semver` crate and spawns iff `core_version >= min_core_version`.

- **Pre-release caveat (must be handled deliberately):** SemVer orders `0.2.0-alpha.1 < 0.2.0`.
  So a Core running `0.2.0-alpha.1` does **not** satisfy a floor of `0.2.0`. Since services are a
  post-0.2 concern and Core will be at a released `0.2.x` when they ship, **floors should target a
  released Core version** (`0.2.0`, not `0.2.0-alpha.1`). If a service must run against a Core
  pre-release, compare against the floor's release-equivalent (strip Core's pre-release identifier
  before comparing) — document whichever rule is chosen at the comparison site.

**What Core does when the floor isn't met:** mirror the existing non-fatal contract — **log a
loud warning naming the service, its floor, and Core's version, then skip the spawn and continue**
(`Ok(())`). Core is unaffected; the incompatible service simply does not start. The GUI panel that
lists the service in `required_services` will then render its existing `ServiceUnavailable` stub,
which is the correct user-facing signal.

**Where it lands in code** (implementation is a post-0.2 roadmap item, not part of this hygiene
change):

- Add `min_core_version: Option<String>` to `DiscoveredService` (`wylde-lifecycle/src/registry.rs`,
  ~line 179) and read it in `discovered_bucket_services_in` (~line 223) next to `name`/`enabled`.
- Enforce in `start_discovered` (`wylde-lifecycle/src/state/services.rs`, ~line 205) right after
  the `enabled` check, before resolving the binary — skip-with-warning on failure.
- Add `semver = "1"` to `wylde-lifecycle/Cargo.toml` (already a vetted workspace transitive dep via
  `wylde-updater`; no new third-party review).

---

## 9. Migration — from "feature-branch-as-trunk" to this model

See the dedicated, step-by-step, **safe (no public history rewrite)** plan in
[`docs/migration-to-branch-model.md`](./migration-to-branch-model.md). The one-paragraph version:
`main` is a *strict ancestor* of the current trunk (it's 7 commits behind, with zero divergence),
so no rewrite is needed — the migration is a GitHub branch **rename** (`feat/thought-bubble-system`
→ `develop`, which preserves history and redirects open PRs), adding branch protection to both
long-lived branches, freezing `main` where it is until `0.2.0` is gated, and a staged cleanup of
the stale local branches. The steps that touch the GitHub web UI are flagged as maintainer-hand.

---

## 10. Keeping this document alive

This policy and the roadmap are the two canonical process artifacts. Update this file whenever the
branch model, versioning scheme, or channel routing changes — and review it at each release as part
of `docs/release-checklist.md`. It is public (tracked) on purpose: an external contributor should be
able to learn how to land a change from this file alone.
