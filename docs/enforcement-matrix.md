# Wylde — Enforcement Matrix

**The acceptance test for the whole hygiene design.** Alpha shipped broken repeatedly because
**nothing STOPPED it** — a norm is not a gate. So every policy here must answer one question:
**what enforces it, and what does it block?** Anything whose honest answer is "nothing, it's a
convention" is either given a real mechanism below, or explicitly justified as unenforceable-but-kept
(and you can overrule those).

Preference order for mechanisms: **GitHub-native** (branch/tag rulesets, required status checks,
milestones, Dependabot) > **CI job that fails the build** > **runtime enforcement in the product** >
**bespoke script** (only where nothing native fits, always justified).

---

## The matrix

| # | Policy / artifact | Enforcement mechanism | What it BLOCKS | Where configured | Live? |
|---|---|---|---|---|---|
| 1 | **No direct pushes / force-push / deletion of `main` (stable)** | GitHub **branch ruleset** (`protect-main`) | pushing/force-pushing/deleting stable; the exact "experimental lands on stable" bug | `.github/rulesets/protect-main.json` → apply via `gh` (Aaron) | ⏳ apply |
| 2 | **Green CI before merge** | **Required status checks** in the ruleset (`strict` = branch must be up to date) | merging a PR whose build/tests/version-check are red or stale | rulesets (main + develop) | ⏳ apply |
| 3 | **Backend builds + 1167 tests pass** | CI job `backend (rust/) build + test` | a PR/tag that doesn't compile or fails tests | `.github/workflows/ci.yml` + `release.yml` | ✅ live (CI) / ⏳ required |
| 4 | **GUI builds** | CI job `gui (Core/GUI/) build` | a GUI-breaking change | `ci.yml`, `release.yml` | ✅ / ⏳ required |
| 4b | **Every GUI page loads + detects its error state (L7 panel-walk)** | CI job `gui panel-walk (L7)` — headless windowed `#[gpui::test]`s mount all 9 panels + the Workspaces subtabs under 4 backend conditions (healthy / down / error-envelope / empty) and assert each loads without panic and isn't in a wrong/stuck error state | a page that panics, stuck-loads, or mis-detects "down" (esp. **the daemon-down case** — a panel that crashes when a service isn't running); the "shipped a GUI page that never loads" class the L2/L3 smoke misses | `ci.yml` (`gui panel-walk (L7)` job) + `Core/GUI/.cargo/config.toml` (`panel-walk` alias) | ✅ **live (CI)** — the windowed tests **do** run headless on the CI runner (gpui mock `TestPlatform`); resolves the §3g "CI-vs-local" open question in the affirmative |
| 5 | **Version consistency across the two workspaces (G7)** | CI job `version consistency (G7)` — **fails**, not warns; `wylde-release` refuses to publish on failure | a release where `rust/` and `Core/GUI/` versions disagree, or a tag ≠ the stamped version | `tools/check-versions.sh` + `ci.yml`/`release.yml` | ✅ live |
| 6 | **PR targets `develop`, not `main`** | CI job `branch target + name` (fails if base=main and head∉{develop, hotfix/*, fix/*}) | accidentally PRing experimental work straight at stable | `.github/workflows/pr-checks.yml` | ✅ live |
| 7 | **Branch naming convention** | same `branch target + name` job (head must match `feat|fix|chore|docs|test|refactor|perf|hotfix/*`) | the `chore/`-vs-`feat/` drift + junk branch names | `pr-checks.yml` | ✅ live |
| 8 | **Conventional Commits** | CI job `conventional commits` (lints every non-merge commit subject) | commits the changelog/branch scheme can't parse; escape via `skip-commit-lint` label | `pr-checks.yml` | ✅ live |
| 9 | **CHANGELOG updated for user-facing changes** | CI job `changelog updated` — fails a PR touching `rust/`/`Core/` product source without a `CHANGELOG.md` entry; escape via `skip-changelog` label | silent, undocumented user-facing changes (the changelog rotting) | `pr-checks.yml` | ✅ live |
| 10 | **Clippy `-D warnings` (G4)** | CI job `clippy (G4) + fmt (G6)` — `cargo clippy --workspace --all-targets --locked -- -D warnings` on each CI-built workspace | a warning landing anywhere in rust/, Core/GUI/, or the two tools/ crates | `ci.yml` (`lint` job) | ✅ **live (CI)** — armed once the tree went clippy-clean (issue #32); the `voice-npu-spike` workspace has no build/test job, so it is fmt'd but not gated |
| 11 | **`cargo fmt --check` (G6)** | CI job `clippy (G4) + fmt (G6)` — `cargo fmt --all -- --check` on each CI-built workspace | unformatted code in rust/, Core/GUI/, or the two tools/ crates | `ci.yml` (`lint` job) | ✅ **live (CI)** — armed after a tree-wide `cargo fmt --all` landed as its own `chore(fmt)` commit (issue #32) |
| 12 | **No vulnerable dependencies** | CI job `cargo-deny (advisories)` (PR + weekly cron) + Dependabot PRs | a bump/commit pulling a crate with an advisory | `security-audit.yml`, `dependabot.yml` | ✅ live / ⏳ mark required |
| 13 | **Only maintainer-blessed `v*` tags; release tags immutable** | GitHub **tag ruleset** (`protect-version-tags`) — blocks deletion + moving a `v*` tag | deleting/re-pointing a published version tag (which would corrupt the updater's version history) | `.github/rulesets/protect-tags.json` | ⏳ apply |
| 14 | **Release actually RAN the live preflight (L1–L7)** | **`wylde-release` refuses to `publish` without a green, launch-verified preflight receipt** for the exact commit (§Preflight receipt) + `release.yml` re-runs the CI-verifiable gates on the tag | shipping a build whose running system was never verified — *the actual "shipped broken" failure* | `release.yml` (CI subset) + `wylde-release preflight/publish` | ✅ **receipt + L2/L3 launch gate built** — `preflight` writes a commit-bound receipt; `publish` refuses without a green, current one (rejects stale-commit / dirty-tree / wrong-version / **not-launch-verified**). Binds **G7 + benchmark gate (L5)** (+ optional L1-lite build) and, via **`preflight --launch`**, the **L2 cold-start + L3 service-health** launch-and-verify checks (each folded into the receipt's `gates` map, fail-closed; `launch_verified` gates publish). Remaining L4/L6 stay manual; L7 has its own CI job (row 4b). |
| 15 | **Service `min_core` compatibility floor** | **Runtime**: Core's loader refuses to spawn an incompatible sibling + the GUI shows why. **CI (service repos)**: a manifest-lint job in each service repo | an incompatible service booting into a silent dead panel | `wylde-lifecycle` (shipped); service-repo CI (spec) | ✅ runtime / ⏳ service-repo CI |
| 16 | **Issue reports are structured** | GitHub **issue forms** (`blank_issues_enabled: false`) — GitHub enforces the form | free-text issues with no repro/version | `.github/ISSUE_TEMPLATE/` | ✅ live once on the **default branch** (see Aaron-action A) |
| 17 | **Security disclosures are private** | GitHub **private vulnerability reporting** + `SECURITY.md` + issue-template `config.yml` contact link | a public 0-day issue | `SECURITY.md`, `.github/ISSUE_TEMPLATE/config.yml` + repo Security setting | ✅ files / ⏳ enable "Private vulnerability reporting" (Aaron-action) |
| 18 | **Dependencies stay current** | **Dependabot** (grouped weekly PRs) — native | dependency rot piling into a scary backlog | `.github/dependabot.yml` | ✅ live |
| 19 | **PR-checklist items generally** | Converted to jobs 6–9 where automatable; the rest is the template | — | `.github/PULL_REQUEST_TEMPLATE.md` | ✅ (automatable items are now checks, not checkboxes) |
| 20 | **0.2 can't ship with open prerequisite milestones** | `tools/check_release_milestones.py` — reads `tools/release-gates.json` (declared prerequisites) + an anti-drift cross-check (any `0.2`-prefixed milestone not listed fails); **fail-closed**; `--force "reason"` override recorded. Runs in `release.yml` (CI-visible) and is **called by `wylde-release publish`** (binding — spec) | tagging/publishing 0.2 while milestone `(1) gate & hygiene` or `(2) verified build` has open issues | `release.yml` + `tools/release-gates.json` | ✅ CI / ⏳ wylde-release wiring |
| 21 | **No performance/quality regression past a threshold (L5 benchmark guardrail)** | **`wylde-release bench`** runs the eval harnesses against live Ollama, medians over reps, and compares each metric to the committed baseline (`benchmarks/baselines/wylde-benchmarks.json`) with a **noise-calibrated per-metric band** — **fails** on a real regression, **warns** on a small one, **flags** an improvement to re-record. Run inside `preflight`, so it rides the receipt gate (row 14). | a silent latency/success/token regression shipping — "planning got 2× slower and nothing noticed" | `tools/wylde-release` (`bench`) + `benchmarks/` | ✅ **built + baselined** (reasoning fast/think arms recorded); retrieval invariants wired, baseline pending the T0.5 re-index |
| 22 | **Baselines can't drift upward silently** | The baseline moves **only** on an explicit `wylde-release bench --accept-baseline`; a compare run never rewrites it, and re-recording **preserves tuned bands** (values move, policy doesn't). A regression can become the new normal only on purpose. | a bad number quietly becoming the accepted baseline | `tools/wylde-release` | ✅ built |
| 23 | **Benchmark trend is retained, not just last-compare** | Every `bench`/`preflight` run appends a JSON line (timestamp, commit, green?, all values) to `outputs/benchmarks/history.jsonl` in the **private planning repo** (junctioned in) — drift over time, not just pass/fail. Silent no-op when the junction isn't mounted. | losing the "benchmarks to work from" record | `tools/wylde-release` + planning repo | ✅ built |

**Legend:** ✅ live = active now on the trunk. ⏳ apply/build/required = needs a one-time Aaron action
(below) or a tracked T0.1 build item.

---

## Planning repo (`PeopleWonder/wylde-planning`, private) — its own gates

A private repo still runs Actions, and the whole point of the planning repo is that plans *know*
whether they're current (a stale plan you act on is worse than none — this bit us repeatedly). So its
lifecycle tracking is CI-enforced too: `.github/workflows/plans-check.yml` → `tools/check_plans.py`
(green on GitHub).

| # | Policy | Mechanism | What it BLOCKS | Live? |
|---|---|---|---|---|
| P1 | Every plan declares a valid lifecycle | `check_plans.py` — frontmatter present + `status` ∈ {active,deferred,done,superseded,legacy} | a plan silently rotting with no/unknown status | ✅ |
| P2 | No dangling "replaced by" | `check_plans.py` — `superseded` needs a `superseded_by` that resolves to a real plan | a supersession pointer to nowhere | ✅ |
| P3 | Real dates | `check_plans.py` — `created`/`last_reviewed` must be `YYYY-MM-DD` | undated/guessed plans | ✅ |
| P4 | Indexes are generated, never hand-edited | `check_plans.py` — regenerate `INDEX.md`/`OUTPUTS_INDEX.md` in-memory and diff (EOL-insensitive) | a hand-edited/stale index — the exact rot this repo exists to prevent | ✅ |
| P5 | Surface stale `active` plans | `check_plans.py` — **warn** when an `active` doc is >90d unreviewed | — (warn only; a machine can't force a re-read) | ✅ |

Negative-tested: a bogus status, a dangling `superseded_by`, and a hand-edited index each fail the
check. **Optional Aaron-action:** add a branch ruleset requiring the "Planning check" status on the
planning repo's `main` (same pattern as Core's rulesets) if you want it to *block* direct pushes
rather than just go red.

**Project ↔ plan link — not machine-enforced, justified.** A plan's `tracks:` field lists the issues
its work lives in; the Project item links back. The rule — **Project = source of truth for STATUS,
plan = source of truth for WHY; the Project wins on conflict** — is a convention a human upholds (no
check can know a plan *should* be `done` because its issue closed). Kept because it's the load-bearing
discipline; the P5 staleness warn is the closest automated nudge.

---

## Deliberately NOT machine-enforced — justified (overrule any of these)

An honest "nothing enforces this" for each, and why it's kept anyway:

- **Roadmap staying current.** *Not machine-enforceable* — "is this doc fresh?" is a content judgment
  no check can make. Kept because it's cheap and load-bearing; enforced *by proxy* two ways: (a) it's
  a line in `docs/release-checklist.md`, so it rides the preflight-receipt gate at release time
  (job 14); (b) actionable work lives in **GitHub Issues under the `0.2` milestone**, which GitHub
  tracks natively — the milestone is the enforced half; the prose roadmap is the narrative half. If it
  ever rots anyway, cut the prose and let the milestone be canon.
- **"Docs updated if behaviour changed" (PR checklist).** *Not reliably automatable* — whether a
  behaviour change needs a doc edit is subjective. Kept as a checklist item; the CHANGELOG half of it
  *is* enforced (job 9).
- **PR-template checkboxes that duplicate a check** (build passes, version bumped, changelog). Kept as
  a human-readable summary, but the **check is the source of truth** — a ticked box over a red check
  doesn't merge (jobs 2–9).
- **Required PR approvals, CODEOWNERS, CODE_OF_CONDUCT, signed commits, merge-queue.** *Enforceable but
  ceremony at solo scale* — each blocks nothing useful with one maintainer and adds friction. Full
  reasoning in `docs/branch-and-release-policy.md §6.1`. Adopt when a second contributor appears.

If a row's mechanism is "nothing" and it's not in this justified list, it should be **cut** — say so.

---

## Preflight receipt — the one legitimately-bespoke enforcement (✅ BUILT, T0.1)

CI structurally **cannot** run the launch-and-verify smoke (L1–L7): no GPU, Ollama, Memgraph, or
desktop session on a GitHub runner. That's exactly the surface where "shipped broken" happened, so it
needs a real gate — and the only place that gate can live is the local release tool. **Built** in
`tools/wylde-release/` (`preflight.rs` + `receipt.rs`, unit-tested):

- **`wylde-release preflight`** runs the gates it can on the release machine and writes a **receipt**
  (`preflight-receipt.json`, gitignored): `{ schema, commit, git_dirty, version, timestamp, host,
  gates: {name: pass|fail|skipped}, benchmarks: {metric: delta}, warnings, all_green, launch_verified }`.
  It binds **G7 version-consistency + the benchmark gate (L5)**, an optional **L1-lite** artifact build
  (`--build`), and — with **`--launch`** — the **L2 cold-start + L3 service-health** launch-and-verify
  checks, each folded into the `gates` map under an `l2.*`/`l3.*` key (fail-closed). `launch_verified`
  is `true` only when every launch check passed. The remaining L4/L6 stay manual; L7 is its own CI job.
- **`wylde-release publish` refuses** unless a receipt exists whose `commit` == the commit being
  published, `git_dirty == false`, `all_green == true`, **`launch_verified == true`**, and `version` ==
  the tag. No green, launch-verified receipt for *this* commit → no publish. This makes "the live
  system was launched and verified" a **precondition of shipping**, not a habit. A deliberate, loud
  `--no-preflight-receipt` escape hatch exists for emergencies.
- **Trust model — deliberately not cryptographic.** A JSON file is forgeable, but for a solo dev the
  threat is *forgetting to run the checks*, not fraud — so the complexity budget goes on the one
  property that prevents the real failure: **binding to the exact commit** (a stale or dirty-tree
  receipt can't validate a new build), not signatures. Forward-compatible with attaching the receipt
  to the GitHub Release + signing its hash if a second maintainer ever makes forgery a real threat.
- Backstop: `release.yml` (already added) re-runs the CI-verifiable gates (G1/G2/G7) on the tag, so
  even the machine-checkable subset is enforced at tag time independent of the local tool.

This is the single most important studio-grade build item (roadmap **T0.1**) — the gate that stops
the *next* broken release.

---

## Milestone gate — making the 0.2 milestone binding, not decorative

GitHub milestones have no native dependency mechanism (a milestone is just a bucket of issues with a
%). So "0.2 ships only after its prerequisite milestones complete" is built in two layers:

**Visibility — the "Ship 0.2" tracking issue ([#41](https://github.com/PeopleWonder/wylde/issues/41)).**
A parent issue whose **sub-issues** are the 12 prerequisite issues (all of `(1) gate & hygiene` +
`(2) verified build`). GitHub renders the completion % automatically and **a parent can't close while
a child is open**, so "is 0.2 ready?" becomes a number, not a judgement. The 0.2 definition-of-done
is in its body. (Sub-issues work with the current `repo` scope — verified.)

**Enforcement — `tools/check_release_milestones.py` (fail-closed).** Given a version tag it reads
`tools/release-gates.json` and refuses if any prerequisite milestone still has open issues.

- **Ordering source — an explicit config, not title-parsing.** `release-gates.json` declares, per
  release tag, the milestones that must be complete first. Chosen over parsing title prefixes because
  it's explicit and reviewable in a PR. **Anti-drift cross-check:** the script *also* fails if any
  milestone whose title starts with the release's `prefix` (`0.2`) is missing from the list — so
  adding a new 0.2 milestone and forgetting to declare it can't silently pass. Config declares intent;
  the cross-check enforces completeness. Neither can drift silently.
- **Fail-closed:** any API error, missing config entry, or renamed/empty required milestone → refuse.
  Never assumes ready.
- **Override:** `--force "reason"` opens it deliberately (0.2 ships on the maintainer's say-so); a
  `--force` with no reason is rejected, and the reason is printed for the receipt to record.
- **Live now** in `release.yml` (the `milestone-gate` job, fail-red on a `v*` tag). Verified: it
  refuses `v0.2.0` today because `(1)` has 5 open and `(2)` has 7 open issues.

**The binding wiring (spec — deliberately NOT implemented here).** `wylde-release`'s source is under
active development in another session, so to avoid colliding with its WIP the one-line integration is
specified rather than written: **`wylde-release publish` must call**
`python tools/check_release_milestones.py <version>` **before tagging and refuse on a non-zero exit**,
threading `--force "<reason>"` through when the operator overrides and recording that reason in the
preflight receipt. That makes the *local publish* refuse (binding), with `release.yml` as the
CI-visible backstop. Tracked in the ship issues (#33 preflight tool, #38 publish).

**Auth at release time:** the script uses `gh api`, so `gh` must be authenticated locally, or
`GH_TOKEN` set in CI (`release.yml` passes `${{ github.token }}`). Documented in the script header.

---

## Aaron-actions — exact commands (one-time; needs your GitHub auth)

`gh` is authenticated (`repo`, `workflow` scopes) and the repo is public, so rulesets are available.
**Do these in order** — rulesets require the checks to already exist as *observed* runs, and community
health files (templates, SECURITY, CONTRIBUTING) go live from the **default branch**.

**A. Rename the trunk and make `develop` the default** (activates the health files + points PRs at
develop). *After this, the issue templates / SECURITY / CONTRIBUTING are live on GitHub.*

```bash
# Rename the trunk (preserves history, redirects open PRs). UI: Settings → Branches → rename.
gh api -X POST repos/PeopleWonder/wylde/branches/feat/thought-bubble-system/rename -f new_name=develop
# Make develop the default branch — this is what makes .github/ community files + issue forms live,
# and defaults new PRs to target develop. (Correction to the earlier "keep main default": the
# enforcement + health-file-liveness argument wins.)
gh api -X PATCH repos/PeopleWonder/wylde -f default_branch=develop
```

**B. Create the escape-hatch labels + the release milestone** (the PR checks reference these labels;
the milestone is the enforced half of "roadmap current").

```bash
gh label create skip-changelog   --color ededed --description "PR intentionally needs no CHANGELOG entry"
gh label create skip-commit-lint --color ededed --description "PR intentionally skips conventional-commit lint"
gh api -X POST repos/PeopleWonder/wylde/milestones -f title="0.2" -f description="First stable release gate (opened on the maintainer's say-so)"
```

**C. Turn on private vulnerability reporting** (backs `SECURITY.md`).

```bash
gh api -X PATCH repos/PeopleWonder/wylde -F security_and_analysis='{"secret_scanning_push_protection":{"status":"enabled"}}' || true
# Private vulnerability reporting (no direct REST field on all plans) — enable in the UI:
#   Settings → Code security and analysis → Private vulnerability reporting → Enable.
```

**D. Let CI run once on `develop`** (open any small PR, or push) so the check names below are
*observed* by GitHub. Then apply the rulesets:

```bash
gh api -X POST repos/PeopleWonder/wylde/rulesets --input .github/rulesets/protect-develop.json
gh api -X POST repos/PeopleWonder/wylde/rulesets --input .github/rulesets/protect-main.json
gh api -X POST repos/PeopleWonder/wylde/rulesets --input .github/rulesets/protect-tags.json
```

> **Required-check name caveat.** The `required_status_checks` contexts in the ruleset JSON must match
> the workflow **job names** exactly. The seven listed (`backend (rust/) build + test`, `gui (Core/GUI/)
> build`, `tools build`, `version consistency (G7)`, `branch target + name`, `conventional commits`,
> `changelog updated`) are stable. **`cargo-deny` is a matrix job**, so its contexts are environment-
> specific (`cargo-deny (advisories) (rust/Cargo.toml)` + `(Core/GUI/Cargo.toml)`) — add those to the
> required list via the UI (Settings → Rules → protect-develop → Require status checks → pick from the
> observed list) after the first run, rather than guessing the exact string here.

**E. (Optional, when a second contributor appears)** flip `required_approving_review_count` to 1 in the
ruleset JSONs and add a `CODEOWNERS`. Not before — see §6.1.

**What stays yours to run, not automatable:** the `wylde-release preflight`/`publish` receipt gate
(build it — T0.1), and the private planning repo for `docs/plans/` + `outputs/` (see the roadmap
"Where things live"). Both are flagged in the roadmap.
