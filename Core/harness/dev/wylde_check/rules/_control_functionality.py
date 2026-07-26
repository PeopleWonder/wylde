"""Rule 59: every interactive GUI control is wired and walkable.

The failure this exists to prevent
----------------------------------

The L7 panel-walk (#35) proves every panel *loads*.  Until #247 nothing
proved a control in it *does anything*.  A button could ship with an
empty handler, a handler wired to a method that no longer runs, or no
handler at all, and every gate stayed green — because no test in the tree
had ever clicked a GUI control through its real listener.

``tests/control_walk.rs`` is the runtime half of the answer: it draws the
panel, enumerates the controls that actually painted, clicks each one
through gpui hit-testing, and asserts an observable effect.  That catches
a dead control **that the walk knows about**.

This rule is the static half — the two cases the walk structurally
cannot see:

1. **A dead handler body.**  A listener whose closure body does nothing
   (empty, or only ``cx.notify()``, or ``todo!()``).  The walk catches
   this too, but only for panels that already have a ``control_walk`` and
   only on the branch the walk paints; the static form catches it
   everywhere, on the PR that writes it.

2. **An interactive site that bypasses the constructor.**  A control
   built with a bare ``.id(...)`` instead of
   ``wylde_gui_controls::control(...)`` never enters the per-frame
   registry, so the walk never enumerates it and never clicks it.  The
   suite stays green while coverage silently drops — the #56 / #101 /
   #116 decay shape, where a gate goes quiet rather than red.

The third case — a control that *is* registered and *did* paint but was
never walked — is a runtime property and is asserted inside
``control_walk.rs`` itself, not here.

Severity, and why there is a ratchet instead of a WARNING
--------------------------------------------------------

The obvious way to ship a rule against an unmigrated tree is at WARNING.
That does not work here: the ``wylde_check (full rule set)`` CI job fails
on **any** finding, error or warning alike (``.github/workflows/ci.yml``
— "must report zero findings", by design since #114, so that no rule is
merely documentation).  A WARN-only rule would red ``develop`` exactly as
hard as an error one, just less legibly.

So the rule ships at **error** with a per-file grandfather budget,
:data:`GRANDFATHERED_UNROUTED` — the 140 sites that existed at the pilot.
It reports zero on today's tree, and a **new** control that bypasses the
constructor fails the build on the PR that adds it.  That is the actual
goal of #247, delivered now rather than after the migration.

It is a ratchet, not an exemption list: a count *below* budget is also a
finding, telling you to lower the entry.  An allowlist nobody is required
to tighten rusts open.  Same precedent as rule 20's
``_FILE_SIZE_QUEUED_SPLITS``.  Dead handler bodies get no budget at all —
the tree has zero, so any new one is red on arrival.

Granularity, stated honestly
----------------------------

Which *function* counts as interactive is decided function-scoped, because
Rust control chains here are routinely split across statements::

    let mut button = div().id(toggle_id).px_2();
    if !pending {
        button = button.on_mouse_down(MouseButton::Left, cx.listener(..));
    }

There is no single expression carrying both the id and the handler to
match against.  So: a function containing an interaction handler is a
control-building function, and within it **every** bare ``.id(`` is
reported as its own finding.

That works because ``control()`` assigns the id *inside* the constructor
and emits no textual ``.id(`` at the call site — a migrated control
contributes zero.  An earlier draft compared the ``.id(`` count against
the ``control(`` count instead, which let a half-migrated function cancel
out to zero and read as fully done; the per-site form has no such blind
spot and gives an accurate remaining-site count for the migration.

The cost is that a non-interactive id — a scroll container, a
``uniform_list`` handle — sitting in a function that also builds a real
control is flagged.  That is what the ``wylde-check: control-ok`` marker
is for, and it is deliberately the rarer case to have to annotate.

It is still not airtight: a control could get its id from a helper defined
in a function with no handler of its own.  That residue is what
``control_walk.rs``'s own coverage assertion is for — the two halves are
meant to be read together.

Empty-scan guard
----------------

A rule that inspects nothing reports a pass.  This one goes **red** if
the GUI source walk matches no files or the tree contains no interaction
handlers at all, so a refactor that moves the GUI cannot disarm it
silently (the #114/#116 lesson, and the reason rule 51 exists).

Like the rest of the suite this walks the active tree read-only and emits
``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List, Optional, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Configuration ────────────────────────────────────────────────────

RULE = "gui_controls_are_wired_and_walkable"

#: Error, not warning.  In this repo a warning is not advisory: the
#: ``wylde_check (full rule set)`` CI job fails on **any** finding, error or
#: warning alike (see ``.github/workflows/ci.yml``).  A "WARN for now" rule
#: would therefore red ``develop`` exactly as hard as an error one — so the
#: unmigrated tree is handled by the grandfather ratchet below instead, which
#: reports zero today while making a *new* unrouted control fail immediately.
SEVERITY = "error"

#: Per-file budget of interactive controls that still bypass the constructor,
#: recorded at the #247 pilot (140 sites / 28 files) and drained since.
#: Batch 2 (#247 part 2): Memory + Changelog migrated -> 136 / 26.
#: Batch 3: Dashboard + RemoteAccess migrated -> 127 / 24.
#: Batch 4: Settings migrated (incl. per_tool_row, a control rule 59
#: could not see because its handler is attached by the caller) -> 120 / 23.
#: Batch 5: Workspaces (all 13 files, 49 sites) migrated -> 71 / 10.
#:
#: This is a **ratchet**, not an exemption list.  Findings are emitted when a
#: file's actual count goes *above* its budget (a new unrouted control — the
#: case #247 is for) and also when it drops *below* (migration progress the
#: table has not recorded).  Both directions matter: an allowlist nobody is
#: required to tighten rusts open, which is how a gate ends up protecting
#: nothing.  Same precedent as rule 20's ``_FILE_SIZE_QUEUED_SPLITS``.
#:
#: #247 part 2 empties this table file by file; when it is empty, delete it
#: and the ratchet branch with it — the rule then simply forbids the pattern.
GRANDFATHERED_UNROUTED = {
    "Core/GUI/Frontend/Code_editor/src/lib.rs": 1,
    "Core/GUI/Frontend/Input/src/lib.rs": 1,
    "Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs": 16,
    "Core/GUI/Frontend/Panels/Chat/src/composer_ui.rs": 17,
    "Core/GUI/Frontend/Panels/Chat/src/markdown.rs": 1,
    "Core/GUI/Shell/src/sidebar.rs": 1,
    "Core/GUI/Shell/src/slot.rs": 1,
    "Core/GUI/Shell/src/update_pill.rs": 5,
}

#: Dead handler bodies are NOT grandfathered.  The tree has zero of them, so
#: the budget is zero everywhere and any new one is red on arrival.

#: The GUI source roots this rule walks.
GUI_ROOTS: Tuple[str, ...] = ("Core/GUI/Frontend", "Core/GUI/Shell")

#: Only shipped panel/shell sources.  Test sources are excluded: a fixture
#: control in a test is not a control the user can click.
#:
#: The ``(.*/)?`` is load-bearing: the Shell's sources live at
#: ``Core/GUI/Shell/src/…`` with nothing between the crate root and ``src``,
#: so the ``.*/src/`` form used elsewhere in this suite silently matches no
#: Shell file at all. The Shell owns the nav chrome — sidebar, tab strip,
#: title bar — which is precisely the interactive surface a control gate must
#: not be blind to.
_GUI_SRC_RE = re.compile(r"^Core/GUI/(Frontend|Shell)/(.*/)?src/.+\.rs$")

#: The constructor every interactive control must route through.
_CONTROL_CALL_RE = re.compile(r"\bcontrol\s*\(")

#: A bare element id.  ``.id(`` on a builder chain is how a gpui element
#: becomes ``Stateful`` — i.e. addressable and interactive.
_BARE_ID_RE = re.compile(r"\.id\s*\(")

#: The interaction handlers that make an element a control the user can
#: act on.  Scroll/hover handlers are deliberately absent: they are not
#: things a user *clicks*, and a control walk cannot assert an effect for
#: them.
_HANDLER_RE = re.compile(
    r"\.on_(mouse_down|mouse_up|click|any_mouse_down|secondary_mouse_down|drag|drop)\s*\("
)

#: Bodies that do nothing.  ``cx.notify()`` alone is a repaint request
#: with no state to repaint — the classic "looks wired, is dead" handler.
_DEAD_BODY_RE = re.compile(r"^(?:\(\)|_?cx\.notify\(\);?|todo!\(\);?|unimplemented!\(\);?)?$")

#: Suppression marker, for the rare genuinely-non-interactive ``.id()``
#: (a scroll container, a ``uniform_list`` handle).  Must carry a reason.
_OPT_OUT = "wylde-check: control-ok"


def _strip_comments(text: str) -> str:
    """Drop ``//`` line comments and ``/* … */`` blocks.

    Prose that mentions ``on_mouse_down`` must not arm the rule — every
    module in this tree documents its own control wiring at length.
    """
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.DOTALL)
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def _line_of(source: str, index: int) -> int:
    return source.count("\n", 0, index) + 1


def _function_spans(code: str) -> List[Tuple[int, int]]:
    """``(start, end)`` offsets of every top-level ``fn`` body in ``code``.

    Brace-matched rather than regex'd, because a control-building helper
    is full of nested closures and braces.
    """
    spans: List[Tuple[int, int]] = []
    for m in re.finditer(r"\bfn\s+\w+", code):
        brace = code.find("{", m.end())
        if brace == -1:
            continue
        depth = 0
        for i in range(brace, len(code)):
            if code[i] == "{":
                depth += 1
            elif code[i] == "}":
                depth -= 1
                if depth == 0:
                    spans.append((m.start(), i))
                    break
    return spans


def _closure_body(code: str, handler_index: int) -> Optional[str]:
    """The body of the closure passed to the handler at ``handler_index``.

    Returns ``None`` when no braced closure body can be located, and the
    caller skips it — an unparsed shape must never be a false positive.
    That is distinct from returning ``""``, which means the body was found
    and is **empty**: the single most important case this rule catches, and
    the one an "empty string is falsy" shortcut would silently drop.
    """
    # Find the closure's `|args|` then its braced body.
    pipe = code.find("|", handler_index)
    if pipe == -1:
        return None
    close_pipe = code.find("|", pipe + 1)
    if close_pipe == -1:
        return None
    rest = code[close_pipe + 1 :]
    stripped = rest.lstrip()
    if not stripped.startswith("{"):
        return None
    start = len(rest) - len(stripped)
    depth = 0
    for i in range(start, len(rest)):
        if rest[i] == "{":
            depth += 1
        elif rest[i] == "}":
            depth -= 1
            if depth == 0:
                return rest[start + 1 : i].strip()
    return None


def check_gui_controls_are_wired_and_walkable() -> List[Finding]:
    """Rule 59 — see the module docstring."""
    findings: List[Finding] = []

    files = [
        p for p in _walk((".rs",), GUI_ROOTS) if _GUI_SRC_RE.match(_to_rel(p))
    ]

    # An empty corpus is a disarmed gate, not a pass (#114/#116).
    if not files:
        return [
            Finding(
                rule=RULE,
                severity="error",
                file="Core/GUI",
                line=0,
                message=(
                    "rule 59 matched no GUI source files — the walk roots are "
                    f"{GUI_ROOTS!r}. If the GUI tree moved, repoint GUI_ROOTS; "
                    "do not leave the rule inspecting nothing."
                ),
            )
        ]

    handler_sites = 0
    #: rel path → line numbers of every unrouted interactive id found.
    unrouted: dict = {}

    for path in files:
        rel = _to_rel(path)
        raw = _read_text(path)
        if not raw:
            continue
        code = _strip_comments(raw)
        raw_lines = raw.splitlines()

        for start, end in _function_spans(code):
            body = code[start:end]
            handlers = list(_HANDLER_RE.finditer(body))
            if not handlers:
                continue
            handler_sites += len(handlers)

            # ── (1) dead handler bodies ──
            for h in handlers:
                closure = _closure_body(body, h.end())
                if closure is None:
                    continue  # unparsed shape — never a false positive
                collapsed = " ".join(closure.split())
                if _DEAD_BODY_RE.match(collapsed):
                    line = _line_of(code, start + h.start())
                    findings.append(
                        Finding(
                            rule=RULE,
                            severity=SEVERITY,
                            file=rel,
                            line=line,
                            message=(
                                "this control's handler body does nothing — clicking it "
                                "cannot produce an observable effect, so it is dead on "
                                "arrival. Wire it to the panel method it is supposed to "
                                "call, or delete the control."
                            ),
                            context=(
                                raw_lines[line - 1].strip() if line <= len(raw_lines) else ""
                            ),
                        )
                    )

            # ── (2) interactive sites bypassing the constructor ──
            #
            # One finding per bare `.id(` site, not a per-function count.
            # `control()` gives its element an id *inside* the constructor and
            # emits no textual `.id(`, so a migrated control contributes zero
            # here — which means any `.id(` left in a function that handles
            # clicks is, by construction, a control that bypassed it. Counting
            # `.id(` against `control(` instead would let a half-migrated
            # function cancel out to zero and read as done.
            for m in _BARE_ID_RE.finditer(body):
                line = _line_of(code, start + m.start())
                # Per-site opt-out, for a genuinely non-interactive id (a
                # scroll container, a `uniform_list` handle) that happens to
                # sit in a function that also builds a real control.
                window = raw_lines[max(0, line - 3) : line]
                if any(_OPT_OUT in ln for ln in window):
                    continue
                unrouted.setdefault(rel, []).append(line)

    # ── The ratchet ──
    #
    # Compare what was found against the grandfathered budget, per file. Over
    # budget = a NEW unrouted control (red, on the PR that adds it). Under
    # budget = migration progress the table has not recorded (also red, but a
    # one-line edit) — a ratchet nobody is required to tighten rusts open.
    for rel in sorted(set(unrouted) | set(GRANDFATHERED_UNROUTED)):
        lines = unrouted.get(rel, [])
        budget = GRANDFATHERED_UNROUTED.get(rel, 0)
        if len(lines) > budget:
            findings.append(
                Finding(
                    rule=RULE,
                    severity=SEVERITY,
                    file=rel,
                    line=lines[budget] if budget < len(lines) else lines[0],
                    message=(
                        f"{len(lines) - budget} new interactive control(s) here get an id "
                        'from a bare `.id(...)` instead of `wylde_gui_controls::control(el, '
                        '"id")` '
                        f"(found {len(lines)} at lines {lines}, grandfathered budget "
                        f"{budget}). A control that does not route through the constructor "
                        "never enters the per-frame registry, so `tests/control_walk.rs` "
                        "never enumerates it and never clicks it — it ships unproven while "
                        "the suite stays green (#247). Replace `div().id(x)` with "
                        "`control(div(), x)`; if this id is not a clickable control, mark "
                        f"it `// {_OPT_OUT}: <reason>`. Do NOT raise the budget."
                    ),
                )
            )
        elif len(lines) < budget:
            findings.append(
                Finding(
                    rule=RULE,
                    severity=SEVERITY,
                    file=rel,
                    line=0,
                    message=(
                        f"GRANDFATHERED_UNROUTED is stale for this file: budget {budget}, "
                        f"actually {len(lines)} unrouted control(s). Lower the entry to "
                        f"{len(lines)}"
                        + (" (or delete it — this file is fully migrated)." if not lines else ".")
                        + " The ratchet only holds while it is tightened as migration lands."
                    ),
                )
            )

    # A tree with GUI sources but no interaction handlers at all means the
    # matchers stopped matching — again a disarmed gate, not a clean tree.
    if handler_sites == 0:
        findings.append(
            Finding(
                rule=RULE,
                severity="error",
                file="Core/GUI",
                line=0,
                message=(
                    f"rule 59 scanned {len(files)} GUI source files and found no "
                    "interaction handlers at all. The GUI certainly has buttons, so "
                    "the handler matcher has gone stale — fix _HANDLER_RE rather than "
                    "accepting a vacuous pass."
                ),
            )
        )

    return findings
