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
| 5 | **Version consistency across the two workspaces (G7)** | CI job `version consistency (G7)` — **fails**, not warns; `wylde-release` refuses to publish on failure | a release where `rust/` and `Core/GUI/` versions disagree, or a tag ≠ the stamped version | `tools/check-versions.sh` + `ci.yml`/`release.yml` | ✅ live |
| 6 | **PR targets `develop`, not `main`** | CI job `branch target + name` (fails if base=main and head∉{develop, hotfix/*, fix/*}) | accidentally PRing experimental work straight at stable | `.github/workflows/pr-checks.yml` | ✅ live |
| 7 | **Branch naming convention** | same `branch target + name` job (head must match `feat|fix|chore|docs|test|refactor|perf|hotfix/*`) | the `chore/`-vs-`feat/` drift + junk branch names | `pr-checks.yml` | ✅ live |
| 8 | **Conventional Commits** | CI job `conventional commits` (lints every non-merge commit subject) | commits the changelog/branch scheme can't parse; escape via `skip-commit-lint` label | `pr-checks.yml` | ✅ live |
| 9 | **CHANGELOG updated for user-facing changes** | CI job `changelog updated` — fails a PR touching `rust/`/`Core/` product source without a `CHANGELOG.md` entry; escape via `skip-changelog` label | silent, undocumented user-facing changes (the changelog rotting) | `pr-checks.yml` | ✅ live |
| 10 | **Clippy `-D warnings` (G4)** | CI job (staged) | warnings landing | `ci.yml` (commented stub) | ⏳ blocked — needs a tree cleanup first (enabling on a dirty tree red-walls CI); tracked as a Tier-1 item |
| 11 | **`cargo fmt --check` (G6)** | CI job (staged) | unformatted code | `ci.yml` (commented stub) | ⏳ blocked — tree is **not** fmt-clean today (verified); run `cargo fmt` as its own slice, then enable |
| 12 | **No vulnerable dependencies** | CI job `cargo-deny (advisories)` (PR + weekly cron) + Dependabot PRs | a bump/commit pulling a crate with an advisory | `security-audit.yml`, `dependabot.yml` | ✅ live / ⏳ mark required |
| 13 | **Only maintainer-blessed `v*` tags; release tags immutable** | GitHub **tag ruleset** (`protect-version-tags`) — blocks deletion + moving a `v*` tag | deleting/re-pointing a published version tag (which would corrupt the updater's version history) | `.github/rulesets/protect-tags.json` | ⏳ apply |
| 14 | **Release actually RAN the live preflight (L1–L7)** | **`wylde-release` refuses to `publish` without a green preflight receipt** for the exact commit (spec §Preflight receipt) + `release.yml` re-runs the CI-verifiable gates on the tag | shipping a build whose running system was never verified — *the actual "shipped broken" failure* | `release.yml` (CI subset) + `wylde-release preflight/publish` (⏳ T0.1) | ⏳ build (T0.1) |
| 15 | **Service `min_core` compatibility floor** | **Runtime**: Core's loader refuses to spawn an incompatible sibling + the GUI shows why. **CI (service repos)**: a manifest-lint job in each service repo | an incompatible service booting into a silent dead panel | `wylde-lifecycle` (shipped); service-repo CI (spec) | ✅ runtime / ⏳ service-repo CI |
| 16 | **Issue reports are structured** | GitHub **issue forms** (`blank_issues_enabled: false`) — GitHub enforces the form | free-text issues with no repro/version | `.github/ISSUE_TEMPLATE/` | ✅ live once on the **default branch** (see Aaron-action A) |
| 17 | **Security disclosures are private** | GitHub **private vulnerability reporting** + `SECURITY.md` + issue-template `config.yml` contact link | a public 0-day issue | `SECURITY.md`, `.github/ISSUE_TEMPLATE/config.yml` + repo Security setting | ✅ files / ⏳ enable "Private vulnerability reporting" (Aaron-action) |
| 18 | **Dependencies stay current** | **Dependabot** (grouped weekly PRs) — native | dependency rot piling into a scary backlog | `.github/dependabot.yml` | ✅ live |
| 19 | **PR-checklist items generally** | Converted to jobs 6–9 where automatable; the rest is the template | — | `.github/PULL_REQUEST_TEMPLATE.md` | ✅ (automatable items are now checks, not checkboxes) |

**Legend:** ✅ live = active now on the trunk. ⏳ apply/build/required = needs a one-time Aaron action
(below) or a tracked T0.1 build item.

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

## Preflight receipt — the one legitimately-bespoke enforcement (spec, T0.1)

CI structurally **cannot** run the launch-and-verify smoke (L1–L7): no GPU, Ollama, Memgraph, or
desktop session on a GitHub runner. That's exactly the surface where "shipped broken" happened, so it
needs a real gate — and the only place that gate can live is the local release tool. Design:

- **`wylde-release preflight`** runs L1–L7 on the release machine and writes a **receipt**
  (`preflight-receipt.json`): `{ commit, version, timestamp, gates: {L1..L7: pass|fail}, all_green,
  host }`.
- **`wylde-release publish` refuses** unless a receipt exists whose `commit` == the commit being
  published, `all_green == true`, and `version` == the tag. No green receipt for *this* commit → no
  publish. This makes "the live system was verified" a **precondition of shipping**, not a habit.
- The receipt is attached to the GitHub Release (auditable: anyone can see the build was preflighted).
- Backstop: `release.yml` (already added) re-runs the CI-verifiable gates (G1/G2/G7) on the tag, so
  even the machine-checkable subset is enforced at tag time independent of the local tool.

This is the single most important studio-grade build item (roadmap **T0.1**) — the gate that stops
the *next* broken release.

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
