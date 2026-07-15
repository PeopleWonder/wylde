# Wylde Roadmap

This is the **public, milestone-level** roadmap — where Wylde is headed, at a glance. It is a summary
for users and contributors; the detailed internal sequencing lives in the maintainer's private
planning notes and is the source this page is derived from.

> **Versioning in one line:** the experimental line ships as `0.1.x` on the **Beta** update channel;
> **`0.2.0` is the first stable release**, cut only when the maintainer judges it ready — a gate that
> is opened, not a date. See [`docs/branch-and-release-policy.md`](docs/branch-and-release-policy.md).

## Now → 0.2 (stabilisation)

0.2 is the first release of the modern all-Rust stack, and its theme is **verified, not just built**.
The headline work:

- **Release gating.** A launch-and-verify preflight (does the app actually start, do the services
  come up, does a chat turn complete) that a release must pass — so 0.2 is the first Wylde release
  that's proven to run, not just proven to compile.
- **Repo & release discipline.** A stable/experimental branch model, enforced branch protection,
  version consistency, and this changelog/roadmap/issue hygiene — so "don't break stable" is
  guaranteed by automation, not hoped for.
- **Ship the fixed trunk and prove it live.** Several alpha-era breakages are already fixed on the
  development trunk; 0.2 ships them and verifies them on a real machine.
- **Service compatibility.** Optional sibling services declare a minimum-Core floor so an
  incompatible service is refused with a clear reason, never a silent dead panel.

## After 0.2

- **Organize & Tabulate panels** over their (independently-versioned) sibling services.
- **Temporal memory graph** (bi-temporal edges), gated.
- **Embedded workflow automation** (n8n), priority TBD.

## Later / exploratory

- Reasoning-tier v2 (off by default; a kill-criterion-gated experiment).
- Mobile app, in-app IDE, gateway/secrets hardening.

## How this stays current

The maintainer updates the detailed internal roadmap in the same change that changes the plan, and
refreshes this summary at each release (a step in the release checklist). For anything actionable —
bugs, feature requests — see [GitHub Issues](https://github.com/PeopleWonder/wylde/issues) and the
**`0.2` milestone**.
