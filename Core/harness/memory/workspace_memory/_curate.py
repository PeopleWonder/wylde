"""LLM-driven curation: sweep memories, ask for keep / supersede / merge verdicts.

Curation is opt-in (no chat_fn → ``skipped=True``). When invoked
directly by the scheduler or a Python caller, it batches the live
records, asks the curator LLM for verdicts, and applies them via
supersession (preserving the audit trail in the JSON store):

* ``keep``      — no store mutation, just an entry in the result.
* ``supersede`` — point the original's ``superseded_by`` at a
  tombstone marker so :func:`list_records(include_superseded=True)`
  still surfaces it but the default surface hides it.
* ``merge``     — write a new merged record, link every cited
  original's ``superseded_by`` to the new id, propagating entities.

Pipe action returns ``skipped=True`` because ``chat_fn`` isn't
injectable across the wire — only direct Python callers (scheduler,
tests) get to run a real curation pass.
"""

from __future__ import annotations

import json
import logging
import secrets
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional

from ._store import WorkspaceMemory, _lance_upsert, _load, _save, list_records, save

logger = logging.getLogger("wylde.harness.memory.workspace")


@dataclass
class CurationResult:
    """Per-batch verdicts the curator applied. Returned by :func:`curate`
    so callers can audit what the LLM did."""

    workspace_id: str
    inputs_considered: int
    kept: List[str] = field(default_factory=list)
    superseded: List[Dict[str, Any]] = field(default_factory=list)  # {old_id, reason}
    merged: List[Dict[str, Any]] = field(
        default_factory=list
    )  # {new_id, old_ids, reason}
    skipped: bool = False
    skip_reason: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "workspace_id": self.workspace_id,
            "inputs_considered": self.inputs_considered,
            "kept": list(self.kept),
            "superseded": [dict(s) for s in self.superseded],
            "merged": [dict(m) for m in self.merged],
            "skipped": self.skipped,
            "skip_reason": self.skip_reason,
        }


_CURATION_SYSTEM = (
    "You are a memory curator. You read a list of memory records about "
    "a project and decide which are still relevant for ongoing work and "
    "which are stale, redundant, or no longer important.\n\n"
    "Output ONE JSON object per line, no preamble, no trailing text. "
    "Each object refers to one input by its 1-based index and carries "
    "a verdict. Three verdict shapes:\n"
    '  {"index": 3, "verdict": "keep"}\n'
    '  {"index": 5, "verdict": "supersede", "reason": "<why>"}\n'
    '  {"index": 7, "verdict": "merge", "into": [3, 8], '
    '"new_body": "<consolidated paragraph>", "reason": "<why>"}\n\n'
    "Rules:\n"
    "* `keep` — the memory is still useful as-is.\n"
    "* `supersede` — the memory is no longer relevant. The reason field "
    "is required. The memory will be soft-deleted (still in history).\n"
    "* `merge` — combine multiple memories into one new entry. List the "
    "input indices in `into` (must include the current index). Provide "
    "the consolidated `new_body`. The originals will be marked superseded.\n\n"
    "Default to `keep` when uncertain. Be conservative — when in doubt, "
    "keep the memory."
)


CURATION_BATCH_SIZE = 20


def curate(
    workspace_id: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    model: Optional[str] = None,
    batch_size: int = CURATION_BATCH_SIZE,
) -> CurationResult:
    """Sweep the workspace's memories through the LLM in batches asking
    for keep / supersede / merge verdicts; apply them.

    Without a chat_fn this returns ``skipped=True`` (mirrors the
    reflection module — the pipe layer can't inject a chat_fn across
    the boundary; the scheduler / Python callers pass one in directly).

    Soft-delete via supersession preserves the audit trail: the old
    record stays on disk with ``superseded_by`` pointing at a tombstone
    marker so :func:`history` walks still surface it but the default
    list / search calls hide it.
    """
    if not isinstance(workspace_id, str) or not workspace_id:
        return CurationResult(
            workspace_id="",
            inputs_considered=0,
            skipped=True,
            skip_reason="empty workspace_id",
        )
    if chat_fn is None:
        return CurationResult(
            workspace_id=workspace_id,
            inputs_considered=0,
            skipped=True,
            skip_reason="no chat_fn supplied",
        )

    records = list_records(workspace_id, include_superseded=False)
    if not records:
        return CurationResult(
            workspace_id=workspace_id,
            inputs_considered=0,
            skipped=True,
            skip_reason="no records to curate",
        )

    result = CurationResult(workspace_id=workspace_id, inputs_considered=len(records))

    for batch_start in range(0, len(records), batch_size):
        batch = records[batch_start : batch_start + batch_size]
        verdicts = _ask_curator(chat_fn, batch, model)
        if verdicts is None:
            continue
        _apply_verdicts(workspace_id, batch, verdicts, result)

    logger.info(
        "workspace_memory: curated %s — %d kept, %d superseded, %d merged",
        workspace_id,
        len(result.kept),
        len(result.superseded),
        len(result.merged),
    )
    return result


def _ask_curator(
    chat_fn: Callable[..., Any],
    batch: List["WorkspaceMemory"],
    model: Optional[str],
) -> Optional[List[Dict[str, Any]]]:
    """Format the batch, ask the LLM for verdicts, parse line-by-line.

    Returns None on transport / parse failure so the outer loop can
    continue with the next batch instead of aborting curation
    entirely.
    """
    lines = []
    for i, m in enumerate(batch, start=1):
        lines.append(f"{i}. (importance {m.importance}, id {m.id}) {m.body}")
    user = "\n".join(lines)

    messages = [
        {"role": "system", "content": _CURATION_SYSTEM},
        {"role": "user", "content": user},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=model)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspace_memory: curator chat_fn raised: %s", exc)
        return None
    raw = getattr(step, "text", "") or ""
    verdicts: List[Dict[str, Any]] = []
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        # Tolerate code-fenced output too; strip backticks/JSON labels.
        if line.startswith("```"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and "index" in obj:
            verdicts.append(obj)
    return verdicts


_TOMBSTONE_PREFIX = "tombstone:"


def _apply_verdicts(
    workspace_id: str,
    batch: List["WorkspaceMemory"],
    verdicts: List[Dict[str, Any]],
    result: CurationResult,
) -> None:
    """Mutate the workspace memory store per the LLM's verdicts.

    * ``keep``      — append to result.kept; no store mutation.
    * ``supersede`` — mark the original ``superseded_by`` a tombstone id
      so the history walk still surfaces it but the default list /
      search hides it.
    * ``merge``     — write a new merged record, link every cited
      original's ``superseded_by`` to the new id.
    """
    by_index = {i: rec for i, rec in enumerate(batch, start=1)}
    by_id = {rec.id: rec for rec in batch}

    for verdict in verdicts:
        idx = verdict.get("index")
        kind = verdict.get("verdict") or verdict.get("action") or "keep"
        target = by_index.get(int(idx) if isinstance(idx, (int, str)) else -1)
        if target is None:
            continue

        if kind == "keep":
            result.kept.append(target.id)
            continue

        if kind == "supersede":
            reason = str(verdict.get("reason") or "curated as stale")
            tombstone_id = _TOMBSTONE_PREFIX + secrets.token_hex(8)
            _link_supersession(workspace_id, target.id, tombstone_id, reason=reason)
            result.superseded.append({"old_id": target.id, "reason": reason})
            continue

        if kind == "merge":
            into_indices = verdict.get("into") or []
            if not isinstance(into_indices, list):
                continue
            old_ids: List[str] = []
            for j in into_indices:
                rec = by_index.get(int(j) if isinstance(j, (int, str)) else -1)
                if rec is not None:
                    old_ids.append(rec.id)
            new_body = str(verdict.get("new_body") or target.body)
            if not old_ids or not new_body.strip():
                continue
            # Pick importance as max of inputs (synthesis is at least as
            # heavy as the heaviest input).
            new_importance = max(
                (by_id[oid].importance for oid in old_ids if oid in by_id),
                default=target.importance,
            )
            # Union entities to preserve the graph edges.
            ent_seen: set = set()
            ent_union: List[str] = []
            for oid in old_ids:
                rec = by_id.get(oid)
                if rec is None:
                    continue
                for e in rec.entities:
                    if e not in ent_seen:
                        ent_seen.add(e)
                        ent_union.append(e)

            new_record = save(
                workspace_id=workspace_id,
                body=new_body,
                source=f"curation:merge from {','.join(old_ids)}",
                importance=new_importance,
                entities=ent_union,
            )
            reason = str(verdict.get("reason") or "merged by curator")
            for oid in old_ids:
                _link_supersession(workspace_id, oid, new_record.id, reason=reason)
            result.merged.append(
                {
                    "new_id": new_record.id,
                    "old_ids": list(old_ids),
                    "reason": reason,
                }
            )


def _link_supersession(
    workspace_id: str,
    old_id: str,
    new_id: str,
    *,
    reason: str = "",
) -> None:
    """Set ``old.superseded_by = new_id``. Used by curator + reflection.

    Tombstone supersession (``new_id`` starting with ``tombstone:``)
    represents a soft-delete with audit trail — the original stays on
    disk for history walks but is hidden from default retrieval.
    """
    records = _load(workspace_id)
    for r in records:
        if r.id == old_id:
            r.superseded_by = new_id
            _save(workspace_id, records)
            try:
                _lance_upsert(r)
            except Exception:  # noqa: BLE001
                pass
            return
