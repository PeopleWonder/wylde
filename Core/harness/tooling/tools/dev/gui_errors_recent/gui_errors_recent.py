"""gui_errors_recent — read recent Wylde GUI error events.

The Tauri GUI's error sink (``Core/GUI/src/lib/error_sink.ts``) POSTs
every normalized error event to the Gateway, which appends it as one
JSON line to repo-root ``logs/gui_errors.jsonl``. This tool reads that
log back tail-first (newest event first) so the LLM agent can answer
"what's wrong?" by looking at what actually broke in the desktop app.

Parameters (all optional):

* ``limit``    — max events to return; default 20, capped at 200.
* ``since``    — ISO8601 timestamp; only events at/after it are kept.
* ``severity`` — exact match on ``error`` / ``warn`` / ``info``.
* ``source``   — exact match on the error source enum.
* ``route``    — exact match on the GUI route the error occurred on.

Returns ``{events, count, total_in_log}`` — ``events`` newest-first
after filtering and limiting, ``count`` its length, ``total_in_log``
the number of valid records in the file before any filter.
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

_DEFAULT_LIMIT = 20
_MAX_LIMIT = 200


def _repo_root() -> Path:
    """Wylde repo root. Honours the ``WYLDE_ROOT`` env var, else derives
    it from this file's location — six directories below the root."""
    return Path(os.getenv("WYLDE_ROOT", Path(__file__).resolve().parents[6]))


def _log_path() -> Path:
    return _repo_root() / "logs" / "gui_errors.jsonl"


def _coerce_limit(raw: Any) -> int:
    try:
        value = int(raw)
    except (TypeError, ValueError):
        return _DEFAULT_LIMIT
    return max(1, min(value, _MAX_LIMIT))


def _parse_iso(value: Any) -> Optional[datetime]:
    """Parse an ISO8601 string into a timezone-aware UTC datetime.
    A naive timestamp is assumed to be UTC. Returns ``None`` when the
    value is missing or unparseable — callers treat that as "skip"."""
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        dt = datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def _load_events() -> List[Dict[str, Any]]:
    """Every valid JSON-object record in the log, in file (chronological)
    order. Unparseable or non-object lines are skipped silently — a
    diagnostics tool must not choke on a single corrupt line."""
    path = _log_path()
    if not path.exists():
        return []
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    events: List[Dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except (ValueError, TypeError):
            continue
        if isinstance(record, dict):
            events.append(record)
    return events


def run_gui_errors_recent(params: Dict[str, Any]) -> Dict[str, Any]:
    """Return recent GUI error events, newest first, after filtering."""
    params = params or {}
    limit = _coerce_limit(params.get("limit", _DEFAULT_LIMIT))
    since = _parse_iso(params.get("since"))
    severity = params.get("severity")
    source = params.get("source")
    route = params.get("route")

    events = _load_events()
    total_in_log = len(events)

    matched: List[Dict[str, Any]] = []
    # Tail-first: the file is append-ordered, so the last line is newest.
    for record in reversed(events):
        if severity and record.get("severity") != severity:
            continue
        if source and record.get("source") != source:
            continue
        if route and record.get("route") != route:
            continue
        if since is not None:
            event_ts = _parse_iso(record.get("timestamp_iso"))
            if event_ts is None or event_ts < since:
                continue
        matched.append(record)
        if len(matched) >= limit:
            break

    return {
        "events": matched,
        "count": len(matched),
        "total_in_log": total_in_log,
    }
