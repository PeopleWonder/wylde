"""Tool-related rules: manifest id/name regex, manifest description required."""

from __future__ import annotations

import json
import sys as _sys
from typing import List

from .. import Finding
from .._config import TOOL_ID_RE
from .._walkers import _is_excluded, _read_text, _to_rel

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 3: tool id / name regex ──────────────────────────────────────


def check_tool_id_regex() -> List[Finding]:
    """Validate ``tools/<group>/<id>/manifest.json`` ``id`` / ``name`` fields.

    Language-agnostic: the rule reads every ``manifest.json`` under any
    ``**/tools/**`` path, regardless of whether the implementation behind
    it is Python, Rust, JS, or a shell wrapper.  No AST involvement; the
    manifest is the single source of truth for tool identity.
    """
    out: List[Finding] = []
    # Walk all manifest.json under tools/ directories.
    for manifest_path in _pkg.WYLDE_ROOT.rglob("manifest.json"):
        if _is_excluded(manifest_path):
            continue
        rel = _to_rel(manifest_path)
        # Tool manifests live under <something>/tools/<group>/<id>/manifest.json
        # AND under extensions: <ext>/tools/<id>/manifest.json.
        if "/tools/" not in rel:
            continue
        try:
            data = json.loads(_read_text(manifest_path))
        except (ValueError, TypeError):
            out.append(
                Finding(
                    rule="tool_id_regex",
                    severity="error",
                    file=rel,
                    line=0,
                    message="manifest.json is not valid JSON",
                )
            )
            continue
        if not isinstance(data, dict):
            continue
        for field_name in ("id", "name"):
            value = data.get(field_name)
            if value is None:
                out.append(
                    Finding(
                        rule="tool_id_regex",
                        severity="error",
                        file=rel,
                        line=0,
                        message=f"manifest is missing required field {field_name!r}",
                    )
                )
                continue
            if not isinstance(value, str) or not TOOL_ID_RE.match(value):
                out.append(
                    Finding(
                        rule="tool_id_regex",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"manifest {field_name}={value!r} doesn't match the "
                            f"canonical regex /^[a-z][a-z0-9_]*(?:\\.[a-z][a-z0-9_]*)*$/"
                        ),
                    )
                )
    return out


# ── Rule 12: tool manifest description required ──────────────────────


def check_tool_docstring_required() -> List[Finding]:
    """Every tool manifest must carry a non-empty ``description`` ≥20 chars.

    The harness surfaces the description to the LLM as the tool's
    contract.  Rule 3 (:func:`check_tool_id_regex`) validates id/name;
    this rule owns the description-quality bar.

    Language-agnostic: only ``manifest.json`` is read.  A tool whose
    implementation is Rust, JS, or a shell wrapper still passes as long
    as its manifest carries a substantive description.  The old AST
    walk of Python tool files was retired in W1.5 — manifest is the
    single source of truth.
    """
    out: List[Finding] = []
    for manifest_path in _pkg.WYLDE_ROOT.rglob("manifest.json"):
        if _is_excluded(manifest_path):
            continue
        rel = _to_rel(manifest_path)
        if "/tools/" not in rel:
            continue
        try:
            data = json.loads(_read_text(manifest_path))
        except (ValueError, TypeError):
            # tool_id_regex already flags malformed JSON; don't double-fire.
            continue
        if not isinstance(data, dict):
            continue
        description = data.get("description")
        if not isinstance(description, str) or not description.strip():
            out.append(
                Finding(
                    rule="tool_docstring_required",
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        "Tool manifest is missing a non-empty 'description'.  "
                        "The harness surfaces this string to the LLM as the "
                        "tool's contract — describe what the tool does, what "
                        "arguments it takes, and what it returns."
                    ),
                )
            )
            continue
        if len(description.strip()) < 20:
            out.append(
                Finding(
                    rule="tool_docstring_required",
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        f"Tool manifest 'description' is too short "
                        f"({len(description.strip())} chars; need ≥20).  "
                        f"Describe what the tool does, what arguments it "
                        f"takes, and what it returns — the harness uses this "
                        f"as the LLM's contract."
                    ),
                )
            )
    return out
