# Wylde — Release Checklist

**Purpose:** turn "release" from lore into a procedure. This is the runbook the maintainer follows
to cut an experimental (Beta) or stable (Stable) release. It operationalises the gates from the
roadmap (`docs/plans/repo-wide-roadmap-2026-07.md` §3) and the promotion rule from
`docs/branch-and-release-policy.md` §5.

> The gates below reference **G1–G7** (CI-enforceable) and **L1–L7** (local preflight — the
> launch-and-verify checks CI structurally cannot run). Items marked ⏳ are staged/not-yet-built
> (roadmap T0.1); do them manually until the tooling exists.

---

## A. Experimental release (Beta channel, cut from `develop`)

For shipping a `0.1.x` build to Beta-channel users. Lighter bar than a stable promotion — but not
*no* bar.

1. **Version.** Bump both workspace roots to `0.1.x` (`rust/Cargo.toml`, `Core/GUI/Cargo.toml`) —
   together. ⏳ `xtask set-version 0.1.x` once it exists.
2. **CI green (G1–G7)** on the `develop` commit:
   - G1 backend build + test · G2 GUI build · G3 tools build · G5 cargo-deny advisories
   - **G7 version-consistency** (`version-consistency` job) — the two roots agree.
   - ⏳ G4 clippy `-D warnings`, ⏳ G6 `cargo fmt --check` (enable once the tree is clean).
3. **Local preflight L1–L3** (mechanical smoke) on the release machine — **`wylde-release preflight
   --launch`** now scripts L2 + L3 (below) into the receipt; run it (add `--build` for L1-lite):
   - L1 build ALL shipped artifacts (`wylde-gui.exe`, every service binary, the NSIS installer).
   - L2 cold-start smoke (clean install/launch → daemon up → services discovered + spawned).
   - L3 service health (vram-broker inventories HW → Ollama up w/ `nomic-embed-text` → harness
     answers → **Memgraph has real data** → RAG answers → GUI renders → a chat turn completes).
4. **CHANGELOG.** Move the relevant `[Unreleased]` entries under a new `## [0.1.x] — <date>`
   heading; regenerate a draft with `tools/changelog-draft.sh` and edit it into shape.
5. **Publish as a pre-release:**
   ```bash
   git tag -a v0.1.x -m "Wylde 0.1.x (experimental)"
   git push origin develop --follow-tags
   wylde-release publish --version 0.1.x --channel beta --binary <path> …
   ```
6. **Verify** the Beta channel picks it up on a second machine/profile.

## B. Stable release / the 0.2 gate (Stable channel, promoted to `main`)

The full bar. Only on the maintainer's explicit say-so. **This is the definition of done for 0.2**
(roadmap §3e).

1. **Say-so.** The maintainer decides the gate is open. 0.2 is not a date.
2. **Version.** Bump both workspace roots to `0.2.0` — together; G7 will enforce it.
3. **CI green (G1–G7)** on the `develop` commit being promoted.
4. **Local preflight — full L1–L7:**
   - L1–L3 via **`wylde-release preflight --launch --build`** (build-all, cold-start, service-health —
     each check reported individually into the receipt; fails closed).
   - **L4 first-run bootstrap** completes on a clean profile. *(manual — not yet scripted)*
   - **L5 reasoning-eval guardrail** — run `wylde-release bench` (or the whole
     `wylde-release preflight`): the reasoning fast/think arms show no regression past the
     baseline's noise-calibrated threshold.
   - **L5 shipped-config assertion** — `preflight --launch` now asserts this (issue #27); it is no
     longer a manual "also confirm". The `l5.reasoning_disabled` check asks the **running harness**
     for its effective config (`settings.reasoning.get`) and fails closed unless `enabled:false`.
     Asking the live system, not a file, is the point: `ReasoningConfig::current()` is the value the
     turn engine actually obeys, already resolved through the product's own
     `WYLDE_DATA_DIR`/`DATA_DIR`/`WYLDE_ROOT` chain — so a shipped `reasoning.json` that turns the
     tier on fails the gate. The unit-tested code default only ever proved the *fallback*.
   - **L6 feel/function checklists** — human-judgment surface (visual/layout correctness the
     automated tests structurally can't assert).
   - **L7 GUI verification** — Tier-B panel-walk (all 9 panels + Workspaces subtabs mount, load,
     no panic/error) is now a **CI job** (`gui panel-walk (L7)`, run via `cargo panel-walk`) — it
     rides G-tier CI, not just the local preflight (issue #35 / enforcement-matrix row 4b). Still
     owed for L7: selective Tier-C on the ~20–30 critical-path controls.
5. **CHANGELOG + RELEASE_NOTES** current: close `[Unreleased]` into `## [0.2.0] — <date>`; refresh
   `release-artifacts/RELEASE_NOTES.md`.
6. **Promote and release** (via a PR — branch protection blocks direct pushes to `main`):
   ```bash
   gh pr create --base main --head develop --title "Release 0.2.0"
   gh pr merge --merge                   # merges once CI (G1–G7) is green; a --no-ff merge commit
   git checkout main && git pull
   git tag -a v0.2.0 -m "Wylde 0.2.0"    # the tag releases (full release, NOT pre-release)
   git push origin v0.2.0                # release.yml re-verifies G1/G2/G7 on the tag
   wylde-release publish --version 0.2.0 --channel stable --binary <path> …   # refuses without a green preflight receipt
   git checkout develop && git merge --no-ff main && git push   # keep develop current
   ```
7. **Verify distribution:** the Stable channel updater picks `0.2.0` up on a second machine/profile
   (and a Beta user also receives it, since Beta ⊇ Stable).

## C. Hotfix (a shipped stable release is broken in the field)

1. Branch `fix/<slug>` off **`main`**.
2. Fix; CI green (G1–G7); preflight L1–L3 (at minimum) on the release machine.
3. `git checkout main && git merge --no-ff fix/<slug>`; bump patch (`0.2.1`); tag; publish
   `--channel stable`.
4. **Merge `main` back into `develop`** so the fix isn't lost on the next promotion.

---

## Gate quick-reference

| | Beta (0.1.x) | Stable (0.2.0) |
|---|---|---|
| Cut from | `develop` | `main` (promoted from `develop`) |
| GitHub flag | pre-release | full release |
| CI | G1–G7 | G1–G7 |
| Preflight | L1–L3 | **L1–L7** |
| Trigger | maintainer | maintainer's explicit say-so |
| Updater reach | Beta only | Stable + Beta |

**`wylde-release` must refuse to publish if the preflight isn't green.** ✅ **Built:**
`wylde-release preflight` runs **G7 + the benchmark gate (L5)** (+ optional `--build` for L1-lite),
and with **`--launch`** the **L2 cold-start + L3 service-health** launch-and-verify gate; it prints a
per-check verdict and writes a commit-bound `preflight-receipt.json`. `wylde-release publish` refuses
unless a receipt exists that is `all_green`, **`launch_verified`**, non-dirty, and whose
`commit`/`version` match what's being shipped (deliberate `--no-preflight-receipt` escape hatch).

- **L2 cold-start (⏳→✅):** launches the shipped daemon (`wylde-lifecycle.exe`) and GUI
  (`wylde-gui.exe`) the way the launcher does — from a **neutral working directory** so it proves
  env-var resolution, not cwd luck — and asserts each starts, stays up, and binds what it should
  (`\\.\pipe\wylde-lifecycle`; GUI = process-alive + no panic, since window *content* is the CI
  panel-walk's job). If a daemon is already running it *attaches* rather than spawn a sibling stack.
- **L3 service-health (⏳→✅):** discrete assertions — daemon pipe answers · services discovered
  (`service.list`) + core services reachable on their pipes · VRAM broker sees the GPU · Ollama has
  the reasoner + embed models · **Memgraph holds real data** (Bolt node counts > 0 — the empty-graph
  bug a port ping can't see) · RAG answers a fixture query · a chat turn completes · a memory
  round-trips. Each **fails closed** (can't determine → FAIL) and reports individually so a failure
  is diagnosable. Everything spawned is torn down (graceful `service.shutdown_all` + `taskkill /T`
  backstop) — no orphan processes, no pipe collisions with a parallel session.
- **L5 shipped-config** (`l5.reasoning_disabled`, issue #27) — asserts the running harness reports
  reasoning `enabled:false`, so the post-0.2 experimental tier cannot ship switched on. Not
  skippable by `--skip-functional`: a release-grade receipt should never be able to omit it.

⏳ **Still owed (T0.1):** **L4** first-run bootstrap remains manual (**#55** scripts it) and the
**L6** human feel-test remains manual deliberately — visual correctness is not automatable; **L7**
panel-walk is its own CI job. See `benchmarks/README.md` for the benchmark-gate design.

### Quick commands

```bash
wylde-release bench                 # benchmark regression gate alone (non-zero exit on FAIL)
wylde-release bench --accept-baseline   # deliberately re-record the baseline
wylde-release smoke                 # L2/L3 launch-and-verify alone (cold-starts the stack; diagnostic)
wylde-release preflight             # G7 + benchmarks → writes preflight-receipt.json (NOT launch-verified)
wylde-release preflight --build     # …also an L1-lite backend+GUI release build
wylde-release preflight --launch    # …AND the L2/L3 launch gate → a launch-verified, publishable receipt
# then, only if the receipt is green AND launch-verified:
wylde-release publish --version v0.1.x --channel beta --binary <path> …
```

> **A release-grade receipt needs `--launch`.** A plain `wylde-release preflight` writes a green
> receipt for fast iteration, but `publish` refuses it as *not launch-verified*. Run
> `wylde-release preflight --launch` on the release machine (with the stack able to come up) to
> certify L2/L3 and produce a publishable receipt.
