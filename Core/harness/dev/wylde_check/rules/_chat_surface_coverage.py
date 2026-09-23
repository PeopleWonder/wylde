"""Rule 58: every GUI chat entry point is covered by the chat-turn e2e.

The failure this exists to prevent
----------------------------------

Chat is the product's primary path, and it has more than one entry point:
the global Chat panel and the Workspaces InferenceBar dock are *separate*
``ChatPanel`` singletons with different scoping rules.  ``tests/
chat_turn_e2e.rs`` (issue #236) drives each of them end-to-end — composer
to turn driver to rendered reply.

The risk is not that the test breaks.  It is that a **new** chat surface
is added and the test simply doesn't know about it: the suite stays
green, the coverage percentage silently drops, and the new surface ships
with no end-to-end proof that typing in it does anything at all.  That is
the same shape as #56 (behavioural tests nothing ran) and #101/#116 (a
rule pointing at a deleted file) — coverage that decays quietly rather
than going red.

The two halves of the enforcement
---------------------------------

Half of it lives in Rust and needs no rule: ``chat_turn_e2e.rs``'s
``spec()`` matches ``ChatScope`` **exhaustively**, so adding a variant
stops the test binary compiling and reds ``cargo panel-walk``.

This rule is the other half — the two cases the compiler cannot see:

1. **A variant added to the match but not to the covered list.**  A
   ``ChatScope`` arm can be added to ``spec()`` (satisfying the compiler)
   without adding an entry to ``COVERED``, so the loop never drives it.
   Checked by :func:`check_chat_surfaces_are_e2e_covered` comparing the
   enum's variants against the ``COVERED`` entries.

2. **A whole new chat composer somewhere else.**  A new panel growing its
   own chat bar adds no ``ChatScope`` variant at all, so the exhaustive
   match is blind to it.  Checked by scanning the GUI tree for
   send-capable chat composers and requiring each one's file to be
   declared in ``COVERED_COMPOSER_FILES``.

What counts as a "send-capable chat composer"
---------------------------------------------

A file that contains **both**:

* a text input built with ``SubmitMode::EnterSubmits`` (the mode that
  makes Enter fire ``InputEvent::Submit`` — a ``SubmitMode::Never`` field
  is a search/filter box and is never a chat bar), and
* a reference to the chat turn path (``submit_text``,
  ``send_user_message``, ``start_turn``, ``start_turn_with_model`` or
  ``chat.start_turn``).

``Core/GUI/Manifest/`` is out of scope on purpose: it is a shipped GUI crate,
but it renders no gpui UI at all (no ``impl Render``, no ``div()``) — it is
panel-registry codegen and manifest plumbing, so it cannot own a composer.

Requiring both keeps the scan honest in each direction: the Models panel's
model-search field is ``EnterSubmits`` but reaches no turn, and the Chat
panel's own ``chat.cancel`` plumbing reaches the turn path but is no
composer.  Neither is flagged.

What the scan covers, and the hole that was in it
-------------------------------------------------

The composer scan walks ``Core/GUI/{Frontend,Shell}/**/src/**/*.rs``.  Until
this was fixed, the ``Shell`` half of that was a lie: the path pattern was
``.*/src/``, which needs at least one segment between the crate root and
``src``.  ``Frontend/<crate>/src/…`` matched; ``Core/GUI/Shell/src/…`` — with
nothing in between — matched **nothing**.  All 13 Shell sources were invisible,
and the rule had never scanned one of them.

Nothing caught it because the failure is silent by construction: the walk
simply returns fewer files and the rule reports a clean pass.  The scan looked
healthy at 158 files while a whole named root was unreachable.  It surfaced
only when rule 59 (#247) hit the identical bug in a copy of the same pattern.

So the fix is two parts, and the second is the important one:

* the pattern is now ``(.*/)?src/``, which reaches a crate root's own ``src``; and
* :data:`GUI_SCAN_ROOTS` names every root the pattern claims, and each must
  contribute at least one scanned file or the rule errors.  Cardinality per
  root is the only thing that distinguishes "this root is clean" from "this
  root is unreachable" — the same distinction rule 51 draws for a whole rule's
  corpus, drawn here one level down.

The widening exposed no new finding: the Shell owns no ``SubmitMode::EnterSubmits``
input and reaches no chat turn path, so there was no hidden uncovered surface.
The guarantee was simply unenforced over it.

Like the rest of the suite this walks the active tree read-only and emits
``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List, Set, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Layout constants ─────────────────────────────────────────────────

RULE = "chat_surfaces_are_e2e_covered"

#: Where the ``ChatScope`` enum — the definition of "a chat surface" — lives.
SCOPE_FILE = "Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs"

#: The end-to-end test that must cover every surface.
E2E_FILE = "Core/GUI/Frontend/Panels/Chat/tests/chat_turn_e2e.rs"

#: GUI source roots the composer scan walks. Test sources are excluded — a
#: fixture composer in a test is not a shipped entry point.
#:
#: The ``(.*/)?`` is load-bearing. The original form was ``.*/src/``, which
#: requires at least one path segment between the crate root and ``src`` — so
#: it matched every ``Frontend/<crate>/src/…`` file but **no**
#: ``Core/GUI/Shell/src/…`` file at all, because the Shell has nothing in
#: between. The ``Shell`` alternation was dead from the day it was written:
#: the rule named the Shell in its scope and scanned 0 of its 13 sources.
#: Same bug, same fix as rule 59's matcher (#247).
_GUI_SRC_RE = re.compile(r"^Core/GUI/(Frontend|Shell)/(.*/)?src/.+\.rs$")

#: Every root ``_GUI_SRC_RE`` claims to cover. Each must contribute at least
#: one scanned file, or the rule reports the shortfall as an error.
#:
#: This is the guard that would have caught the above. A regex that names a
#: root it cannot reach fails *silently* — the walk just returns fewer files
#: and every rule downstream reports a clean pass, which is the #101/#114/#116
#: decay shape one level down: not a rule pointing at a deleted tree, but a
#: rule pointing at a live tree it cannot express a path to.
#:
#: `Core/GUI/Manifest/` is deliberately NOT here. It is a shipped GUI crate,
#: but it renders no gpui UI at all (no `impl Render`, no `div()`) — it is
#: panel-registry codegen and manifest plumbing, so it cannot own a chat
#: composer. Out of scope by intent, not by accident.
GUI_SCAN_ROOTS: Tuple[str, ...] = ("Core/GUI/Frontend", "Core/GUI/Shell")

# ── Matchers ─────────────────────────────────────────────────────────

#: `pub enum ChatScope { … }` — captured body, so the variants can be read.
_SCOPE_ENUM_RE = re.compile(r"\benum\s+ChatScope\s*\{(.*?)\n\}", re.DOTALL)

#: A variant name at the start of a line inside the enum body. Attribute
#: lines (`#[default]`) and doc comments are skipped by the leading-name
#: requirement.
_VARIANT_RE = re.compile(r"^\s*([A-Z]\w*)\s*(?:,|$)", re.MULTILINE)

#: `const COVERED: &[SurfaceSpec] = &[ … ];`
_COVERED_RE = re.compile(r"\bCOVERED\s*:\s*&\[SurfaceSpec\]\s*=\s*&\[(.*?)\]\s*;", re.DOTALL)

#: `spec(ChatScope::Global)` inside the COVERED list.
_COVERED_SCOPE_RE = re.compile(r"ChatScope::(\w+)")

#: `const COVERED_COMPOSER_FILES: &[&str] = &[ … ];`
_COMPOSER_FILES_RE = re.compile(
    r"\bCOVERED_COMPOSER_FILES\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\]\s*;", re.DOTALL
)

#: A quoted path inside that list.
_QUOTED_RE = re.compile(r'"([^"]+)"')

#: The composer signal: Enter actually submits.
_ENTER_SUBMITS_RE = re.compile(r"SubmitMode::EnterSubmits")

#: The turn-path signal.
_TURN_PATH_RE = re.compile(
    r"\b(submit_text|send_user_message|start_turn_with_model|start_turn)\b"
    r'|"chat\.start_turn"'
)


def _strip_line_comments(text: str) -> str:
    """Drop ``//`` line comments so a mention in prose isn't a match.

    Deliberately crude: block comments and string literals are left alone.
    Both directions are safe here — the rule requires *two* independent
    signals in a file before it flags anything, and the covered-file list
    is read from a ``const``, not from prose.
    """
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def _declared_scope_variants(source: str) -> List[str]:
    body = _SCOPE_ENUM_RE.search(source)
    if not body:
        return []
    return _VARIANT_RE.findall(body.group(1))


def _covered_scopes(source: str) -> Set[str]:
    body = _COVERED_RE.search(source)
    if not body:
        return set()
    return set(_COVERED_SCOPE_RE.findall(body.group(1)))


def _covered_composer_files(source: str) -> Set[str]:
    body = _COMPOSER_FILES_RE.search(source)
    if not body:
        return set()
    return set(_QUOTED_RE.findall(body.group(1)))


def _is_chat_composer(code: str) -> bool:
    """A file owning a send-capable chat composer carries both signals."""
    return bool(_ENTER_SUBMITS_RE.search(code)) and bool(_TURN_PATH_RE.search(code))


def check_chat_surfaces_are_e2e_covered() -> List[Finding]:
    """Rule 58 — see the module docstring."""
    import sys as _sys

    pkg = _sys.modules[__name__.rsplit(".", 2)[0]]
    findings: List[Finding] = []

    scope_path = pkg.WYLDE_ROOT / SCOPE_FILE
    e2e_path = pkg.WYLDE_ROOT / E2E_FILE

    # A missing corpus must go RED, not quiet (the #101/#116 lesson) —
    # deleting either file would otherwise disarm this rule silently.
    if not scope_path.is_file():
        return [
            Finding(
                rule=RULE,
                severity="error",
                file=SCOPE_FILE,
                line=0,
                message=(
                    "the ChatScope definition is missing — rule 58 cannot "
                    "enumerate chat surfaces. If the enum moved, repoint "
                    "SCOPE_FILE; do not leave the rule pointing at nothing."
                ),
            )
        ]
    if not e2e_path.is_file():
        return [
            Finding(
                rule=RULE,
                severity="error",
                file=E2E_FILE,
                line=0,
                message=(
                    "the all-surfaces chat-turn e2e is missing — every GUI chat "
                    "entry point is now covered by nothing (issue #236). Restore "
                    "it rather than deleting the gate."
                ),
            )
        ]

    scope_src = _read_text(scope_path)
    e2e_src = _read_text(e2e_path)

    variants = _declared_scope_variants(scope_src)
    if not variants:
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file=SCOPE_FILE,
                line=0,
                message=(
                    "could not parse any ChatScope variant — the enum's shape "
                    "changed and rule 58 is now checking nothing. Update "
                    "_SCOPE_ENUM_RE / _VARIANT_RE to match."
                ),
            )
        )

    covered = _covered_scopes(e2e_src)
    if not covered:
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file=E2E_FILE,
                line=0,
                message=(
                    "could not parse the COVERED surface list — rule 58 cannot "
                    "tell which chat surfaces are exercised end-to-end."
                ),
            )
        )

    # (1) Every declared surface is actually driven by the e2e.
    for variant in variants:
        if variant not in covered:
            findings.append(
                Finding(
                    rule=RULE,
                    severity="error",
                    file=E2E_FILE,
                    line=0,
                    message=(
                        f"chat surface ChatScope::{variant} is not in COVERED — "
                        f"it is a real place a user can chat, and no end-to-end "
                        f"test drives it. Add `spec(ChatScope::{variant})` to "
                        f"COVERED in {E2E_FILE} so the composer -> turn driver "
                        f"-> rendered reply path is proven for it too."
                    ),
                )
            )

    # A COVERED entry naming a scope that no longer exists means the list
    # rotted the other way — it would not compile, but say so precisely.
    for variant in sorted(covered - set(variants)):
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file=E2E_FILE,
                line=0,
                message=(
                    f"COVERED names ChatScope::{variant}, which is not declared "
                    f"in {SCOPE_FILE} — the covered list is stale."
                ),
            )
        )

    # (2) Every send-capable chat composer in the GUI tree is declared.
    declared_files = _covered_composer_files(e2e_src)
    if not declared_files:
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file=E2E_FILE,
                line=0,
                message=(
                    "COVERED_COMPOSER_FILES is missing or empty — the source "
                    "scan would pass vacuously, so a new chat bar anywhere in "
                    "the GUI would go uncovered and unnoticed."
                ),
            )
        )

    found_files: Set[str] = set()
    scanned_per_root = {root: 0 for root in GUI_SCAN_ROOTS}
    for path in _walk((".rs",), roots=("Core/GUI",)):
        rel = _to_rel(path)
        if not _GUI_SRC_RE.match(rel):
            continue
        for root in GUI_SCAN_ROOTS:
            if rel.startswith(root + "/"):
                scanned_per_root[root] += 1
        if not _is_chat_composer(_strip_line_comments(_read_text(path))):
            continue
        found_files.add(rel)
        if rel not in declared_files:
            findings.append(
                Finding(
                    rule=RULE,
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        "this file owns a send-capable chat composer (a "
                        "SubmitMode::EnterSubmits input that reaches the chat "
                        "turn path) but is not declared in "
                        f"COVERED_COMPOSER_FILES in {E2E_FILE}. A new place a "
                        "user can chat must be driven end-to-end before it "
                        "ships: give it a surface in COVERED and add this path "
                        "to COVERED_COMPOSER_FILES."
                    ),
                )
            )

    # A root the matcher claims to cover but reaches zero files in. Not a
    # hypothetical: `Shell` sat in this regex matching nothing at all, because
    # `.*/src/` cannot express `Core/GUI/Shell/src/…`. The scan looked healthy
    # — 158 files — while a whole named root was invisible. Cardinality per
    # root is the only thing that distinguishes "clean" from "unreachable".
    for root in GUI_SCAN_ROOTS:
        if scanned_per_root[root] == 0:
            findings.append(
                Finding(
                    rule=RULE,
                    severity="error",
                    file=root,
                    line=0,
                    message=(
                        f"the composer scan matched no file under {root}, which "
                        "_GUI_SRC_RE claims to cover. Either the tree moved or the "
                        "path pattern cannot express it — a chat bar added there "
                        "would be invisible to this rule. Fix _GUI_SRC_RE (or drop "
                        "the root from GUI_SCAN_ROOTS if it is genuinely gone); do "
                        "not leave a named root unreachable."
                    ),
                )
            )

    # A declared file that no longer holds a composer is stale bookkeeping —
    # and, worse, hides that a surface lost its send wiring entirely.
    for rel in sorted(declared_files - found_files):
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file=E2E_FILE,
                line=0,
                message=(
                    f"COVERED_COMPOSER_FILES lists {rel}, but no send-capable "
                    "chat composer was found there. Either the composer moved "
                    "(update the list) or the surface lost its send wiring "
                    "(fix that first)."
                ),
            )
        )

    return findings
