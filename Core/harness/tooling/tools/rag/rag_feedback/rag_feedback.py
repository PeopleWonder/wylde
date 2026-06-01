"""rag_feedback — attach +1/0/-1 feedback to a prior rag_ask answer.

Wired through :mod:`Wylde.Core.harness.memory.miss_log`. The new
JSONL log issues string ids (timestamp-hex + random suffix), not
integers — the legacy SQLite-era ``int(query_id)`` coercion would
reject every real id. We accept whatever string the caller passes.
"""

from __future__ import annotations

from typing import Any, Dict

from Core.harness.memory import miss_log


def run_rag_feedback(params: Dict[str, Any]) -> Dict[str, Any]:
    if "query_id" not in params:
        return {"status": "error", "error": "'query_id' parameter required"}
    if "score" not in params:
        return {"status": "error", "error": "'score' parameter required"}

    qid = params["query_id"]
    if not isinstance(qid, (str, int)) or qid in ("", None):
        return {
            "status": "error",
            "error": "'query_id' must be a non-empty string or int",
        }
    qid = str(qid)

    try:
        score = int(params["score"])
    except (TypeError, ValueError):
        return {"status": "error", "error": "'score' must be an integer"}

    if score not in (-1, 0, 1):
        return {"status": "error", "error": "'score' must be -1, 0, or 1"}

    comment = params.get("comment")
    recorded = miss_log.record_feedback(
        qid,
        score,
        comment if isinstance(comment, str) else None,
    )

    return {
        "validated": {"query_id": qid, "score": score},
        "recorded": bool(recorded),
    }
