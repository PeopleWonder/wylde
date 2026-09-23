#!/usr/bin/env python3
"""Release milestone gate — refuse to ship a version until its prerequisite
milestones are complete.

GitHub has no native milestone dependency, so this builds it: given a version
tag, it reads tools/release-gates.json for the milestones that must be COMPLETE
(0 open issues) first, verifies each against the live GitHub API, and exits
non-zero if any is incomplete. It is the binding half of the milestone
structure — wired into release.yml and (spec) called by `wylde-release publish`.

Design that can't silently drift:
  - The requirement is DECLARED explicitly per release in release-gates.json
    (reviewable in a PR), not inferred from titles.
  - PLUS an anti-drift cross-check: any milestone whose title starts with the
    release's `prefix` (e.g. "0.2") but is NOT in requires_milestones fails the
    gate. So adding a new 0.2 milestone and forgetting to list it can't pass.

FAIL-CLOSED: any API error, missing config entry, renamed/missing required
milestone, or an empty required milestone → refuse (exit 1). Never assume ready.

Override: --force "reason" opens the gate deliberately (0.2 ships on the
maintainer's say-so) and prints the reason for the caller/receipt to record. A
--force with no reason is rejected.

Usage:
  python tools/check_release_milestones.py v0.2.0
  python tools/check_release_milestones.py v0.2.0 --force "hotfix; (2) intentionally deferred"

Auth: uses `gh api`, so `gh` must be authenticated (locally) or GH_TOKEN set
(CI: env GH_TOKEN: ${{ github.token }}).
"""
from __future__ import annotations
import argparse, json, re, subprocess, sys, pathlib

HERE = pathlib.Path(__file__).resolve().parent
CONFIG = HERE / "release-gates.json"


def die(msg: str) -> "None":
    print(f"::error:: release-milestone gate REFUSED: {msg}", file=sys.stderr)
    sys.exit(1)


def gh_api(path: str):
    """Call `gh api <path>`, fail-closed on any error."""
    try:
        r = subprocess.run(["gh", "api", path], capture_output=True, text=True)
    except FileNotFoundError:
        die("`gh` CLI not found — cannot determine milestone state (fail-closed).")
    if r.returncode != 0:
        die(f"GitHub API call failed for {path!r}: {r.stderr.strip() or 'unknown error'} (fail-closed).")
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as e:
        die(f"could not parse GitHub API response for {path!r}: {e} (fail-closed).")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("tag", help="the version tag being shipped, e.g. v0.2.0")
    ap.add_argument("--force", metavar="REASON", nargs="?", const="", default=None,
                    help="open the gate deliberately; a non-empty reason is required")
    args = ap.parse_args()

    if not CONFIG.exists():
        die(f"missing {CONFIG} (fail-closed).")
    try:
        cfg = json.loads(CONFIG.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        die(f"{CONFIG} is not valid JSON: {e} (fail-closed).")

    repo = cfg.get("repo")
    if not repo:
        die("release-gates.json has no `repo`.")

    entry = (cfg.get("releases") or {}).get(args.tag)
    if entry is None:
        # No entry. Gate only tags that policy says MUST be gated (stable releases);
        # experimental tags (e.g. the 0.1.x line) are ungated by design and pass.
        pattern = cfg.get("require_entry_for")
        if pattern and re.search(pattern, args.tag):
            die(f"tag {args.tag!r} matches require_entry_for {pattern!r} (a gated release) but "
                f"has no entry in release-gates.json — add one before tagging (fail-closed).")
        print(f"OK: {args.tag} is not a milestone-gated release (no matching config entry) — clear to ship.")
        return

    required = entry.get("requires_milestones") or []
    prefix = entry.get("prefix")
    target = entry.get("target_milestone")

    milestones = gh_api(f"repos/{repo}/milestones?state=all&per_page=100")
    by_title = {m["title"]: m for m in milestones}

    problems = []

    # 1. every required milestone must exist, be non-empty, and have 0 open issues.
    for title in required:
        m = by_title.get(title)
        if m is None:
            problems.append(f"required milestone {title!r} does not exist (renamed? deleted?)")
            continue
        total = m.get("open_issues", 0) + m.get("closed_issues", 0)
        if total == 0:
            problems.append(f"required milestone {title!r} has NO issues — it tracks nothing (suspicious)")
        elif m.get("open_issues", 0) > 0:
            problems.append(f"required milestone {title!r} still has {m['open_issues']} open issue(s)")

    # 2. anti-drift: any milestone matching the release prefix (except the target)
    #    must be listed as a requirement.
    if prefix:
        for m in milestones:
            t = m["title"]
            if t.startswith(prefix) and t != target and t not in required:
                problems.append(
                    f"milestone {t!r} matches the {prefix!r} release prefix but is not in "
                    f"requires_milestones — update release-gates.json (drift guard)")

    if not problems:
        print(f"OK: all prerequisite milestones for {args.tag} are complete — clear to ship.")
        return

    # There are open prerequisites. Either refuse, or honour a reasoned override.
    print("Prerequisite milestones are NOT satisfied for " + args.tag + ":", file=sys.stderr)
    for p in problems:
        print(f"  - {p}", file=sys.stderr)

    if args.force is not None:
        reason = (args.force or "").strip()
        if not reason:
            die("--force requires a written reason (e.g. --force \"why you are shipping anyway\").")
        print(f"::warning:: milestone gate OVERRIDDEN with --force. Reason: {reason}")
        print(f"OVERRIDE reason: {reason}")  # for wylde-release to record in the receipt
        return
    die(f"{len(problems)} unmet prerequisite(s) for {args.tag}. Close them, or override with "
        f"--force \"reason\".")


if __name__ == "__main__":
    main()
