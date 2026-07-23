"""The tracker pointer must degrade to silence, not to a broken path (#253/#83).

A tracker doc under ``docs/trackers/`` is designed to be deleted once its subject goes
quiet. Every reference to one therefore has to survive its target's disappearance. This
file pins that: absent doc -> empty string, and rule 56 still emits its finding with the
message merely one sentence shorter.

The failure this exists to prevent is the one the linter has hit twice already (#101,
#116): a rule pointing at a file that no longer exists. Here the file's removal is
*scheduled*, so the dangling reference would be a certainty rather than a risk.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .conftest import _write

SLUG = "self-collision-class"


def _tr(wc: Any) -> Any:
    return wc.rules._tracker_ref


def _make_tracker(root: Path, slug: str = SLUG) -> Path:
    p = root / "docs" / "trackers" / f"{slug}.md"
    _write(p, "---\nexpires: 2026-08-23\n---\n\nbody\n")
    return p


def _arm_rule_56(root: Path) -> None:
    """Two ignored live-graph tests in one binary, no DB_LOCK, and a CI file that
    never runs it — arms both arms of rule 56."""
    _write(
        root / "rust" / "crates" / "wylde-harness" / "tests" / "live_demo.rs",
        "//! Live graph binary.\n"
        "static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());\n"
        "\n"
        "#[tokio::test]\n"
        '#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]\n'
        "async fn round_trip_a() {\n"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
        "\n"
        "#[tokio::test]\n"
        '#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]\n'
        "async fn round_trip_b() {\n"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n",
    )
    # A CI file whose live-graph leg never runs this stem, so the coverage arm fires too.
    _write(root / ".github" / "workflows" / "ci.yml", "jobs:\n  live-graph:\n    steps: []\n")


def test_pointer_is_empty_when_the_tracker_is_absent(isolated_tree):
    """The designed end state: the doc expired and was deleted. Nothing dangles."""
    wc, _root = isolated_tree
    assert _tr(wc).tracker_exists(SLUG) is False
    assert _tr(wc).tracker_pointer(SLUG) == ""


def test_pointer_names_the_doc_when_present(isolated_tree):
    wc, root = isolated_tree
    _make_tracker(root)
    out = _tr(wc).tracker_pointer(SLUG)
    assert out != ""
    assert "docs/trackers/self-collision-class.md" in out
    assert "self-expiring" in out


def test_pointer_survives_the_doc_being_deleted_mid_run(isolated_tree):
    """Same interpreter, doc removed underneath it — no exception, just silence."""
    wc, root = isolated_tree
    p = _make_tracker(root)
    assert _tr(wc).tracker_pointer(SLUG) != ""
    p.unlink()
    assert _tr(wc).tracker_pointer(SLUG) == ""


def test_tracker_path_is_stable_regardless_of_existence(isolated_tree):
    wc, _root = isolated_tree
    assert _tr(wc).tracker_path(SLUG) == "docs/trackers/self-collision-class.md"


def test_the_tracker_is_not_registered_as_a_rule_target(isolated_tree):
    """Rule 51 reds on a missing target — a tracker there is a scheduled outage.

    This is the trap the whole presence-gated design exists to dodge, so it is pinned
    rather than left to a code comment.
    """
    wc, _root = isolated_tree
    listed = [s for s in wc.rules._selfcheck.RULE_TARGET_SPECS if "docs/trackers" in str(s)]
    assert listed == [], (
        "a docs/trackers path is registered in RULE_TARGET_SPECS; rule 51 will red the "
        "build the day that tracker expires. Use tracker_pointer() instead."
    )


def test_rule_56_still_fires_without_the_tracker(isolated_tree):
    """The finding is the gate; the pointer is a nicety. Losing it must not lose the gate."""
    wc, root = isolated_tree
    _arm_rule_56(root)

    without = wc.check_graph_test_serialized_on_db_lock()
    assert without, "rule 56 emitted nothing — the fixture no longer arms it"
    assert all("docs/trackers" not in f.message for f in without)

    _make_tracker(root)
    with_doc = wc.check_graph_test_serialized_on_db_lock()
    assert len(with_doc) == len(without), "the pointer changed how many findings fire"
    assert any("docs/trackers" in f.message for f in with_doc)
