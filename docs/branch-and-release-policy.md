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
| **The 0.2 series** (`main`) | `0.2.1`, `0.2.2`, … `0.2.24`, … | full release | Stable and Beta | A **running series**, not a bugfix-only track — see §3.2. |

Why plain `0.1.x` and not `0.2.0-alpha.N`:

- It honours the maintainer's stated scheme literally ("everything building up to it is 0.1.x").
- The **GitHub pre-release flag** — not the version string — is what keeps these off the Stable
  channel (see §4). SemVer pre-1.0 already means "unstable," so the string needs no `-alpha`
  suffix to be honest.
- `0.1.x → 0.2.0` is monotonic (`0.2.0 > 0.1.9 > … > 0.1.0-alpha.1`), so opening the gate is a
  clean forward step and *every* user — Stable and Beta — upgrades to `0.2.0` when it lands.

> **DECIDED — 2026-07-16 (was "one decision left to the maintainer").** The first release is
> plain **`0.2.0`**, *not* `0.2.0-alpha.N`. The existing `v0.1.0-alpha.1` tag is left alone —
> it's history (and `protect-version-tags` makes `v*` tags immutable anyway). Recorded verbatim:
>
> > "leave the tag alone. we'll bump to 0.2 and track from there. initial release 0.2.0, and we
> > count up from there until I decide it's complete enough to seem it worthy of a 0.3.
> > therefore: 0.2.0, 0.2.1, 0.2.2.....0.2.24....."

### 3.2 The 0.2 series is a running series, and 0.3 is declared — not derived

Two things follow from the decision above, and both are **deliberate departures from
SemVer orthodoxy**. They are written down here so nobody "corrects" them later:

1. **`0.2.x` patch bumps will carry features, not just fixes.** `0.2.1 … 0.2.24` is a running
   series — the ordinary way work ships on the 0.2 line. Do **not** read a patch bump as
   "bugfix-only", and do **not** propose a minor bump because a release contains a feature.
2. **`0.3` is a maintainer declaration, not a semver derivation.** 0.3 happens when the
   maintainer decides the line is "complete enough to seem it worthy of a 0.3" — a judgement
   call about product substance, not a rule triggered by change type. Nothing automated should
   ever compute the next minor.

This is coherent because pre-1.0 SemVer already says anything may change within `0.y.z` (§3.1) —
the series just uses that latitude deliberately. The gate, the channels, and the promotion path
are unchanged; only the *meaning* of an increment is.

**Steady state during 0.2:** `develop` carries the run-up to the next `0.2.x` as pre-releases;
each gate opening drops the pre-release marker and ships `0.2.x` stable. Same mechanism as §3.1,
repeated for as long as the maintainer keeps the 0.2 line open.

### 3.3 The bump itself was gated on the maintainer's say-so — now given (#36 DONE)

**Status: BUMPED to `0.2.0` (2026-07-17, #36).** The workspace versions were held at
`0.1.0-alpha.1` until the maintainer's explicit go-ahead, which he gave on 2026-07-17
("okay, then carry out 36"), reversing the earlier hold:

> "im not ready to switch to 0.2 yet, it's not ready" — 2026-07-16

The bump was made uniform across both workspaces and every non-`version.workspace` crate:
`rust/Cargo.toml` and `Core/GUI/Cargo.toml` (the two `[workspace.package]` versions G7
compares), `Core/GUI/Frontend/test-support/Cargo.toml`, `tools/xtask`,
`tools/wylde-release`, `rust/tests/parity`, and the installer's source defaults — with all
five `Cargo.lock`s regenerated so the `--locked` CI gates don't red-wall on a stale lock.
`version consistency (G7)` passes.

**Still gated on a separate say-so: tagging + publishing (#38).** #36 is the *version
string in the source tree*; it does not create the `v0.2.0` tag, promote `develop`→`main`,
or run `wylde-release publish`. Until #38, the latest *release* remains the pre-release
`v0.1.0-alpha.1`, the Stable channel correctly serves nothing (§4), and the CHANGELOG's
0.2.0 section is headed "— unreleased". The existing `v0.1.0-alpha.1` tag is left untouched
(§3.1).

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

### 4.1 Staged rollout (ship to 1% before 100%) — assessed, not now

Real studios stage a release: 1% → 10% → 100%, watching crash telemetry between rings, so a bad
build hits a handful of users, not everyone. Should Wylde? **Honest answer: no — the two-channel
model already gives you the same protection at this scale, and true percentage rollout costs
infrastructure Wylde deliberately doesn't have.**

- **What staged rollout needs:** a server that hands different clients different "latest" answers by
  cohort, plus **crash/health telemetry** flowing back to decide whether to widen the ring. Wylde's
  updater polls a *static* GitHub Releases list (every client sees the same releases), and Wylde is
  **local-first with no telemetry by design** — there is no phone-home to watch a canary's health.
  Building percentage rollout means building a rollout server and a telemetry pipe, both of which cut
  against the privacy thesis.
- **What you already have that does the same job:** the **Beta channel is your canary ring.** Beta
  users run `develop`'s `0.1.x` pre-releases first; problems surface there before a build is ever
  promoted to Stable. That is a staged rollout — just opt-in by channel instead of by percentage.
  Combined with the local preflight (L1–L7) that must pass before promotion, a broken build has to
  clear (a) the gate and (b) the Beta cohort before it can reach a Stable user.
- **The one cheap upgrade worth considering later, not now:** if Beta ever has enough users that you
  want a "soak" signal, add a **minimum soak time** to the release checklist — "a `0.1.x` must sit on
  Beta for N days with no new blocker Issue before it's eligible for promotion." That's a checklist
  line, not infrastructure, and it approximates ring-widening without a server or telemetry.

**Recommendation:** stable-vs-experimental (Stable/Beta) **is** the right-sized staged rollout for
Wylde's user count and privacy posture. Revisit percentage rollout only if the user base grows large
enough that "the whole Beta cohort" is too big a blast radius — which is a good problem to have and a
long way off.

---

## 5. What exactly promotes experimental → stable

Promotion is a **`--no-ff` merge of `develop` into `main`, immediately tagged**, and it happens
**only on the maintainer's explicit say-so.** 0.2 is a gate someone opens, not a date.

**Entry conditions (all must be green on the `develop` commit being promoted):**

- **CI gates G1–G7** green (see `docs/release-checklist.md` and §6 below).
- **Local preflight L1–L7** green on the maintainer's release machine (the launch-and-verify
  smoke + panel-walk that CI structurally cannot run — GPU/Ollama/Memgraph/desktop).

**The promotion itself** — a **PR from `develop` into `main`** (branch protection blocks direct
pushes to `main`, so the promotion *is* a PR; that's deliberate — even the maintainer promotes
through the gate):

```bash
# on the gated develop commit, versions already bumped to 0.2.0 and G7-consistent
gh pr create --base main --head develop --title "Release 0.2.0" --body "Promotion — gates + preflight green"
# CI (G1–G7) runs on the PR; the local preflight (L1–L7) receipt is green on this commit
gh pr merge --merge        # a --no-ff merge commit; branch protection lets it in once checks are green
git checkout main && git pull
git tag -a v0.2.0 -m "Wylde 0.2.0"   # the tag IS the release (Stable = a non-prerelease GitHub release)
git push origin v0.2.0               # tag ruleset allows create, blocks later delete/move
wylde-release publish --version 0.2.0 --channel stable --binary <path> …   # refuses without a green preflight receipt
git checkout develop && git merge --no-ff main && git push   # keep develop current with the promotion
```

The **PR merge** promotes; the **tag** releases; the **gates + preflight receipt** are the entry
condition; the **say-so** is the trigger. Nothing reaches `main` — and therefore the Stable channel —
that hasn't passed all of them, and GitHub *enforces* it (`docs/enforcement-matrix.md`).

**Hotfixes** (a stable release is broken in the field): branch `hotfix/*` (or `fix/*`) off `main`,
fix, gate, open a PR **into `main`** (the branch-target guard allows `hotfix/*`/`fix/*` → `main`),
merge, tag `0.2.1`, then merge `main` back into `develop` so the fix isn't lost. This is the only
path that writes to `main` without coming from `develop`, and it's rare.

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

### 6.1 Studio-grade enforcement — what actually blocks what

The reason alpha shipped broken repeatedly is not that the docs were unclear — it's that **nothing
stopped it.** A studio wouldn't have caught those four defects with better documentation; they'd have
been **blocked by a gate.** So the test for every item here is one question: **what does it block?**
If the answer is "nothing, it's a norm," it's ceremony and it's excluded (and I say why below, so you
can overrule). **The full accounting — every policy → mechanism → what it blocks → where configured →
live-yet, plus the exact `gh` commands to apply the rulesets — is `docs/enforcement-matrix.md`.**

**Enforced by machines (these BLOCK):**

| Control | What it blocks | Where it's enforced |
|---|---|---|
| **Branch protection on `main`** | any direct push / force-push / deletion of stable; any merge whose CI is red | GitHub ruleset (§ setup below). *The* control that makes "stable is only proven code" true rather than aspirational. |
| **Branch protection on `develop`** | force-push / deletion; merging a red PR | GitHub ruleset |
| **Required status checks** (backend, gui, tools, security-audit, **version-consistency**) | merging a PR that fails to build, fails tests, pulls a vulnerable crate, or has a split version | GitHub ruleset → the CI/security-audit jobs already defined |
| **G7 version-consistency job** | a release where the two workspaces disagree, or a tag ≠ the stamped version | `tools/check-versions.sh` in CI, and `wylde-release` refuses to publish on failure |
| **The local preflight (L1–L7) as the promotion gate** | promoting `develop`→`main` when the running system doesn't actually work (the launch-and-verify smoke CI can't run) | ⏳ `wylde-release preflight` / `xtask release-check` — the one-command gate `publish` refuses to run without (roadmap T0.1) |
| **`min_core` floor** | a service incompatible with the running Core spawning and failing confusingly | `wylde-lifecycle` (shipped; §8.1) |

**The rule of thumb:** discipline lives in **automation**, not in memory. A release checklist a human
*reads* is a fallback; a `publish` command that *refuses to run* until the checklist is green is the
gate. The roadmap's T0.1 turns L1–L7 from a doc into that command — that is the single most important
studio-grade upgrade, because it's the gate that stops the *next* broken release.

**Deliberately excluded as ceremony (a solo repo can't sustain them, and they block nothing useful) —
overrule any of these if you disagree:**

- **Required PR reviews / approvals.** With one maintainer, "require 1 approval" either blocks you
  from merging your own work or is satisfied by self-approval — it protects nothing and adds friction.
  *Adopt the moment a second regular contributor exists.*
- **`CODEOWNERS`.** Its job is to auto-request the right reviewer per path. With one owner it
  auto-requests you for everything — noise, not signal. It also *documents* ownership, but for a
  solo repo the README does that. *Add a one-line `* @PeopleWonder` (plus per-path owners) when
  contributors appear and you want auto-assignment.* **Not created now** — it would be pure ceremony.
- **`CODE_OF_CONDUCT.md`.** Governs a community that doesn't exist yet; folded into a CONTRIBUTING
  paragraph until it does (already done).
- **Signed-commit requirement.** Real value, real friction (key setup, CI verification). Worth it if
  Wylde ever ships to an audience that verifies provenance; over-process for now. The *release
  artifacts* are already signed (minisign, `wylde-release`), which is where signing actually protects
  users. *Commit signing is a later upgrade.*
- **Merge-queue.** Solves contention when many PRs race to merge. One dev, no contention. *Later.*

**Branch-protection setup (the concrete config — maintainer applies once, GitHub web UI).** Public
repos get branch protection / rulesets free. For each of `main` and `develop`, add a **ruleset**
(Settings → Rules → Rulesets → New branch ruleset) targeting the branch with:

- ✅ **Restrict deletions** and ✅ **Block force pushes.**
- ✅ **Require status checks to pass** → add: `backend (rust/) build + test`, `gui (Core/GUI/) build`,
  `tools build`, `version consistency (G7)`, `cargo-deny (advisories)`. ✅ **Require branches to be
  up to date before merging.**
- On **`main`** additionally: ✅ **Require a pull request before merging** (so nothing lands on stable
  except via a reviewed PR / the promotion merge) — but **leave "required approvals" at 0** (solo).
- **Do not** enable required reviews, signed commits, or a merge queue yet (see exclusions above).

This is the difference between a policy and a guarantee: after this, "you can't push experimental code
straight to stable" is enforced by GitHub, not remembered by a human.

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

### 8.1 The `min_core` compatibility floor — **implemented** (decided requirement)

Aaron confirmed the decision: **service repos version independently and declare a minimum-Core
compatibility floor; not lockstep.** This is built and shipped in `wylde-lifecycle` (not a
post-0.2 spec) — code + tests below.

**What it is:** a service manifest declares the oldest Wylde Core it is compatible with. Core
**refuses to spawn** a service whose floor exceeds the running Core and **surfaces the reason to the
user** — never a silent skip, because a silently-absent feature is exactly the "the panel is there
but does nothing" failure class.

**Manifest field** — `min_core` in each service's folder `manifest.json`, a plain version string:

```json
{
  "name": "wylde-organize",
  "enabled": true,
  "version": "1.4.0",
  "min_core": "0.2.0"
}
```

- Optional. Absent/empty ⇒ no floor ⇒ Core spawns it (back-compatible with today's manifests).
- (Field name is `min_core`, not `min_core_version` — the value is obviously a version; keeping it
  short matches the manifest's other keys.)

**Comparison semantics** (`registry::check_core_floor`, unit-tested): compatible iff Core's
**release** version (`major.minor.patch`, with any pre-release/build identifier stripped) `>=` the
floor. Core's version is `env!("CARGO_PKG_VERSION")` of the lifecycle crate (= the workspace version).

- **Pre-release rule — decided:** the pre-release identifier is **stripped from Core** before
  comparing, so a Core pre-release on the run-up to X (`0.2.0-alpha.3`) **satisfies** a floor of X
  (`0.2.0`). Rationale: during the experimental line Core ships pre-releases; blocking every service
  on every pre-release would be useless. Trade-off (accepted, solo dev): an early `0.2.0-alpha.1`
  might not yet carry all of `0.2.0`'s surface. Floors should still target a **released** version.
- **Malformed floor ⇒ fail-closed** (`CoreCompat::BadFloor`): a manifest typo is treated as
  incompatible with a "fix the manifest" reason, so a broken declaration surfaces loudly instead of
  silently disabling the gate.

**Reverse direction (a MAX) — assessed, not needed.** A floor alone suffices. The failure a max
would guard (Core got too new and broke the service) is, for a solo dev who ships Core and the
services from the same release process, caught at the break — you bump Core and move the service's
floor together. A `max_core` would also invite the "pinned below current, silently disabled" trap.
The field is **forward-compatible**: if a genuine upper bound is ever needed (an unmaintained
third-party service), add a separate `core` field carrying a full semver `VersionReq`
(`>=0.2.0, <0.4.0`) without changing `min_core`'s meaning. Not built now.

**What Core does when the floor isn't met** (`state::services::start_discovered`): logs a **loud
`tracing::error!`** naming the service, its floor, and Core's version, then **skips the spawn** and
continues (`Ok(())`, non-fatal — Core is unaffected). Independently, `registry::build_info` marks
the service `state = "incompatible"` with the reason, so it's carried on `service.list`, and
`service.health` short-circuits to a structured `{ ok:false, incompatible:true, reason }` reply.

**What the user sees (GUI):** the panel that lists the service in `required_services` renders its
`ServiceUnavailable` stub — but now with the **specific reason** ("`wylde-organize` needs Wylde Core
>= 0.3.0, but this Core is 0.2.1 — update Wylde") and **no futile "Start" button** (starting can't
fix an incompatibility; the fix is updating Wylde). An ordinary down service still shows "not
running" + Start. Threaded through `nav::service_health_body_is_ready`/`service_health_reason` →
`NavModel` → `SlotState::ServiceUnavailable { reasons }` → `slot::render_unavailable`.

**Where it lives in code** (shipped in this work, with tests):

- `wylde-lifecycle/src/registry.rs`: `core_version()`, `CoreCompat`, `check_core_floor()`;
  `DiscoveredService.min_core` (read in `discovered_bucket_services_in`); `ServiceInfo.incompatible_reason`
  (computed in `build_info`). Tests: `check_core_floor_semantics`, `discovered_bucket_services_reads_min_core_floor`, `core_version_is_valid_semver`.
- `wylde-lifecycle/src/state/services.rs`: the refusal in `start_discovered`. Test:
  `start_discovered_refuses_incompatible_min_core`.
- `wylde-lifecycle/src/control.rs`: `service.health` short-circuit + `service.list` `incompatible_reason` field.
- `Core/GUI/Shell/src/{nav,slot,shell_root}.rs`: the GUI reason display. Tests in `nav::tests`.
- `semver = "1"` added to `wylde-lifecycle/Cargo.toml`.
- Applied as a live example to `Services/wylde-images/manifest.json` (`"min_core": "0.1.0"`, compatible).

**For the two external service repos** (`wylde-organize`, `wylde-tabulate`) — add to each repo's
folder `manifest.json` when their repos are next touched (they aren't in Core's tree):

```jsonc
// wylde-organize/manifest.json and wylde-tabulate/manifest.json
{ "name": "wylde-organize", "enabled": true, "min_core": "0.2.0" }
```

Set the floor to whichever Core release first carries the IPC/API surface the service depends on
(≥ `0.2.0`, since the panels are post-0.2).

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
