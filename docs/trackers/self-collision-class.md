---
tracker: self-collision-class
expires: 2026-08-23
warn_days: 7
origin: 83
# SELF-EXPIRING DOC. `expires` is re-derived as (date of the last commit that
# touched this file) + 1 month, so recording a sighting below resets the clock.
# Untouched past `expires`, a scheduled job DELETES this file via a normal PR —
# recoverable from git history, never force-pushed. That is the automatic
# encoding of #83's own closing criterion: "close it if the class goes quiet
# long enough to call it dead". See docs/trackers/README.md.
---

# The self-collision class

> **This doc expires.** It is the live home for the next sighting of a bug class that
> currently has none. If a month passes with nothing recorded here, the class has gone
> quiet long enough to call it dead and this file is deleted automatically — that is the
> point, not a failure. Recovering it is `git log --diff-filter=D -- docs/trackers/self-collision-class.md`.
> Full mechanism: [`README.md`](README.md). Origin: **#83**.

## The class

**A test asserts against a resource the *product* owns.** On a machine running or
configured for Wylde the resource is already taken (or already populated), so the test
fails. On CI it is free, so the test passes.

**That inversion is the whole problem.** It is not a flake — flakes fail randomly
everywhere. This class is:

- **deterministic** on a developer's rig,
- **permanently green** on CI,
- and therefore **invisible to the gate that is supposed to catch it.**

CI never runs the Wylde stack and sets no `WYLDE_*` variables. So the environment that
reviews every PR is precisely the environment in which the bug cannot manifest. The signal
is inverted: a red local run and a green PR is the *symptom*, and the natural reading of it
— "CI is right, my box is weird, re-run it" — is exactly backwards.

## The tell

**A fixture that reads ambient state measures the machine, not the fixture.**

The giveaway is an assertion whose expected value is only true by accident of a clean box.
If a number in an assertion would change when a developer installs another service, boots
the stack once, or sets an env var — it is measuring the machine.

**#80** is the clearest illustration: it asserted `count == 0` and got **`10` on 2026-07-16
and `11` on 2026-07-17**. Nothing about the test changed. The number tracked a real service
estate as it grew. A test whose expected value drifts with the developer's install is not
testing the code.

## Sightings

| # | resource | how it presented | outcome |
|---|---|---|---|
| **#47** | live `wylde-*.exe` | prebuild guard false-positived on any running binary; blocked `preflight --launch` | fixed |
| **#75** | pipe name `\\.\pipe\wylde-workspaces` | fixture server bound the PRODUCTION pipe; `ERROR_ACCESS_DENIED` / `ERROR_PIPE_BUSY` on a live rig. **Caught because it red-walled a PR containing no Rust at all.** | fixed; became the #79 scanner |
| **#80** | runtime-manifest directory | `is_or_was_tracked` = bare `path.exists()` under the ambient `WYLDE_ROOT`; counted real manifests | fixed; became the #82 hermetic root |
| **#224** | `WYLDE_IMAGES_*` env | `Config::load` tests read ambient env in the separate `wylde-images` repo; demonstrated RED-on-dev / GREEN-on-CI. Latent — no in-tree offender | closed (repo parked) |
| **#225** | fixture pipe in `src/**` | the #79 scanner walked `tests/**` only, so a `#[cfg(test)]` module inside `src/**` could bind a production pipe unseen. No offender existed; the hole did | closed; scanner extended |
| **#226** | shared Neo4j (`bolt://`) | two `#[ignore]`d live-graph tests in one binary contend on graph-*global* state (`ensure_schema`, `stats()`, the `delete_workspace` orphan prune) under the default multi-threaded runner | closed; became rule 56 |
| **#232** | pipe half of `memgraph_parity_integration` | targeted a service removed in the Rust cutover — a guard aimed at something that no longer exists | closed; retired |

Two of the first three were caught by accident. That is the argument for a gate.

## What guards it now — and the trap to avoid

**The class has two halves, and they need different enforcement.** Getting this wrong
builds a check that cannot fail, which is worse than no check because it reads as coverage.

| half | tell | enforcement | sightings |
|---|---|---|---|
| resource is a **literal in the test** | `\\.\pipe\wylde-x` appears in the source | **static source scan** — `Core/GUI/Frontend/Panels/Workspaces/tests/fixture_pipes_are_private.rs` (#79, extended to `src/**` `#[cfg(test)]` regions by #225) | #47, #75 |
| resource is **resolved inside production code** | **nothing appears in the test at all** | **hermetic `cfg(test)`** + a test pinning the property — `rust/crates/wylde-lifecycle/src/state/mod.rs::resolve_root_is_hermetic_under_cfg_test` (#82) | #80 |
| resource is **shared and stateful** (one live DB) | two live tests, one process, no lock | **wylde_check rule 56** `graph_test_serialized_on_db_lock` — every multi-test `bolt://` binary must take a per-test `DB_LOCK` **and** be run in the live-graph CI leg (#216/#226/#227) | #226 |
| resource is **the GUI's own runtime** | a panel test that boots the real Shell | **panel-walk hermeticity** — `cargo panel-walk` drives panels against fixture state, never a live service; a panel test that reaches a real pipe fails the walk | #75 |

**Why the second half can't be scanned for.** #80's test named no resource. It called
`dispatch_action`; the `WYLDE_ROOT` read happened three layers down inside a process-global
`OnceLock`. The only `WYLDE_ROOT` text in that test was in a **comment** — which the #79
scanner deliberately strips. Extending the scan to cover #80 would have produced a
permanently-green required check.

**Before adding a resource to any guard, ask which half it is.** If the test never names
the resource, a scan buys a green check and no safety — make the resolution hermetic under
`cfg(test)` instead, so ambient state physically cannot leak in.

## Rules for a test author

1. **A fixture owns a fixture resource.** Pipes: `unique_pipe_name` + `PipeNameOverride`
   (GUI) or `unique_service_name()` (`rust/`, since #29). Roots/dirs: resolve under
   `cfg(test)` to a scratch path — never ambient env.
2. **An assertion's expected value must be a fact of the fixture, not of the machine.** If
   installing a service would change it, it is wrong.
3. **A discovery path must pin *every* variable feeding the resolution** — `WYLDE_ROOT`
   *and* `WYLDE_SERVICES`, not one (#78's playbook shape #1).
4. **A shared stateful resource needs a lock, not just a namespace.** Namespacing your own
   workspace id does not save you from graph-*global* operations (#226).
5. **A gate you have not watched fail is a rumour.** Re-arm the bug and confirm it fires —
   **including with the ambient variable unset**, because that is CI, the environment blind
   to the bug. A gate that only fires where the bug is already visible is worthless.

## Reproducing without launching the stack

Bind the contested resource from a stand-in. For #75 a single PowerShell
`NamedPipeServerStream('wylde-workspaces', ...)` reproduced it exactly. The errno may differ
(231 pipe-busy vs 5 access-denied) because a stand-in's instance limit differs from the real
service's — the collision is the same.

---

## Record a new sighting here

**This section is the reason the file exists.** Appending to it is also what resets the
expiry clock — a commit that touches this file re-derives `expires` to that commit's date
+ 1 month. So recording a sighting keeps the tracker alive for another month by itself; you
do not edit the date by hand.

Copy the block below, fill it in, commit. Open a normal issue too if the sighting needs
work tracked — this doc is the *diagnosis* home, not a substitute for an issue.

```markdown
### <date> — <one-line title> (#<issue>)

- **Resource:** <the product-owned thing the test collided with>
- **Half:** literal-in-test | resolved-in-production | shared-stateful | gui-runtime
- **How it presented:** <red where, green where, and what the misleading reading was>
- **Why the existing guards missed it:** <this is the valuable part>
- **Fix + new guard:** <what changed, and which half now enforces it>
```

*No sightings recorded since this doc was created. The guards above are the reason — see
the closing note in #83.*
