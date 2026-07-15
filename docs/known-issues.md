# Known Issues — interim backlog (migrate to GitHub Issues)

**Status:** temporary staging area. These are deferred bugs and follow-ups that had no home. The
target is **GitHub Issues** (public repo, links commits/PRs, has state) — file each from here using
the bug template, then trim this file to a pointer. Until then, this is the tracked backlog so
nothing is lost.

> Filing note: creating public Issues publishes public content, so it isn't automated. File them
> from the maintainer account, or ask and they'll be opened. Suggested labels are in each entry;
> `bug` and `enhancement` already exist as default repo labels.

Legend: **OPEN** = live bug · **VERIFY** = believed fixed on trunk, prove it live · **CHORE** =
tooling/process · **NEEDS-DETAIL** = real but its specifics live in a session record not in this repo.

---

## KI-1 — Planner↔executor tool-vocabulary mismatch (reasoning tier)
**Status:** OPEN · **Labels:** `bug`, `reasoning` · **Area:** Chat / turn engine (harness)

The reasoning planner and executor are grounded in different tool universes: the planner
(`reasoning/inputs.rs::render_tool_catalog`) emits canonical tool `id`s (`read_file`), while the
executor advertises only the dotted verb surface (`wylde_get`, …) filtered through `advertise()`.
Plans therefore name tools the executor can't dispatch, so plan steps never "realize," the
expected-outcome/surprise machinery goes dark, and the tier is effectively decorative on multi-step
tasks. This is off the 0.2 critical path (the tier is `enabled:false` by default), but it's the
linchpin defect for the post-0.2 reasoning-v2 work.

**Fix:** `docs/plans/reasoning-tier-v2-plan.md` Slice B (ground the planner in the advertised catalog,
inject the resource-type catalog, canonicalize both sides through `build_alias_map()`).
**Repro:** run `examples/reasoning_eval.rs` on a multi-step task; step-realization rate ≈ 0.

## KI-2 — RAG service could not start (dead Python venv) on shipped alpha
**Status:** VERIFY · **Labels:** `bug`, `rag` · **Area:** Workspaces / RAG

Shipped `v0.1.0-alpha.1` ran the Python RAG whose 3.11 venv was later uninstalled. The full-Rust
cutover (`2f5aa82`) deleted the Python runtime and moved RAG to Rust, so this **fixes by construction
when shipping from trunk** — but it has never been proven live. Belongs to release preflight **L3**
(RAG answers a query on a clean install), not a code fix.

## KI-3 — Two unbumpable advisories behind the gpui git-rev pin
**Status:** CHORE (accepted) · **Labels:** `dependencies`, `security` · **Area:** deps

`Core/GUI/Cargo.toml` pins `gpui`/`gpui_platform` to a git rev; Dependabot can't bump git revs, so
`async-tar` (rides behind the pin) and `glib` (off-Windows only) are frozen. Policy (roadmap §6,
`docs/security/dependency-hygiene-policy.md`): accept both as environmental in `deny.toml` with a
review date; bump gpui deliberately as its own reviewed slice. Track here so the review date isn't
lost. Not a code fix — a standing dependency-hygiene item.

## KI-4 — Reasoning tier must stay `enabled:false` in the shipped config
**Status:** VERIFY · **Labels:** `reasoning`, `release` · **Area:** Chat / turn engine

`ReasoningConfig::default` is `enabled:false, default_depth:Fast` on trunk, and must stay that way in
the shipped 0.2 config (the tier is a post-0.2 experiment; its worst bug — empty deep-turn answers —
was fixed in S6). Belongs to preflight **L5** (assert the shipped config keeps `enabled:false`).

## KI-5 — Move-stale workspace index (`Wylde-release-abbafc`)
**Status:** OPEN · **Labels:** `bug`, `workspaces` · **Area:** Workspaces / indexing

A workspace manifest hardcodes old Obsidian-vault paths
(`\\?\C:\...\Obsidian Vault\...\manifest.json`), so opening that workspace in a first-run demo hits
dead paths. Re-index or purge before demoing 0.2 (roadmap T0.5). Runtime data, not shipped in the
artifact, but breaks the demo.

## KI-6 — Carrying-over test failures (incl. a confirmed env-isolation bug)
**Status:** OPEN (partial) · **Labels:** `bug`, `test` · **Area:** wylde-lifecycle + others

A prior session reported ~6 test failures with no tracked home. Enumerate with `cargo test
--workspace` on trunk and file one issue per failure. **One is now root-caused** (found while
building the min_core floor): two `wylde-lifecycle` tests are **not env-isolated** and flip on the
ambient `WYLDE_SERVICES` variable in *opposite* directions —

- `control::tests::service_start_accepts_discovered_sibling` **fails when `WYLDE_SERVICES` is set**
  (it sets `WYLDE_ROOT` to a tempdir, but `WYLDE_SERVICES` takes precedence in discovery, so the
  fixture sibling isn't found → `not_registered`).
- `control::tests::shutdown_all_returns_structured_summary` **fails when `WYLDE_SERVICES` is unset**.

**Fix:** these tests must save/clear/restore `WYLDE_SERVICES` (and `WYLDE_ROOT`) around the body, the
way other registry tests guard the env — a discovery test must pin *both* variables, not one. This is
a real test-hygiene bug, not a code defect, and it's exactly why a green suite isn't a green *system*.
The remaining ~4 failures still need enumeration; don't close until each is filed or resolved.

## KI-8 — Enable clippy (G4) + fmt (G6) gates — needs a cleanup pass first
**Status:** CHORE · **Labels:** `ci`, `chore` · **Area:** tooling

The `cargo fmt --check` (G6) and `cargo clippy -D warnings` (G4) gates are staged as commented stubs
in `ci.yml` but **not enabled**, because the tree isn't clean yet — `cargo fmt --check` on `rust/`
reports diffs today (verified; e.g. `build-support/wylde-prebuild-guard/src/lib.rs`). Enabling a gate
on a dirty tree red-walls CI for every PR. **Do:** run `cargo fmt --all` (each workspace) as its own
slice, land it, then uncomment the G6 stub; separately do a `cargo clippy --workspace` pass, fix or
`#[allow]`-with-reason the warnings, then uncomment G4. Once enabled they become required checks
(add to the rulesets). This is the last piece of the enforcement matrix still marked ⏳-blocked.

## KI-7 — Move-stale doc references (old vault paths)
**Status:** CHORE · **Labels:** `docs` · **Area:** docs

Several docs reference the old `Obsidian Vault\Wylde-release` path and are superseded:
`HANDOFF_TO_FABLE_5.md` (untracked, 2026-06-09), and per roadmap T1.3:
`docs/security/pre-alpha-release-2026-05-31.md`, `docs/mypy_baseline.txt`, `WYLDE_ENDPOINTS.md:504`.
Scrub or delete. Low priority; grouped so the tree stops implying stale locations/a Python runtime.

---

## How these map to the roadmap

- KI-2, KI-4, KI-5 are **release-preflight verifications** folded into 0.2 gating (roadmap Tier 0).
- KI-1 is **post-0.2** reasoning-v2 work (companion plan, Slice B).
- KI-3, KI-7 are **Tier-1 hygiene** (roadmap T1.2/T1.3 and §6).
- KI-6 is **unknown scope** until enumerated — do that early since it may hide a 0.2 blocker.
