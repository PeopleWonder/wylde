"""Tests for the self-expiring tracker mechanism (#253, retiring #83).

The date logic is pure and `today` is always injected, so a doc's whole lifetime is
walked deterministically here rather than waiting a month to find out. Two behaviours
are the point of the file and are pinned hardest:

  * **reset on touch** — a commit touching the doc pushes expiry out a month;
  * **the renewal-loop guard** — the automation's OWN bump commits must not count as a
    touch, or the doc renews itself forever and can never expire.
"""

from __future__ import annotations

import subprocess
import sys
from datetime import date
from pathlib import Path

import pytest

_REPO = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(_REPO / "Core" / "harness" / "dev"))

import tracker_expiry as tx  # noqa: E402

DOC = """\
---
tracker: demo
expires: 2026-08-23
warn_days: 7
origin: 83
# a comment line the parser must ignore
---

# Demo

body text
"""


# ---------------------------------------------------------------------------- dates


@pytest.mark.parametrize(
    "start,expected",
    [
        (date(2026, 7, 23), date(2026, 8, 23)),  # ordinary
        (date(2026, 12, 15), date(2027, 1, 15)),  # year rollover
        (date(2026, 1, 31), date(2026, 2, 28)),  # clamp to a short month
        (date(2028, 1, 31), date(2028, 2, 29)),  # ...and a leap February
        (date(2026, 3, 31), date(2026, 4, 30)),  # 31 -> 30
        (date(2026, 11, 30), date(2026, 12, 30)),  # into December (the `month == 12` arm)
    ],
)
def test_add_one_month(start, expected):
    assert tx.add_one_month(start) == expected


def test_derive_expiry_is_touch_plus_a_month():
    assert tx.derive_expiry(date(2026, 7, 23)) == date(2026, 8, 23)


def test_effective_expiry_takes_the_later_date():
    early, late = date(2026, 8, 1), date(2026, 9, 1)
    # A hand-set later date wins: the maintainer's manual extension is a floor.
    assert tx.effective_expiry(late, early) == late
    # A fresh touch wins over a stale recorded date.
    assert tx.effective_expiry(early, late) == late
    assert tx.effective_expiry(None, early) == early
    assert tx.effective_expiry(early, None) == early
    assert tx.effective_expiry(None, None) is None


# ----------------------------------------------------------------------- classify


@pytest.mark.parametrize(
    "today,expected",
    [
        (date(2026, 8, 15), tx.Status.LIVE),  # 8 days out — one day before the window
        (date(2026, 8, 16), tx.Status.WARN),  # exactly warn_days out — window opens
        (date(2026, 8, 22), tx.Status.WARN),  # day before
        (date(2026, 8, 23), tx.Status.WARN),  # ON the date: still a live day
        (date(2026, 8, 24), tx.Status.EXPIRED),  # the day AFTER — deletion day
        (date(2026, 9, 30), tx.Status.EXPIRED),
    ],
)
def test_classify_boundaries(today, expected):
    assert tx.classify(today, date(2026, 8, 23), 7) == expected


def test_classify_without_an_expiry_is_live():
    assert tx.classify(date(2026, 8, 24), None, 7) == tx.Status.LIVE


# -------------------------------------------------------------------- front matter


def test_parse_front_matter_reads_scalars_and_skips_comments():
    fm = tx.parse_front_matter(DOC)
    assert fm["tracker"] == "demo"
    assert fm["expires"] == "2026-08-23"
    assert fm["warn_days"] == "7"
    assert fm["origin"] == "83"
    assert not any(k.startswith("#") for k in fm)


def test_parse_front_matter_absent():
    assert tx.parse_front_matter("# no front matter\n") == {}


def test_set_expires_rewrites_only_that_line():
    out = tx.set_expires(DOC, date(2026, 9, 30))
    assert "expires: 2026-09-30" in out
    assert "expires: 2026-08-23" not in out
    # Everything else is byte-identical.
    assert out.replace("2026-09-30", "2026-08-23") == DOC


def test_set_expires_is_a_no_op_without_front_matter():
    text = "# plain\n"
    assert tx.set_expires(text, date(2026, 9, 1)) == text


# ------------------------------------------------------------------------ evaluate


def test_doc_without_expires_is_not_a_tracker():
    """This is how `docs/trackers/README.md` lives in the folder without expiring."""
    assert tx.evaluate("---\ntitle: x\n---\n\nhi\n", "docs/trackers/README.md",
                       date(2026, 7, 23), None) is None


def test_touch_resets_the_clock():
    """The core behaviour: recording a sighting buys another month."""
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 8, 20), date(2026, 8, 19))
    assert t.derived == date(2026, 9, 19)
    assert t.expiry == date(2026, 9, 19)
    assert t.status == tx.Status.LIVE  # was 3 days from expiry; the touch rescued it
    assert t.needs_bump is True  # front matter still says 2026-08-23


def test_untouched_doc_expires_on_its_recorded_date():
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 8, 24), date(2026, 7, 23))
    assert t.expiry == date(2026, 8, 23)
    assert t.status == tx.Status.EXPIRED


def test_expiring_doc_is_never_bumped():
    """The delete PR wins; a bump would resurrect a doc mid-expiry."""
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 9, 1), None)
    assert t.status == tx.Status.EXPIRED
    assert t.needs_bump is False


def test_no_bump_when_front_matter_already_agrees():
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 8, 1), date(2026, 7, 23))
    assert t.expiry == date(2026, 8, 23)
    assert t.needs_bump is False


# --------------------------------------------------------------- the renewal loop


def test_last_touch_skips_the_automations_own_bump_commits():
    """The single load-bearing filter: without it the doc renews itself forever.

    Newest commit first, as `git log` emits. The bump is newest; the real touch is
    older. Reading the bump as a touch is exactly the bug.
    """
    log = (
        "2026-08-19T10:00:00+00:00\tchore(docs): bump expires [tracker-expiry] demo\n"
        "2026-07-23T09:00:00+00:00\tdocs(trackers): record a fourth sighting (#83)\n"
    )
    got = tx.last_touch("docs/trackers/demo.md", _REPO, _runner=lambda: log)
    assert got == date(2026, 7, 23)


def test_last_touch_returns_the_newest_real_touch():
    log = (
        "2026-08-19T10:00:00+00:00\tdocs(trackers): a newer real edit\n"
        "2026-07-23T09:00:00+00:00\tdocs(trackers): an older one\n"
    )
    assert tx.last_touch("x", _REPO, _runner=lambda: log) == date(2026, 8, 19)


def test_last_touch_of_an_uncommitted_file_is_none():
    assert tx.last_touch("x", _REPO, _runner=lambda: "") is None


def test_last_touch_survives_a_git_failure():
    def boom():
        raise subprocess.CalledProcessError(128, "git")

    assert tx.last_touch("x", _REPO, _runner=boom) is None


def test_a_doc_touched_only_by_bumps_still_expires():
    """End to end on the loop: bump-only history must NOT keep a doc alive.

    Simulates the renewal loop directly — every commit on the file is the automation's
    own bump, so `last_touch` is None, the derived date is None, and the recorded date
    stands. Without the marker filter the derived date would be "yesterday + 1 month"
    on every run and this doc would be immortal.
    """
    log = "2026-09-01T10:00:00+00:00\tchore(docs): bump expires [tracker-expiry] demo\n"
    touch = tx.last_touch("docs/trackers/demo.md", _REPO, _runner=lambda: log)
    assert touch is None
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 9, 2), touch)
    assert t.status == tx.Status.EXPIRED


# ------------------------------------------------------------------ lifecycle walk


def test_full_lifecycle_live_then_warn_then_expired():
    """Walk one untouched doc across its life; then prove a touch restarts it."""
    path = "docs/trackers/demo.md"
    created = date(2026, 7, 23)

    seen = [
        tx.evaluate(DOC, path, day, created).status
        for day in (
            date(2026, 7, 24),  # live
            date(2026, 8, 15),  # live, one day before the window
            date(2026, 8, 16),  # warn opens
            date(2026, 8, 23),  # warn, final day
            date(2026, 8, 24),  # expired
        )
    ]
    assert seen == [
        tx.Status.LIVE,
        tx.Status.LIVE,
        tx.Status.WARN,
        tx.Status.WARN,
        tx.Status.EXPIRED,
    ]

    # Someone records a sighting on the warn day. The doc goes back to LIVE, and the
    # date it would otherwise have died on is now unremarkable.
    rescued = tx.evaluate(DOC, path, date(2026, 8, 24), date(2026, 8, 16))
    assert rescued.status == tx.Status.LIVE
    assert rescued.expiry == date(2026, 9, 16)


# ----------------------------------------------------------------------- plan TSV


def test_plan_field_order_is_pinned():
    """The workflow destructures positionally; a reorder here silently miswires it.

    `IFS=$'\\t' read -r slug path status expiry warn_days origin needs_bump` in
    `.github/workflows/tracker-expiry.yml` depends on exactly this order. Swapping two
    fields would not error anywhere — the sweep would just act on the wrong value.
    """
    assert tx.PLAN_FIELDS == (
        "slug",
        "path",
        "status",
        "expiry",
        "warn_days",
        "origin",
        "needs_bump",
    )


def test_plan_row_is_tab_separated_in_field_order():
    t = tx.evaluate(DOC, "docs/trackers/demo.md", date(2026, 8, 20), date(2026, 8, 19))
    (row,) = tx.plan_rows([t])
    parts = row.split("\t")
    assert len(parts) == len(tx.PLAN_FIELDS)
    assert parts == [
        "demo",
        "docs/trackers/demo.md",
        "live",
        "2026-09-19",
        "7",
        "83",
        "yes",
    ]


def test_plan_row_never_contains_a_tab_or_newline_inside_a_field():
    """The format's only correctness requirement — no value may carry a separator."""
    for t in tx.scan(_REPO, date(2026, 7, 23)):
        (row,) = tx.plan_rows([t])
        assert row.count("\t") == len(tx.PLAN_FIELDS) - 1
        assert "\n" not in row


def test_plan_row_marks_a_missing_origin_as_empty_not_none():
    doc = "---\nexpires: 2026-08-23\n---\n\nbody\n"
    t = tx.evaluate(doc, "docs/trackers/x.md", date(2026, 7, 1), None)
    (row,) = tx.plan_rows([t])
    # Empty field, not the string "None" — the workflow falls back to #253 on empty.
    assert row.split("\t")[5] == ""


# --------------------------------------------------------------- reference stripping


def test_strip_tracker_refs_removes_only_the_matching_slug():
    text = (
        "keep this line\n"
        "drop me  <!-- tracker-ref: self-collision-class -->\n"
        "keep me   <!-- tracker-ref: some-other-tracker -->\n"
        "keep this too\n"
    )
    out = tx.strip_tracker_refs(text, "self-collision-class")
    assert "drop me" not in out
    assert "some-other-tracker" in out
    assert out.count("\n") == 3


def test_strip_tracker_refs_is_a_no_op_when_absent():
    text = "nothing marked here\n"
    assert tx.strip_tracker_refs(text, "self-collision-class") == text


# ------------------------------------------------------- the shipped doc itself


def test_the_shipped_tracker_conforms_to_the_contract():
    """A guard you have not watched fail is a rumour — pin the real doc, not a fixture.

    If `self-collision-class.md` is edited into a shape the mechanism cannot read, the
    doc would silently stop expiring. This fails instead. It is skipped once the
    tracker has expired and been deleted, which is the designed end state, not a break.
    """
    p = _REPO / "docs" / "trackers" / "self-collision-class.md"
    if not p.exists():
        pytest.skip("tracker already expired and was deleted — the designed end state")
    t = tx.evaluate(p.read_text(encoding="utf-8"), "docs/trackers/self-collision-class.md",
                    date(2026, 7, 23), None)
    assert t is not None, "the shipped tracker no longer parses as a tracker"
    assert t.slug == "self-collision-class"
    assert t.recorded is not None
    assert t.warn_days == 7
    assert t.origin == "83"


def test_the_pattern_readme_is_not_itself_a_tracker():
    p = _REPO / "docs" / "trackers" / "README.md"
    assert tx.evaluate(p.read_text(encoding="utf-8"), "docs/trackers/README.md",
                       date(2026, 7, 23), None) is None


def test_scan_finds_the_shipped_tracker_and_skips_the_readme():
    slugs = {t.slug for t in tx.scan(_REPO, date(2026, 7, 23))}
    assert "README" not in slugs
