# Migration — from feature-branch-as-trunk to the stable/experimental model

**Companion to** [`docs/branch-and-release-policy.md`](./branch-and-release-policy.md).
**Goal:** move from today's state (a feature branch acting as trunk; `main` mirroring it) to
`main` = stable / `develop` = experimental — **without rewriting any public history**, because
`main` is public and anyone who cloned has it.

---

## 0. The good news: there is nothing to untangle

```
$ git merge-base --is-ancestor main feat/thought-bubble-system   →  YES
$ git log --oneline feat/thought-bubble-system..main             →  (empty)
```

**`main` is a strict ancestor of the trunk** — it is 7 commits *behind* `feat/thought-bubble-system`,
with **zero divergence**. "Stable" today isn't a *forked* line that has to be reconciled; it's just a
pointer that lags the experimental line. That makes this migration a **re-labelling exercise, not a
history operation.** No rebase, no force-push, no rewrite. The only care needed is around the GitHub
web-UI steps and the pile of stale local branches.

Current facts (2026-07-14):
- Trunk: `feat/thought-bubble-system` @ `c28e991` (pushed to origin).
- `main` @ `9a1a087` (pushed) — the S5 merge; ancestor of trunk.
- Only tag: `v0.1.0-alpha.1` (a GitHub pre-release).
- Default branch on GitHub: `main` (`origin/HEAD → origin/main`).

---

## 1. Sequence overview

| # | Step | Rewrites history? | Whose hand |
|---|---|---|---|
| A | Land the hygiene artifacts (this change) on the trunk | No | done in this change |
| B | Rename `feat/thought-bubble-system` → `develop` on GitHub | No | **maintainer (web UI)** |
| C | Sync local clone to the rename | No | maintainer (CLI) |
| D | Add branch protection to `main` and `develop` | No | **maintainer (web UI)** |
| E | Leave `main` frozen until the 0.2 gate | No | policy |
| F | Point CI triggers at `develop` | No | done in this change |
| G | Staged cleanup of stale local branches + worktrees | No | maintainer (CLI, reviewed) |
| H | File the deferred bugs as GitHub Issues | No | **maintainer (or on say-so)** |

Only B, D, and H touch the GitHub web UI or publish public content — those are flagged
**maintainer-hand** and must not be automated from here.

---

## 2. Step A — land the hygiene artifacts (this change)

Already in flight: templates, `CHANGELOG` note, `SECURITY.md`, `CONTRIBUTING.md`, the policy docs,
the `check-versions.sh` gate + CI job, and the `develop` CI trigger. This lands on the current
trunk via a `--no-ff` merge, so it's present the moment the trunk becomes `develop`.

## 3. Step B — rename the trunk to `develop` *(maintainer, GitHub web UI)*

**Do the rename on GitHub, do not delete-and-recreate.** A rename preserves history, sets up
automatic redirects for the old name, and **retargets any open pull requests** — a fresh branch
would orphan them.

1. First check for open PRs targeting `feat/thought-bubble-system`
   (`gh pr list --base feat/thought-bubble-system`). The rename retargets them automatically, but
   know what's affected.
2. GitHub → repo **Settings → Branches** (or the branch list) → rename `feat/thought-bubble-system`
   → **`develop`**. (Equivalently `gh api -X POST repos/PeopleWonder/wylde/branches/feat/thought-bubble-system/rename -f new_name=develop`.)
3. Leave the **default branch as `main`** — a public repo's landing branch should show the stable
   face, and the updater's Stable source should be the default. (Active work happens on `develop`;
   PRs target `develop`; the default staying `main` is fine and conventional.)

## 4. Step C — sync the local clone *(maintainer, CLI)*

```bash
git branch -m feat/thought-bubble-system develop   # rename locally
git fetch origin
git branch -u origin/develop develop                # re-track
git remote set-head origin -a                        # refresh origin/HEAD
git worktree list                                    # note: any worktree on the old name needs re-checkout
```

Any worktree checked out on `feat/thought-bubble-system` should be moved to `develop`
(`git -C <worktree> checkout develop`).

## 5. Step D — branch protection *(maintainer, GitHub web UI)*

Protect **both** long-lived branches. The exact ruleset config — which checks to require, what to
block, and what to deliberately leave off for a solo repo — is specified in
**`docs/branch-and-release-policy.md` §6.1 ("Studio-grade enforcement")**. In short: for each of
`main` and `develop`, add a ruleset that **blocks force-pushes + deletions** and **requires the CI
status checks** (backend, gui, tools, version-consistency, cargo-deny) to pass; on `main` also
**require a PR** but leave required approvals at **0** (solo). Skip required reviews, signed commits,
and merge-queue (ceremony at this scale — see §6.1 for why).

This is the setting that makes "don't break stable" *enforced by GitHub* rather than remembered by a
human — the whole point of the migration.

## 6. Step E — freeze `main` until the 0.2 gate *(policy, no action)*

**Do not fast-forward `main` to the trunk.** Moving `main` up to the experimental HEAD would
re-create the exact bug this migration removes (stable = experimental). `main` stays at `9a1a087`
— honestly labelled as *pre-0.2, not yet a verified release* — until `0.2.0` is gated and promoted
per policy §5. Because the only tag is a pre-release, the updater's **Stable channel serves nothing
in the meantime**, which is correct: there is no stable release yet. Beta-channel users get the
`0.1.x` experimental builds cut from `develop`.

> If you want the very next thing on `main` to be a *clean* 0.2.0 rather than an S5-era commit,
> that happens automatically at the first promotion: `git checkout main && git merge --no-ff develop`
> brings `main` up to the gated 0.2.0 in one reviewed, tagged step.

## 7. Step F — CI triggers *(done in this change)*

`ci.yml` and `security-audit.yml` push-triggers now include **`develop`** alongside the current
branch name (kept temporarily with a comment), so CI keeps running before *and* after the rename.
Once the rename lands and no branch named `feat/thought-bubble-system` remains, drop that line.

## 8. Step G — staged cleanup of stale branches & worktrees *(maintainer, CLI, reviewed)*

There are ~180 local branches and a dozen worktrees — most are merged slices whose work is already
on the trunk. **Do not bulk-delete blind**; several are attached to worktrees and cannot be deleted
until the worktree is removed. Use a *reviewed*, staged approach:

**8a. List what is provably merged into the new trunk (safe-to-delete candidates):**

```bash
# after the rename, with develop checked out:
git branch --merged develop | grep -vE '^\*|(^|\s)(develop|main)$'
```

Everything that prints here has its commits already in `develop` — deleting the *label* loses
nothing. Review the list, then delete in batches:

```bash
git branch --merged develop | grep -vE '^\*|(^|\s)(develop|main)$' | xargs -r -n1 git branch -d
```

(`-d`, not `-D`, so Git refuses to delete anything *not* actually merged — a built-in safety net.)

**8b. Worktree-attached branches** (the `+` rows in `git branch -vv`, e.g. the reasoning slices
`wylde-wt-reasoning-*`, `wylde-wt-hierarchy`, `wylde-wt-lexical`): remove the worktree first, then
the branch.

```bash
git worktree list                 # see attachments
git worktree remove <path>        # frees the branch
git branch -d <branch>
```

**8c. Un-merged branches you still want gone** (superseded experiments): confirm intent per branch,
then `git branch -D`. The roadmap (T1.1) lists an objective delete-set; reconcile against it.
**Flag:** the roadmap notes "4 branches to bin" that aren't labelled in-repo — confirm which four
before force-deleting anything un-merged.

**8d. Remote branches:** the stale `origin/*` topic branches (`origin/feat/tbs-slice-0a-scaffold`,
etc.) can be pruned once their work is confirmed on `develop`:
`git push origin --delete <branch>` — reviewed, one at a time. `git remote prune origin` drops
local refs to already-deleted remotes.

This is deliberately manual and reviewed: branch deletion is cheap to get right and annoying to get
wrong, and it's not on the 0.2 critical path.

## 9. Step H — file the deferred bugs as Issues *(maintainer, or on say-so)*

The interim backlog lives in [`docs/known-issues.md`](./known-issues.md). Creating GitHub Issues
publishes public content, so it isn't automated from here — file them from the backlog using the
new bug template (30 seconds each), or say the word and I'll open them. Once filed, `known-issues.md`
becomes a pointer and can be trimmed.

---

## 10. What needs the maintainer's hand — summary

1. **Rename** `feat/thought-bubble-system` → `develop` on GitHub (§3). *Web UI.*
2. **Branch protection** on `main` and `develop` (§5). *Web UI.*
3. **Reviewed branch/worktree cleanup** (§8). *CLI, your judgement on the un-merged ones.*
4. **File the deferred-bug Issues** (§9). *Publishes public content — your call or your say-so.*

Everything else (artifacts, CI triggers, the version gate) lands automatically with this change.
