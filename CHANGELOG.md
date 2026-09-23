# Changelog

All notable changes to Wylde are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/), and pre-1.0 alphas may break between builds
(see [`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md) §3).

<!--
Maintenance: this changelog is hand-curated (deliberately richer than an
auto-generated bullet list). For any user-facing change, add an entry in the
matching section of the current unreleased version. `tools/changelog-draft.sh`
seeds a draft from Conventional Commits since the last tag — edit it into
narrative form.
Release lines: experimental builds ship 0.1.x (Beta channel); the stable gate is
0.2.0 (Stable channel), cut only on the maintainer's say-so.
State: the workspace version is now 0.2.0-beta.1, but it is NOT yet tagged/released
(that is #38, the maintainer's separate say-so). The section below is therefore
headed "[0.2.0-beta.1] — unreleased"; on release, replace "unreleased" with the tag
date and start a fresh [Unreleased] section above it for later work.
-->

## [0.2.0-beta.1] — unreleased

**Wylde 0.2.0-beta.1 is the first pre-release of the modern, all-Rust stack** (the stable
`0.2.0` cut remains gated on the maintainer's separate say-so, #38). The only
earlier tag, `v0.1.0-alpha.1` (2026-06-04, a GitHub *pre-release* on the Beta channel),
predates the full-Rust cutover entirely — it shipped the gpui desktop rebuild while the
runtime beneath it was still Python. Everything between that tag and this one was built in
the open on the `develop` line and is only now judged ready to carry a 0.2 version, so
this pre-release absorbs an unusually large body of work.

The headline changes: the **full-Rust cutover** (every Python runtime component ported to
Rust and its source deleted); a local-first **memory system** (short-term, long-term, and
reflection across the conversation, workspace, and long-term scopes); the **Thought Bubble
System** with pre-turn structural retrieval; a workspace **knowledge graph** with a native
gpui graph panel and an in-app IDE; **BM25 lexical retrieval + RRF fusion**; a definitional
**concept hierarchy** and a **concept-routing** decision layer (both isolated, default-off,
and byte-identical when disabled); and an **agentic reasoning tier** shipped `enabled:
false` as an opt-in experiment. Wrapping all of it is the **enforcement layer** whose
absence let the alpha ship broken — the GUI panel-walk (L7), the launch-and-verify preflight
and its commit-bound receipt, the benchmark regression gate, version-consistency (G7), and
the license/advisory gates — now wired so the class of defect that shipped before is blocked
rather than merely documented.

The entries below are long because the release is, and they are written to be read: each
says what changed and why it mattered. The release date is stamped when this version is
tagged on the maintainer's say-so (`docs/branch-and-release-policy.md` §5).

### Added

