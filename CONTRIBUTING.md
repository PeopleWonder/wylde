# Contributing to Wylde

Wylde is a local-first personal AI system, currently built by a very small team (effectively one
maintainer). Contributions are welcome, but the bar is real: Wylde ships a self-updating desktop app
to real machines, so "don't break stable" is the governing principle. This document is how a change
gets from your machine to a release safely.

## TL;DR

1. Branch off **`develop`** (the experimental trunk), never `main`.
2. Name the branch `feat|fix|chore|docs|test|refactor|perf/<slug>`.
3. Write [Conventional Commits](#commit-messages): `type(scope): subject`.
4. Open a PR into **`develop`**. Fill in the PR checklist. Get CI green.
5. It merges `--no-ff`; your branch is deleted; the change ships to the Beta channel with the next
   `0.1.x`, and to the Stable channel when the maintainer gates the next stable release.

## The branch model (read this)

The full policy is in **[`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md)** —
please read it. The short version:

- **`main`** = stable. Only proven, gated releases. This is what the updater's Stable channel serves.
  **Do not target `main` in a PR** (the one exception is a hotfix, which is branched *from* `main`).
- **`develop`** = the experimental integration trunk. Everything lands here first, proves out on the
  Beta channel, and is promoted to `main` only on the maintainer's explicit say-so.
- **Topic branches** are short-lived, branched off `develop`, merged back `--no-ff`, then deleted.

## Commit messages

Wylde uses **Conventional Commits** — the commit history already largely follows this, and it drives
the changelog and the branch-naming scheme.

```
type(scope): subject

[optional body]
[optional BREAKING CHANGE: …  footer]
```

Types: `feat`, `fix`, `chore`, `docs`, `test`, `refactor`, `perf`, `ci`, `build`. A breaking change
is `type!:` (e.g. `feat!:`) or a `BREAKING CHANGE:` footer.

Rule of thumb for `feat` vs `chore`: if a *user* would notice it in the app, it's `feat`; if only a
*developer* would, it's `chore`.

## Changelog

Wylde keeps a hand-curated **[Keep-a-Changelog](https://keepachangelog.com)** file
(`CHANGELOG.md`) — it's deliberately richer than an auto-generated bullet list. For any user-facing
change, add an entry under `[Unreleased]` in the matching section (Added / Changed / Fixed /
Deprecated / Removed / Security). You can seed a draft from your commits with
`tools/changelog-draft.sh`, then edit it into narrative form. Docs/chore/test-only changes usually
don't need a changelog entry.

## Before you open a PR

- `cargo build --workspace --locked` in each workspace you touched
  (`rust/`, `Core/GUI/`, `tools/xtask`, `tools/wylde-release`).
- `cargo test --workspace --locked` in `rust/` if you changed backend code.
- If you changed the version, bump **both** workspace roots together and run
  `tools/check-versions.sh` (the CI `version-consistency` gate, G7).
- Green tests are necessary but **not sufficient** — Wylde's own history is that a fully green suite
  shipped a broken running system more than once. If your change has a runtime surface, drive it and
  say what you observed in the PR's Verification section. Full launch-and-verify (L1–L7) runs at
  release time; see [`docs/release-checklist.md`](docs/release-checklist.md).

## Dependencies & security

- Dependency bumps flow through Dependabot as grouped PRs (see `.github/dependabot.yml`). CI +
  cargo-deny gate them, so a bump that breaks the build or pulls a vulnerable crate turns red.
- Accepting an unavoidable advisory = adding it to the relevant `deny.toml` `ignore` list **with a
  reason and a review date** (see `docs/security/dependency-hygiene-policy.md`). Note the `gpui`
  git-rev pin caveat documented there.
- Found a security vulnerability? **Do not open a public issue** — see
  [`SECURITY.md`](SECURITY.md).

## Filing issues

Use the **[bug report or feature request templates](.github/ISSUE_TEMPLATE/)**. Search first — Wylde
has an opinionated roadmap, and a good idea may still be deferred if it's off the current line.

## Where things live

- **Code, changelog, security, this guide** — this repo.
- **Bugs & feature requests** — [GitHub Issues](https://github.com/PeopleWonder/wylde/issues), tracked
  toward the **`0.2` milestone**. Use the [templates](.github/ISSUE_TEMPLATE/).
- **Direction** — [`ROADMAP.md`](ROADMAP.md) (milestones); the detailed internal roadmap is
  maintainer-private.
- **How releases work** — [`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md) and
  [`docs/release-checklist.md`](docs/release-checklist.md).

## Conduct

Be decent: assume good faith, keep discussion technical and respectful, and don't be a jerk. Wylde is
a small project without the contributor volume to justify a formal Code of Conduct document yet; if
the community grows to where one earns its keep, we'll adopt the
[Contributor Covenant](https://www.contributor-covenant.org/). Until then, this paragraph is the
policy, and the maintainer's decision is final on conduct matters.

## License

By contributing, you agree that your contributions are licensed under the project's
**[GPLv3](LICENSE)**.
