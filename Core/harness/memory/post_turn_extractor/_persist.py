"""Verdict → memory-store mutation.

Owns :func:`_apply_verdict`, the side-effect step of the extractor.
Each verdict either writes a new long-term / workspace record,
supersedes an existing one, or no-ops. Failures from the underlying
stores are swallowed at the call site in ``_extract.extract_post_turn``;
this module just hands back a row describing what was written.
"""

from __future__ import annotations

from typing import Any, Dict, Optional, TYPE_CHECKING

from .. import long_term as _long_term
from .. import workspace_memory as _ws_mem

# workspaces: removed in config-file-backed redesign (2026-06-05) —
# the Python workspace registry now lives in Rust; the save_workspace
# path no longer checks workspace existence before writing.
from ..long_term import LongTermMemory
from ..workspace_memory import WorkspaceMemory

if TYPE_CHECKING:
    from ._extract import Verdict


def _apply_verdict(
    v: "Verdict",
    conversation_id: str,
    turn_id: str,
    workspace_id: str,
) -> Optional[Dict[str, Any]]:
    """Execute one verdict against the live memory stores.

    Returns a row describing what was written (for the extractor's
    result list + the on_memory_written callback), or None if the
    verdict was a noop / not applicable.
    """
    source = f"post_turn:{conversation_id}/turn:{turn_id}"

    if v.action == "noop":
        return None

    record: LongTermMemory | WorkspaceMemory
    if v.action == "save_long_term":
        record = _long_term.save(
            body=v.body,
            source=source,
            importance=v.importance,
            tags=["auto"],
        )
        return {
            "scope": "long_term",
            "memory_id": record.id,
            "body": record.body,
            "importance": record.importance,
            "action": "save_long_term",
        }

    if v.action == "save_workspace":
        if not workspace_id:
            # Fall back to long-term — the user's intent ("remember
            # this") still holds even if no workspace was bound.
            record = _long_term.save(
                body=v.body,
                source=source,
                importance=v.importance,
                tags=["auto"],
            )
            return {
                "scope": "long_term",
                "memory_id": record.id,
                "body": record.body,
                "importance": record.importance,
                "action": "save_workspace_fallback_long_term",
            }
        # workspaces: removed in config-file-backed redesign (2026-06-05) —
        # workspace existence is no longer verified here (Rust owns the
        # registry); a bound workspace_id is treated as resolvable.
        record = _ws_mem.save(
            workspace_id=workspace_id,
            body=v.body,
            source=source,
            importance=v.importance,
        )
        return {
            "scope": "workspace",
            "memory_id": record.id,
            "body": record.body,
            "importance": record.importance,
            "workspace_id": workspace_id,
            "action": "save_workspace",
        }

    if v.action == "supersede":
        if not v.target_id:
            return None
        # We don't know which scope the target_id lives in without
        # looking it up. Try long-term first, then workspace.
        try:
            updated = _long_term.update(
                v.target_id,
                body=v.body,
                importance=v.importance,
                source=source,
            )
        except Exception:  # noqa: BLE001
            updated = None
        if updated is not None:
            return {
                "scope": "long_term",
                "memory_id": updated.id,
                "body": updated.body,
                "importance": updated.importance,
                "supersedes": v.target_id,
                "action": "supersede",
            }
        if workspace_id:
            try:
                updated_ws = _ws_mem.update(
                    workspace_id,
                    v.target_id,
                    body=v.body,
                    importance=v.importance,
                )
            except Exception:  # noqa: BLE001
                updated_ws = None
            if updated_ws is not None:
                return {
                    "scope": "workspace",
                    "memory_id": updated_ws.id,
                    "body": updated_ws.body,
                    "importance": updated_ws.importance,
                    "workspace_id": workspace_id,
                    "supersedes": v.target_id,
                    "action": "supersede",
                }
        return None

    return None
