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

- [ ] Targets **`develop`**, not `main`.
- [ ] Branch is named `feat|fix|chore|docs|test|refactor|perf/<slug>` and is short-lived.
- [ ] Commits follow Conventional Commits (`type(scope): subject`).
- [ ] `cargo build --workspace --locked` passes in each affected workspace (G1–G3).
- [ ] `cargo test --workspace --locked` passes for `rust/` if backend code changed (G1).
- [ ] cargo-deny is happy, or any new advisory is documented in `deny.toml` with a reason + review date (G5).
- [ ] If the version changed, **both** workspace roots moved together (`tools/check-versions.sh` green — G7).
- [ ] CHANGELOG `[Unreleased]` updated for any user-facing change (Keep-a-Changelog; `tools/changelog-draft.sh` can seed it).
- [ ] Docs updated if behaviour, config, or a public interface changed.

## Verification

<!-- How did you confirm this works? Unit tests are necessary but not sufficient —
     Wylde's release history shows green tests can still ship a broken running system.
     If this touches a runtime surface, say what you drove and what you observed.
     (Full launch-and-verify L1–L7 runs at release time, not per-PR — see the release checklist.) -->

## Notes for the reviewer

<!-- Anything non-obvious: a risky area, a follow-up you deferred, a decision you want a second look at. -->
