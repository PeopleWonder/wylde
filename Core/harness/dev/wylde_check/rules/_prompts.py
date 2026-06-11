"""Prompt-management rules (prompt-engineering improvement plan B11).

* :func:`check_no_hardcoded_prompts_rust` — new LLM system-prompt
  string literals (``"You are ...``) in Rust source must live in the
  prompts catalog (``rust/crates/wylde-harness/src/prompts/catalog.json``,
  resolved via ``store::effective_prompt``), not as hardcoded ``&str``
  constants.  The override/preset infrastructure is shipped; a hardcoded
  prompt is a prompt the user cannot tune without a rebuild.

A small **grandfather allowlist** covers the pre-B9 offenders (the chat
base prompt, the memory curator, the memory consolidator).  B9 migrates
them into the catalog and empties the list — do NOT add new entries
without an architectural reason.  Test fixtures may suppress the rule
with an inline ``// wylde-check: prompt-literal-ok`` marker.
"""

from __future__ import annotations

import sys as _sys
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel
from ._rust import _is_doc_or_comment, _walk_rust_sources

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]

# Grandfathered prompt sites.
#
# The three harness files are pre-B9 offenders: B9 (prompts→catalog
# migration) moves them into catalog.json and removes them from this
# list.
#
# ``wylde-ext-study`` is grandfathered on ARCHITECTURAL grounds, not as
# B9 debt: the prompts catalog/store is harness-internal, and Wylde
# crates may only reach each other via wylde-shared/IPC (rule 26) — an
# extension crate cannot call ``store::effective_prompt``.  If extension
# prompts ever need tuning, the catalog store moves to wylde-shared (or
# a ``prompts.get`` verb) first.
PROMPT_LITERAL_ALLOWLIST = frozenset(
    {
        "rust/crates/wylde-harness/src/turn/prompt.rs",
        "rust/crates/wylde-harness/src/memory/workspace/mod.rs",
        "rust/crates/wylde-harness/src/memory/long_term/reflection.rs",
        "rust/crates/wylde-ext-study/src/tools.rs",
    }
)

_PROMPT_LITERAL_NEEDLE = '"You are '
_SUPPRESS_MARKER = "wylde-check: prompt-literal-ok"


def check_no_hardcoded_prompts_rust() -> List[Finding]:
    """Flag ``"You are ...`` string literals in Rust source outside the
    prompts catalog.  System prompts belong in
    ``prompts/catalog.json`` + ``store::effective_prompt`` so the
    shipped override/preset surface can tune them without a rebuild.
    """
    out: List[Finding] = []
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        if rel in PROMPT_LITERAL_ALLOWLIST:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if _SUPPRESS_MARKER in line:
                continue
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            if _PROMPT_LITERAL_NEEDLE not in line:
                continue
            out.append(
                Finding(
                    rule="no_hardcoded_prompts_rust",
                    severity="error",
                    file=rel,
                    line=lineno,
                    message=(
                        "Hardcoded LLM system-prompt literal.  Add an entry "
                        "to rust/crates/wylde-harness/src/prompts/"
                        "catalog.json and resolve it via "
                        "store::effective_prompt() so the prompt-override "
                        "surface can tune it without a rebuild.  Test "
                        "fixtures may suppress with "
                        "`// wylde-check: prompt-literal-ok`."
                    ),
                    context=line.strip()[:200],
                )
            )
    return out
