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
3. **Local preflight L1–L3** (mechanical smoke) on the release machine:
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
   - L1–L3 as above (build-all, cold-start, service-health).
   - **L4 first-run bootstrap** completes on a clean profile.
   - **L5 reasoning-eval guardrail** — run `wylde-release bench` (or the whole
     `wylde-release preflight`): the reasoning fast/think arms show no regression past the
     baseline's noise-calibrated threshold. Also confirm the shipped config keeps `enabled: false`.
   - **L6 feel/function checklists** — human-judgment surface (visual/layout correctness the
     automated tests structurally can't assert).
   - **L7 GUI verification** — Tier-B panel-walk (all 9 panels + Workspaces subtabs mount, load,
     no panic/error) + selective Tier-C on the ~20–30 critical-path controls.
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
prints pass/fail per metric, and writes a commit-bound `preflight-receipt.json`. `wylde-release
publish` refuses unless a receipt exists that is `all_green`, non-dirty, and whose `commit`/`version`
match what's being shipped (deliberate `--no-preflight-receipt` escape hatch). ⏳ **Remaining
(T0.1):** script the launch-checks **L2/L3** (cold-start + service health) into `preflight` so they
feed the same receipt; today L2–L4/L6/L7 are still run by hand per the sections above. See
`benchmarks/README.md` for the benchmark-gate design.

### Quick commands

```bash
wylde-release bench                 # benchmark regression gate alone (non-zero exit on FAIL)
wylde-release bench --accept-baseline   # deliberately re-record the baseline
wylde-release preflight             # G7 + benchmarks → writes preflight-receipt.json
wylde-release preflight --build     # …also an L1-lite backend+GUI release build
# then, only if the receipt is green:
wylde-release publish --version v0.1.x --channel beta --binary <path> …
```
