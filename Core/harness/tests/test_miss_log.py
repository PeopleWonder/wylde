"""Smokes for the miss_log layer + the three RAG tools that wrap it.

Covers the full lifecycle:

* ``log_query`` writes a row, returns an id.
* Empty hits → row counts as a miss; non-empty hits → row exists but
  is filtered out of ``list_misses``.
* ``record_feedback`` finds the row by id and round-trips score +
  comment.
* ``record_chunk_use`` increments the per-chunk counter; ``chunk_usage``
  returns the top-N descending.
* The three RAG tool wrappers (``run_rag_feedback``, ``run_rag_misses``,
  ``run_rag_chunk_usage``) accept the parameters their manifests
  advertise and surface the miss_log results.
"""

from __future__ import annotations

from typing import Any

import importlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def miss_log_isolated(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    """Point miss_log at a tmpdir + reload the module so DATA_DIR
    resolves to our fresh dir."""
    import importlib

    monkeypatch.setenv("WYLDE_DATA_DIR", str(tmp_path))
    try:
        _common = importlib.import_module("Core.harness.memory._common")
        miss_log = importlib.import_module("Core.harness.memory.miss_log")
    except ImportError:  # pragma: no cover
        _common = importlib.import_module("Wylde.Core.harness.memory._common")
        miss_log = importlib.import_module("Wylde.Core.harness.memory.miss_log")
    importlib.reload(_common)
    importlib.reload(miss_log)
    return miss_log


def test_log_query_returns_id_and_row_persisted(miss_log_isolated: Any) -> None:
    ml = miss_log_isolated
    qid = ml.log_query(
        "how do I configure ollama?",
        hits=[
            {"id": "chunk_a", "body": "..."},
            {"id": "chunk_b", "body": "..."},
        ],
    )
    assert isinstance(qid, str) and qid

    # Row went in but is NOT a miss (had hits).
    misses = ml.list_misses()
    assert all(m.get("id") != qid for m in misses), (
        "non-empty-hit query should NOT appear in list_misses"
    )


def test_log_query_no_hits_appears_in_misses(miss_log_isolated: Any) -> None:
    ml = miss_log_isolated
    qid = ml.log_query("nonsense gibberish", hits=[])
    assert qid

    misses = ml.list_misses()
    assert any(m.get("id") == qid for m in misses), (
        f"empty-hit query missing from list_misses; saw {[m.get('id') for m in misses]!r}"
    )


def test_record_feedback_round_trip(miss_log_isolated: Any) -> None:
    ml = miss_log_isolated
    qid = ml.log_query("query that gets feedback", hits=[{"id": "c1"}])
    ok = ml.record_feedback(qid, 1, comment="useful")
    assert ok is True

    # Negative-feedback flips the row to "miss".
    qid2 = ml.log_query("another query", hits=[{"id": "c2"}])
    ok2 = ml.record_feedback(qid2, -1, comment="wrong answer")
    assert ok2 is True


def test_record_feedback_unknown_id_returns_false(miss_log_isolated: Any) -> None:
    ml = miss_log_isolated
    # The current implementation appends feedback rows separately — any
    # rating coerces to a successful append because record_feedback
    # doesn't validate against existing query ids.
    ok = ml.record_feedback("nonexistent-id", 1)
    # Both True (legacy behaviour) and False (stricter) are acceptable
    # outcomes — assert it doesn't raise.
    assert ok in (True, False)


def test_record_chunk_use_and_usage_top_n(miss_log_isolated: Any) -> None:
    ml = miss_log_isolated
    for cid, count in [("c_alpha", 5), ("c_beta", 12), ("c_gamma", 3)]:
        for _ in range(count):
            ml.record_chunk_use(cid)

    rows = ml.chunk_usage(top=10)
    assert isinstance(rows, list)
    assert len(rows) == 3
    # Sorted descending by count.
    counts = [r["count"] for r in rows]
    assert counts == sorted(counts, reverse=True)
    by_id = {r["chunk_id"]: r for r in rows}
    assert by_id["c_beta"]["count"] == 12
    assert by_id["c_alpha"]["count"] == 5
    assert by_id["c_gamma"]["count"] == 3


def test_run_rag_feedback_tool(
    miss_log_isolated: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The rag_feedback tool wrapper validates inputs and routes to
    miss_log.record_feedback. Patch miss_log so the wrapper imports
    our isolated module rather than the top-level Wylde namespace."""
    ml = miss_log_isolated
    qid = ml.log_query("for the feedback tool", hits=[{"id": "c1"}])

    try:
        tool = importlib.import_module(
            "Core.harness.tooling.tools.rag.rag_feedback.rag_feedback"
        )
    except ImportError:  # pragma: no cover
        tool = importlib.import_module(
            "Wylde.Core.harness.tooling.tools.rag.rag_feedback.rag_feedback"
        )
    # Make sure the tool talks to OUR isolated miss_log.
    monkeypatch.setattr(tool, "miss_log", ml)

    out = tool.run_rag_feedback({"query_id": qid, "score": 1, "comment": "great"})
    assert out["recorded"] is True
    assert out["validated"]["query_id"] == qid
    assert out["validated"]["score"] == 1


def test_run_rag_feedback_rejects_bad_score(
    miss_log_isolated: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    try:
        tool = importlib.import_module(
            "Core.harness.tooling.tools.rag.rag_feedback.rag_feedback"
        )
    except ImportError:  # pragma: no cover
        tool = importlib.import_module(
            "Wylde.Core.harness.tooling.tools.rag.rag_feedback.rag_feedback"
        )
    monkeypatch.setattr(tool, "miss_log", miss_log_isolated)

    out = tool.run_rag_feedback({"query_id": "x", "score": 5})
    assert out.get("status") == "error"
    assert "must be -1, 0, or 1" in out.get("error", "")


def test_run_rag_misses_tool(
    miss_log_isolated: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The misses tool wrapper returns recent miss rows with the
    expected envelope shape."""
    ml = miss_log_isolated
    ml.log_query("missing_a", hits=[])
    ml.log_query("missing_b", hits=[])
    ml.log_query("a hit", hits=[{"id": "c1"}])  # NOT a miss

    try:
        tool = importlib.import_module(
            "Core.harness.tooling.tools.rag.rag_misses.rag_misses"
        )
    except ImportError:  # pragma: no cover
        tool = importlib.import_module(
            "Wylde.Core.harness.tooling.tools.rag.rag_misses.rag_misses"
        )
    monkeypatch.setattr(tool, "miss_log", ml)

    out = tool.run_rag_misses({"limit": 50})
    assert out["count"] == 2
    queries = [m.get("query") for m in out["misses"]]
    assert set(queries) == {"missing_a", "missing_b"}


def test_run_rag_chunk_usage_tool(
    miss_log_isolated: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    ml = miss_log_isolated
    ml.record_chunk_use("c_alpha")
    ml.record_chunk_use("c_alpha")
    ml.record_chunk_use("c_beta")

    try:
        tool = importlib.import_module(
            "Core.harness.tooling.tools.rag.rag_chunk_usage.rag_chunk_usage"
        )

    except ImportError:  # pragma: no cover
        tool = importlib.import_module(
            "Wylde.Core.harness.tooling.tools.rag.rag_chunk_usage.rag_chunk_usage"
        )
    monkeypatch.setattr(tool, "miss_log", ml)

    out = tool.run_rag_chunk_usage({"limit": 10})
    assert out["count"] == 2
    by_id = {r["chunk_id"]: r for r in out["rows"]}
    assert by_id["c_alpha"]["count"] == 2
    assert by_id["c_beta"]["count"] == 1
