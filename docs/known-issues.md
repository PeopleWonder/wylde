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

**ENUMERATED 2026-07-17 (trunk `0e111e3`). The "~6 failures" were never counted — there is exactly
ONE, and it is filed as #80.** A prior session reported ~6 with no tracked home; this entry then
carried "the remaining ~4 still need enumeration" for weeks. A full sweep on a configured box
(`WYLDE_ROOT` + `WYLDE_SERVICES` both set — the condition that provokes this class):

| workspace | result |
|---|---|
| `rust/` `cargo test --workspace` | green except **one** test (`wylde-lifecycle --lib`: 122 passed, 1 failed) |
| `Core/GUI` `cargo panel-walk` (44 binaries) | all green |
| `tools/xtask` · `tools/wylde-release` · `Services/wylde-images` | all green |

**Re GUI tests:** both `cargo test --workspace --locked` and `cargo panel-walk` run them from
`Core/GUI` — the `test-support` seam is wired via the panels' `[dev-dependencies]`, so it compiles
into each crate's test targets under either. CI's required `gui panel-walk (L7)` job uses the alias,
which scopes to the 9 panel crates so the headless gate never links the Shell. (An earlier version of
this entry claimed `--workspace` "runs 0 GUI tests" — that was the #85 misread, now closed: a
`--workspace` run is exit 0, **1151 passed**; the "0 passed" lines are the ~17 doc-test binaries plus
a couple of empty/`#[ignore]`d targets. The genuine cousin — the alias's hardcoded `-p` list drifting
from the panel members — is tracked as #95.)

**The 0.2 question this entry existed to answer is answered: NO, KI-6 hides no 0.2 blocker.**

**The `WYLDE_SERVICES` half is FIXED — #78 (`1dd51be`), landed 2026-07-16.**
`control::tests::service_start_accepts_discovered_sibling` pinned `WYLDE_ROOT` but not
`WYLDE_SERVICES`, which relocates the Services bucket outright — so an ambient value pointed the test
at the developer's real estate instead of its tempdir (`not_registered`; failed 15/15 locally, green
on CI only because CI sets neither). #78 unified **eleven** tests across four modules — which were
using *three different disciplines* (two separate async mutexes and, in one case, nothing at all) —
onto `#[serial]`. Two mutexes over one global is no mutual exclusion; same shape as the gateway
egress bug below. Playbook shape #1: **a discovery test must pin every variable feeding the
resolution**, via an RAII guard so a panicking assert can't leak the override into the next test.

**This entry's diagnosis of the second test was WRONG, and #78 disproved it.** It claimed
`shutdown_all_returns_structured_summary` "fails when `WYLDE_SERVICES` is unset", implying the same
env-isolation cause in the opposite direction. It isn't: by isolation, unsetting **`WYLDE_ROOT`**
alone makes it pass, and **it fails when run entirely alone** — so it is not the parallel race #78
fixed, and it is not a flake. #78 correctly declined to lump it in and documented it in place.

**The one real failure → #80** (Tier 1, not a blocker). `count == 0` asserts "nothing discovered to
stop"; it returns 11. `count` is gated by `is_or_was_tracked()`, which is a bare `path.exists()` on
each service's runtime manifest under the ambient `WYLDE_ROOT` — so the test **stats the developer's
real manifests**. 10 of the 12 core teardown steps have one on this box, plus `wylde-organize`
discovered via `WYLDE_SERVICES` = 11. It read **10 yesterday** (#78) and **11 today**: the number
tracks Aaron's actual service estate as it grows, which is the tell that it is measuring the machine
rather than the fixture. Pinning the env inside the body cannot fix it — the root is resolved before
the body runs — so it needs the root *injected*
(`registry::discovered_bucket_services_in(<tempdir>)` is the existing seam).

**This is the #47/#75 self-collision class, third sighting** — a test asserting against production
state, green in CI *precisely because* CI never runs the product. #47/#75 were **pipe names**; #80 is
the **runtime-manifest directory**. #79's `fixture_pipes_are_private.rs` gate closes the pipe-name
half; **nothing guards the data-dir half.** Expect a fourth sighting on whatever production resource
is next; the durable countermeasure is a static gate, since the dynamic one is structurally blind.

**#80 also turned up a probable product bug** while being diagnosed: `is_or_was_tracked` looks for
`wylde-vram-broker.json`, but the broker self-registers as `vram-broker.json` (no prefix). The
predicate is therefore unconditionally false for the broker, so a real `service.shutdown_all`
under-reports it as not-stopped even when it was just stopped. `registry.rs:1056` documents this
exact quirk and works around it; the teardown reporter doesn't. One quirk, two paths, one patched.

**#80 is FIXED, and the class now has a gate (2026-07-17).** `State::resolve_root` no longer reads
`WYLDE_ROOT` under `cfg(test)`; it returns a per-process scratch path, so every test in
`wylde-lifecycle` is hermetic **by construction** rather than by remembering to guard.
`cargo test --workspace` is now green on a configured box for the first time (106 binaries, exit 0) —
previously impossible.

**Why the gate is structural and not another source scan.** The obvious move was to extend #79's
`fixture_pipes_are_private.rs` from pipe names to the data dir. **That cannot work, and the reason is
the durable lesson.** #79's guard is a scan of *source text*: it catches a production resource that
appears **as a literal in the test**. #80's test contained no literal — it called `dispatch_action`,
and the env read happened three layers down in a process-global `OnceLock`; the only `WYLDE_ROOT`
text in it was a comment, which that guard deliberately strips. A textual gate for #80 would have
been **permanently green — a required check that cannot fail**, which is worse than no gate because
it reads as coverage.

So the class has two halves, and they need different enforcement:

| half | tell | enforcement | sightings |
|---|---|---|---|
| literal in the test | `\\.\pipe\wylde-x` | source scan (#79's `fixture_pipes_are_private.rs`) | #47, #75 |
| resolved inside production code | *none — invisible* | hermetic `cfg(test)` + a gate pinning the property (`state/mod.rs::resolve_root_is_hermetic_under_cfg_test`) | #80 |

**Before extending either, ask which half the resource is.** If the test never names it, a scan buys
a green check and no safety.

The gate was verified to *fail* both ways before being trusted: re-arming #80 trips it **with**
ambient `WYLDE_ROOT` (`assert_ne`) **and without it** (the scratch-dir assertion) — the second matters
most, because it is what makes the gate work in CI, the environment that is blind to the bug itself.
A companion test pins that the *production* arm still reads `WYLDE_ROOT`, so a refactor collapsing the
two arms can't leave the gate green while the shipped daemon writes manifests to a temp dir.

**#80 is CLOSED (#82). KI-6's own question is fully discharged.**

**The findings from this enumeration are now tracked as issues, not just prose here** — a finding that
lives only in a doc is a finding that gets passed over, which is the disease this whole entry is about:

- **#83** — the **self-collision class** as a tracking issue, with #47 / #75 / #80 as sub-issues so the
  pattern reads as one thing instead of three unrelated numbers. Carries the two-halves table, the tell,
  and the rules for a test author. **Open with no open instance, by design** — it is the home for the
  fourth sighting, so the diagnosis isn't re-derived from scratch under a fresh number.
- **#84** — the **vram-broker manifest-name defect** (a real product bug found while diagnosing #80).
- **#85** — CLOSED not-reproducing: the claimed "`--workspace` runs 0 GUI tests" was a misread of
  cargo's doc-test `0 passed` lines; `--workspace` runs the full suite (1151). The genuine cousin —
  the `panel-walk` alias's hardcoded `-p` list can silently under-cover a new panel — is **#95**.

**KI-6 itself can be closed** — it is a duplicate of tracked work now. Kept until the maintainer says,
since this file's own migration to GitHub Issues is the standing plan (see the header) rather than a
per-entry call.

**A second one in this class is now FOUND AND FIXED (2026-07-16):**
`wylde-extension-bridge`'s `mcp::client::tests` mutated the process-global `WYLDE_BIN` / `WYLDE_ROOT`
while `cargo test` ran them in **parallel threads** — `cwd_wylde_root_token_resolves_to_real_root`
set `WYLDE_ROOT=/the/real/root` while `wylde_bin_token_falls_back_to_release_dir` was mid-assert
against `/repo`. **Reproduced at ~8% (2 failures in 25 local runs)**; 0 in 40 after the fix. It
red-walled CI on a PR that touched **no Rust at all**, which is how it was caught.

Two lessons worth generalising:

- **The tests carried a comment asserting `// SAFETY: single-threaded test`.** That was simply
  false — cargo is multi-threaded by default — and the wrong premise is what let the race in.
  Deleted, not corrected-in-place.
- **Fixed with `#[serial]`** (serial_test), the guard the rest of the tree already uses
  (`wylde-shared`, `wylde-harness`, `wylde-concept-routing`, `wylde-concept-hierarchy`). **Any test
  that calls `set_var`/`remove_var` must be `#[serial]`.** Same shape as the `wylde-lifecycle` bug
  above: a test that pins one variable but depends on two.

**Two MORE found and fixed in `wylde-gateway` (2026-07-16), and one was a real product bug:**

- **`egress` — two mutexes guarding one resource.** `egress::destinations`' tests took a private
  `REGISTRY_LOCK`; `egress::client`'s and `pipe`'s took `EGRESS_TEST_LOCK` — for the *same*
  process-global destination registry. **Two different mutexes over one resource give no mutual
  exclusion**: each module was internally serialised and completely unsynchronised against the
  other. A `reload` in one wiped the registry under a request in the other, so the SSRF tests
  failed with `Denied("caller ... declares no egress destinations")` instead of the `Ssrf` they
  assert. Unified on `EGRESS_TEST_LOCK`. **The measurement that proved it:** `egress::client`
  alone = 0/20, `egress::` (client + destinations) = **4/20**. That gap *is* the diagnosis — the
  race is *between* modules, so a per-module loop would have found nothing. Widen the scope until
  the flake appears.
- **`routes::dev::append_line` — a genuine product defect, not a test bug.** It called `write_all()`
  on a `tokio::fs::File` and never flushed. Tokio buffers and defers to a blocking task, does not
  guarantee a flush on drop, and swallows drop-time errors — so the route returned
  `{"recorded": true}` while the record never reached disk. **The GUI error sink could silently
  lose the error it had just confirmed.** Fixed with an explicit `flush().await`. Surfaced as a ~3%
  flake (file created but empty): **the test was right and the product was wrong.**

**A flake is a hypothesis, not a verdict.** Two of the four found today were tests being *correct*
about broken behaviour — one product bug, one genuinely unsafe test premise. "Flaky, re-run it" would
have shipped both.

**Flaky ≠ ignorable.** An ~8% flake in a *required* check is a random tax on every PR and trains
people to hit re-run instead of reading the failure. Enumerate with a repeat loop
(`for i in $(seq 1 25); do cargo test …; done`), not a single run — a single green run proves nothing
about a race.

**Done, 2026-07-17 — the loop came back clean.** 15× over the crates that produced every real bug in
this entry (`wylde-gateway`, `wylde-extension-bridge`, `wylde-workspaces`, `wylde-vram-broker`,
`wylde-vpn`): **15/15 green, nothing intermittent.** Worth stating explicitly because a *clean* repeat
loop is the only result that licenses the conclusion above — it is what makes "there is exactly one
failure, and it is deterministic" a measurement rather than a hope. #80 never needed the loop: it
failed 1/1, run alone.

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
**Status:** RESOLVED (#31) · **Labels:** `docs` · **Area:** docs

Docs referenced the retired `Obsidian Vault\Wylde` / `Wylde-release` locations. Scrubbed in #31:

- **`docs/wylde-repo-organization.md` — the one that actually mattered.** A `status: living reference`
  doc whose §1 stated the root was `%USERPROFILE%\Documents\Obsidian Vault\Wylde\`, that there was no
  `.git/`, and that "`git status` will refuse" — so history lived in progress-memory files, and every
  file was "authoritative current state". All flatly false: the tree is under git, trunk `develop`.
  A living reference asserting the repo isn't a repo actively misleads. Corrected, and its §11
  auto-memory slug now derives from the repo path instead of hardcoding the vault one.
- **`WYLDE_ENDPOINTS.md:504`** — `cwd=vault root` → `cwd=repo root`.
- **`docs/security/pre-alpha-release-2026-05-31.md` — deliberately NOT scrubbed.** It is a dated log
  of "every action taken" on 2026-05-31; rewriting its paths would falsify the record. It carries a
  header note instead: paths are as-of that date, the locations no longer exist, don't navigate by it.

**Deliberately left (belongs to T1.2, not here):** `docs/mypy_baseline.txt` — its vault paths sit
inside captured mypy/uv *stdout*, so it's a tool-output log, not a location claim; rewriting it has
the same falsification problem as the security record. The whole file is a Python-era artifact due for
deletion with the Python scrub (T1.2 / KI-3), which is where that call belongs.

**Also left:** `HANDOFF_TO_FABLE_5.md` — **untracked**, so it isn't part of the tree and can't imply a
stale location to anyone who clones. It's a local scratch file; deleting someone's untracked local
file isn't this issue's business.

> **DELETED 2026-07-17 on the maintainer's say-so.** The reasoning above still stands for *this issue's*
> scope — it was never KI-7's to delete. It went as its own decision: dated 2026-06-09, it named the dead
> vault path as the repo root, told its reader to treat **Nextcloud as authoritative** (retired from canon),
> declared `feat/thought-bubble-system` the trunk, and hard-ruled *"NEVER merge to main"* — which now
> contradicts the develop→main promotion model outright. Nothing unique was lost: its hard rules live in
> agent memory, and its plan content is superseded by `ROADMAP.md`, the issues, and `docs/plans`.

## KI-9 — One worktree outside `worktrees\` (`wt-license-gate`)
**Status:** CHORE · **Labels:** `chore` · **Area:** dev environment

**Recorded so it isn't forgotten, not because it's broken.** The 2026-07-17 worktree sweep removed the
14 merged worktrees under `C:\Users\aaron\Wylde\worktrees\`. **`C:\Users\aaron\Wylde\wt-license-gate`
(`chore/license-compliance-gate`) sits outside that directory** and was left alone: the approval was
scoped to `worktrees\`, and widening it on a technicality wasn't this sweep's call.

It qualifies on the merits — **branch merged into `develop`, tree clean** (verified in the same sweep) —
so it is removable whenever the maintainer says. `git worktree remove <path>`; the branch survives
removal either way.

Six worktrees remain under `worktrees\` **by design, not neglect** — each carries real unmerged commits
(`feeltest/all-hier` 10, `feeltest/organize+tabulate` 7, `feat/wylde-organize-v1` 3,
`feat/temporal-memory-graph` 2, `feat/n8n-embedded-service` 1, `feat/tabulate-panel` 1). The last three
feed **#39/#40** (0.3). **Don't sweep them.**

---

## How these map to the roadmap

- KI-2, KI-4, KI-5 are **release-preflight verifications** folded into 0.2 gating (roadmap Tier 0).
- KI-1 is **post-0.2** reasoning-v2 work (companion plan, Slice B).
- KI-9 is a **dev-environment chore** — off every gate; needs one word from the maintainer, not work.
- KI-3, KI-7 are **Tier-1 hygiene** (roadmap T1.2/T1.3 and §6).
- KI-6 is **enumerated (2026-07-17), fixed (#80/#82), and hid no 0.2 blocker.** The "do this early, it
  may hide a blocker" instruction is discharged and doesn't need repeating. Its findings now live as
  **#83** (self-collision class, tracking) · **#84** (vram-broker under-report) · **#85** (`--workspace`
  GUI trap — **CLOSED as a misread**; its real cousin, panel-walk `-p` drift, is **#95**) — none of
  them 0.2-gating, all Tier 1, none milestoned.
