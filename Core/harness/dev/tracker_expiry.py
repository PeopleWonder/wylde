"""Self-expiring tracker docs — the date logic and the file surgery.

A tracker doc (`docs/trackers/*.md` carrying an `expires:` front-matter key) is the
home for the *next* instance of a recurring problem. It has no open work in it; its
value is that a future diagnosis lands somewhere that already knows the history.

Left alone, such a doc rots: it outlives its subject and becomes a confidently-worded
description of a problem that no longer exists. Kept as an open ISSUE instead, it
clutters the issue list forever, because its closing criterion is always "close it when
the thing has gone quiet long enough to call it dead" — a judgement call requiring
someone to notice the *absence* of events, which nobody does.

Self-expiry makes that criterion a timer:

  * a commit that touches the doc re-derives `expires` to (that commit's date + 1 month)
    — so USING the doc is what keeps it alive;
  * `warn_days` before expiry a heads-up issue is opened;
  * past expiry the doc is DELETED by a PR through the normal gates.

See `docs/trackers/README.md` for the contract. This module is the mechanism; it is
generic over any doc in `docs/trackers/` and knows nothing about #83 specifically.

## The renewal loop, and why bump commits are skipped

The obvious implementation renews forever. The bump commit modifies the doc, so the next
run reads that commit as a "touch" and bumps again — a doc that can never expire, which
is the rot failure mode with extra steps. Every commit this module authors carries
`_BUMP_MARKER` in its subject, and `last_touch()` skips those. That filter is the single
load-bearing line in the whole design, and `tests/test_tracker_expiry.py` pins it.

## Purity

Everything below the git helpers is pure: dates in, dates out, no clock and no
subprocess. `today` is always a parameter, never `date.today()` — so the tests can walk
a doc across its whole lifetime deterministically instead of sleeping.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Iterable, List, Optional, Sequence

# --------------------------------------------------------------------------------------
# Constants
# --------------------------------------------------------------------------------------

TRACKER_DIR = "docs/trackers"

#: Subject marker on every commit this module authors. `last_touch()` skips commits
#: carrying it — see the module docstring. Changing this string orphans the history of
#: previous bumps and re-arms the renewal loop for one cycle.
_BUMP_MARKER = "[tracker-expiry]"

#: Default heads-up window, in days before expiry, when a doc sets no `warn_days`.
DEFAULT_WARN_DAYS = 7

#: How far a touch pushes the expiry out. "1 month" is calendar-relative, not 30 days —
#: see `add_one_month`.
_LIFETIME_MONTHS = 1

_FRONT_MATTER_RE = re.compile(r"\A---\r?\n(.*?)\r?\n---\r?\n", re.DOTALL)
_EXPIRES_RE = re.compile(r"^(?P<indent>\s*)expires:\s*(?P<value>\S+)\s*$", re.MULTILINE)
_SIMPLE_KEY_RE = re.compile(r"^(?P<key>[A-Za-z_][A-Za-z0-9_-]*):\s*(?P<value>.*?)\s*$")

#: A markdown/comment line carrying `tracker-ref: <slug>` is stripped when <slug> expires.
_TRACKER_REF_RE = re.compile(r"tracker-ref:\s*(?P<slug>[A-Za-z0-9._-]+)")

#: Files the expiry step scans for `tracker-ref:` markers. Kept narrow on purpose: a
#: sweep of the whole tree would be slow and would edit files nobody expects to change.
_REF_SCAN_GLOBS = ("docs/**/*.md", "Core/harness/dev/wylde_check/**/*.py", "*.md")


# --------------------------------------------------------------------------------------
# Pure date logic
# --------------------------------------------------------------------------------------


def add_one_month(d: date, months: int = _LIFETIME_MONTHS) -> date:
    """`d` plus `months` calendar months, clamped to the end of the target month.

    Calendar-relative, not 30 days, so a doc touched on the 3rd always expires on the
    3rd and the date reads as intentional. The clamp is the Jan-31 case: one month on
    from 2026-01-31 is 2026-02-28, not a `ValueError` and not 2026-03-03.
    """
    month_index = d.month - 1 + months
    year = d.year + month_index // 12
    month = month_index % 12 + 1
    # Day-of-month clamp: step back one day from the 1st of the FOLLOWING month.
    if month == 12:
        last_day = 31
    else:
        last_day = (date(year, month + 1, 1) - timedelta(days=1)).day
    return date(year, month, min(d.day, last_day))


def derive_expiry(last_touch_day: date) -> date:
    """The expiry a doc SHOULD carry, given when it was last meaningfully touched."""
    return add_one_month(last_touch_day)


def effective_expiry(recorded: Optional[date], derived: Optional[date]) -> Optional[date]:
    """The date actually enforced: the LATER of what's written and what's derived.

    The recorded value is a floor, so a maintainer can extend a tracker by hand past
    what its commit history would give it ("this matters even though nothing has
    happened yet") and the automation will not walk it back. The derived value is a
    floor too, so a stale recorded date cannot expire a doc that was touched yesterday.
    """
    candidates = [c for c in (recorded, derived) if c is not None]
    return max(candidates) if candidates else None


class Status:
    """What should happen to a tracker today."""

    LIVE = "live"  #: nothing to do
    WARN = "warn"  #: inside the heads-up window
    EXPIRED = "expired"  #: past expiry — delete it


def classify(today: date, expiry: Optional[date], warn_days: int) -> str:
    """Map today + expiry onto a `Status`.

    Boundaries, pinned by the tests because off-by-one here is the difference between
    "deleted with a week's notice" and "deleted the day it was written":

      * `today == expiry`            -> WARN. The last day is still a live day; deletion
                                        happens the day AFTER the date on the tin.
      * `today == expiry - warn_days`-> WARN, the first day of the window.
      * `today >  expiry`            -> EXPIRED.
    """
    if expiry is None:
        return Status.LIVE
    if today > expiry:
        return Status.EXPIRED
    if (expiry - today).days <= warn_days:
        return Status.WARN
    return Status.LIVE


# --------------------------------------------------------------------------------------
# Front matter
# --------------------------------------------------------------------------------------


def _parse_date(value: str) -> Optional[date]:
    value = value.strip().strip("\"'")
    try:
        return date.fromisoformat(value)
    except ValueError:
        return None


def parse_front_matter(text: str) -> dict:
    """Minimal `key: value` front-matter reader — deliberately not a YAML parser.

    The tracker contract is a handful of scalar keys, and every consumer of this module
    (CI, the lint hook) runs on a bare stdlib interpreter with no PyYAML. Comment lines
    and anything non-scalar are ignored rather than erroring, so a doc can carry the
    explanatory `#` comment the contract asks for.
    """
    m = _FRONT_MATTER_RE.match(text)
    if not m:
        return {}
    out: dict = {}
    for raw in m.group(1).splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        km = _SIMPLE_KEY_RE.match(line)
        if km:
            out[km.group("key")] = km.group("value").strip().strip("\"'")
    return out


def set_expires(text: str, new_expiry: date) -> str:
    """Rewrite the `expires:` line in place, preserving everything else byte for byte.

    Returns `text` unchanged if there is no front matter or no `expires` key — callers
    treat "unchanged" as "nothing to commit".
    """
    m = _FRONT_MATTER_RE.match(text)
    if not m:
        return text
    block = m.group(1)
    if not _EXPIRES_RE.search(block):
        return text
    new_block = _EXPIRES_RE.sub(
        lambda mm: f"{mm.group('indent')}expires: {new_expiry.isoformat()}", block, count=1
    )
    return text[: m.start(1)] + new_block + text[m.end(1) :]


# --------------------------------------------------------------------------------------
# Git helpers (the only impure surface)
# --------------------------------------------------------------------------------------


def _git(args: Sequence[str], repo: Path) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=str(repo),
        check=True,
        capture_output=True,
        text=True,
        # Explicit DEVNULL, not inheritance: under pytest's output capture on Windows,
        # the inherited stdin handle is not inheritable and Popen dies with
        # `OSError: [WinError 6] The handle is invalid` before git ever runs. Also stops
        # `git log` from ever blocking on a pager or a prompt in CI.
        stdin=subprocess.DEVNULL,
    ).stdout


def last_touch(path: str, repo: Path, _runner=None) -> Optional[date]:
    """Committer date of the newest NON-BUMP commit touching `path`.

    Skipping `_BUMP_MARKER` commits is what stops the doc renewing itself forever; see
    the module docstring. Returns None for a path with no commits yet (a tracker added
    in the same PR that first runs this) — callers fall back to the recorded date.
    """
    runner = _runner or (lambda: _git(["log", "--format=%cI%x09%s", "--", path], repo))
    try:
        out = runner()
    except subprocess.CalledProcessError:
        return None
    for line in out.splitlines():
        if "\t" not in line:
            continue
        iso, subject = line.split("\t", 1)
        if _BUMP_MARKER in subject:
            continue
        try:
            return datetime.fromisoformat(iso).date()
        except ValueError:
            continue
    return None


# --------------------------------------------------------------------------------------
# Tracker model
# --------------------------------------------------------------------------------------


@dataclass
class Tracker:
    slug: str
    path: str
    recorded: Optional[date]
    derived: Optional[date]
    warn_days: int
    origin: Optional[str] = None
    status: str = Status.LIVE
    expiry: Optional[date] = None
    needs_bump: bool = False

    def to_json(self) -> dict:
        return {
            "slug": self.slug,
            "path": self.path,
            "recorded": self.recorded.isoformat() if self.recorded else None,
            "derived": self.derived.isoformat() if self.derived else None,
            "expiry": self.expiry.isoformat() if self.expiry else None,
            "warn_days": self.warn_days,
            "origin": self.origin,
            "status": self.status,
            "needs_bump": self.needs_bump,
        }


def evaluate(
    text: str, path: str, today: date, touch: Optional[date]
) -> Optional[Tracker]:
    """Build a `Tracker` from a doc's text + its last-touch date. Pure.

    Returns None for a doc with no `expires` key — that is how a non-tracker markdown
    file living in `docs/trackers/` (this pattern's own README, for instance) opts out.
    """
    fm = parse_front_matter(text)
    if "expires" not in fm:
        return None
    slug = fm.get("tracker") or Path(path).stem
    recorded = _parse_date(fm["expires"])
    try:
        warn_days = int(fm.get("warn_days", DEFAULT_WARN_DAYS))
    except (TypeError, ValueError):
        warn_days = DEFAULT_WARN_DAYS
    derived = derive_expiry(touch) if touch else None
    expiry = effective_expiry(recorded, derived)
    t = Tracker(
        slug=slug,
        path=path,
        recorded=recorded,
        derived=derived,
        warn_days=warn_days,
        origin=fm.get("origin"),
        expiry=expiry,
    )
    t.status = classify(today, expiry, warn_days)
    # Only bump when the file would visibly change; `expiry` is the enforced value
    # whether or not the front matter agrees, so a bump is cosmetic honesty, not
    # correctness. Never bump a doc that is already expiring — the delete PR wins.
    t.needs_bump = (
        expiry is not None and recorded != expiry and t.status != Status.EXPIRED
    )
    return t


def scan(repo: Path, today: date) -> List[Tracker]:
    """Evaluate every tracker doc in the repo."""
    out: List[Tracker] = []
    tdir = repo / TRACKER_DIR
    if not tdir.is_dir():
        return out
    for p in sorted(tdir.glob("*.md")):
        rel = f"{TRACKER_DIR}/{p.name}"
        t = evaluate(p.read_text(encoding="utf-8"), rel, today, last_touch(rel, repo))
        if t:
            out.append(t)
    return out


# --------------------------------------------------------------------------------------
# Reference stripping
# --------------------------------------------------------------------------------------


def strip_tracker_refs(text: str, slug: str) -> str:
    """Drop every line carrying `tracker-ref: <slug>`.

    Line granularity is the whole contract — a reference that must survive its tracker
    is written as its own line, marked, and disappears whole. Unmarked prose mentions
    are left alone deliberately: `docs/trackers/README.md` asks authors to phrase those
    so they still read correctly once the tracker is gone.
    """
    kept = []
    for line in text.splitlines(keepends=True):
        m = _TRACKER_REF_RE.search(line)
        if m and m.group("slug") == slug:
            continue
        kept.append(line)
    return "".join(kept)


def files_with_refs(repo: Path, slug: str) -> List[Path]:
    hits: List[Path] = []
    for pattern in _REF_SCAN_GLOBS:
        for p in repo.glob(pattern):
            if not p.is_file():
                continue
            try:
                text = p.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            for m in _TRACKER_REF_RE.finditer(text):
                if m.group("slug") == slug:
                    hits.append(p)
                    break
    return sorted(set(hits))


# --------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------


def _today(arg: Optional[str]) -> date:
    return date.fromisoformat(arg) if arg else date.today()


def cmd_report(repo: Path, today: date) -> int:
    trackers = scan(repo, today)
    print(json.dumps([t.to_json() for t in trackers], indent=2))
    return 0


#: Field order of the ``plan`` command's TSV. The workflow destructures positionally
#: (``IFS=$'\t' read -r slug path status ...``), so REORDERING THIS SILENTLY MISWIRES IT —
#: `status` would land in `path` and the sweep would act on the wrong thing. Pinned by
#: `tests/test_tracker_expiry.py::test_plan_field_order_is_pinned`.
PLAN_FIELDS = ("slug", "path", "status", "expiry", "warn_days", "origin", "needs_bump")


def plan_rows(trackers: Sequence[Tracker]) -> List[str]:
    """One tab-separated line per tracker, in `PLAN_FIELDS` order.

    TSV rather than JSON so the workflow needs no `jq`: every value here is a slug, a
    path, an ISO date or a small integer, none of which can contain a tab or a newline,
    so the format cannot be ambiguous. One less tool on the runner is one less thing to
    break a job whose whole purpose is unattended tidying.
    """
    out = []
    for t in trackers:
        out.append(
            "\t".join(
                [
                    t.slug,
                    t.path,
                    t.status,
                    t.expiry.isoformat() if t.expiry else "",
                    str(t.warn_days),
                    t.origin or "",
                    "yes" if t.needs_bump else "no",
                ]
            )
        )
    return out


def cmd_plan(repo: Path, today: date) -> int:
    for line in plan_rows(scan(repo, today)):
        print(line)
    return 0


def cmd_bump(repo: Path, today: date, slug: Optional[str]) -> int:
    """Rewrite stale `expires:` front matter. Prints the paths it changed."""
    changed = 0
    for t in scan(repo, today):
        if slug and t.slug != slug:
            continue
        if not t.needs_bump or t.expiry is None:
            continue
        p = repo / t.path
        text = p.read_text(encoding="utf-8")
        new = set_expires(text, t.expiry)
        if new != text:
            p.write_text(new, encoding="utf-8")
            print(f"{t.path}: expires {t.recorded} -> {t.expiry}")
            changed += 1
    if not changed:
        print("no tracker front matter needed bumping")
    return 0


def cmd_expire(repo: Path, today: date, slug: Optional[str]) -> int:
    """Delete expired trackers and strip their marked references."""
    acted = 0
    for t in scan(repo, today):
        if slug and t.slug != slug:
            continue
        if t.status != Status.EXPIRED:
            continue
        for ref in files_with_refs(repo, t.slug):
            text = ref.read_text(encoding="utf-8")
            new = strip_tracker_refs(text, t.slug)
            if new != text:
                ref.write_text(new, encoding="utf-8")
                print(f"stripped tracker-ref {t.slug} from {ref.relative_to(repo)}")
        (repo / t.path).unlink()
        print(f"deleted {t.path} (expired {t.expiry})")
        acted += 1
    if not acted:
        print("nothing expired today")
    return 0


def main(argv: Optional[Iterable[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", default=".", help="repo root (default: cwd)")
    ap.add_argument("--today", help="ISO date override; the tests and dry runs use this")
    ap.add_argument("--slug", help="restrict to one tracker")
    ap.add_argument("command", choices=("report", "plan", "bump", "expire"))
    ns = ap.parse_args(list(argv) if argv is not None else None)

    repo = Path(ns.repo).resolve()
    today = _today(ns.today)
    if ns.command == "report":
        return cmd_report(repo, today)
    if ns.command == "plan":
        return cmd_plan(repo, today)
    if ns.command == "bump":
        return cmd_bump(repo, today, ns.slug)
    return cmd_expire(repo, today, ns.slug)


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
