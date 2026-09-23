<!--
Wylde PR. Target `develop` (the experimental trunk), NOT `main`.
`main` only ever receives gated promotions from `develop` — see
docs/branch-and-release-policy.md. A PR into `main` is almost always a mistake
(the exception is a hotfix, which is branched from `main` on purpose).
-->

## What & why

<!-- One or two sentences: what this changes and the problem it solves.
     Link the issue it closes: "Closes #123". -->

## Type

<!-- Match your branch prefix / Conventional Commit type. -->

- [ ] `feat` — user-facing feature
- [ ] `fix` — bug fix
- [ ] `chore` — tooling / deps / repo mechanics (no user-facing behaviour)
- [ ] `docs` / `test` / `refactor` / `perf`
- [ ] **Breaking change** (a `!` commit / `BREAKING CHANGE`) — describe the migration below

## Checklist

Items marked **(CI)** are enforced by a required status check — a ticked box over a red check does
**not** merge (see `docs/enforcement-matrix.md`). The rest are human judgment.

- [ ] **(CI)** Targets **`develop`**, not `main`.
- [ ] **(CI)** Branch is named `feat|fix|chore|docs|test|refactor|perf/<slug>` and is short-lived.
- [ ] **(CI)** Commits follow Conventional Commits (`type(scope): subject`) — escape: `skip-commit-lint` label.
- [ ] **(CI)** Backend/GUI/tools build + `rust/` tests pass (G1–G3).
- [ ] **(CI)** cargo-deny is happy, or any new advisory is documented in `deny.toml` with a reason + review date (G5).
- [ ] **(CI)** If the version changed, **both** workspace roots moved together (G7).
- [ ] **(CI)** CHANGELOG `[Unreleased]` updated for any user-facing change — escape: `skip-changelog` label. (`tools/changelog-draft.sh` seeds it.)
- [ ] **(CI)** References a tracking issue (`Closes #123`, `Refs #123`, or a bare `#123` in the title, body, or a commit) — escape: `no-issue` label for a deliberate no-issue change. (Dependabot + the `develop`→`main` promotion are exempt automatically.)
- [ ] Docs updated if behaviour, config, or a public interface changed. *(human judgment)*

## Verification

<!-- How did you confirm this works? Unit tests are necessary but not sufficient —
     Wylde's release history shows green tests can still ship a broken running system.
     If this touches a runtime surface, say what you drove and what you observed.
     (Full launch-and-verify L1–L7 runs at release time, not per-PR — see the release checklist.) -->

## Notes for the reviewer

<!-- Anything non-obvious: a risky area, a follow-up you deferred, a decision you want a second look at. -->
