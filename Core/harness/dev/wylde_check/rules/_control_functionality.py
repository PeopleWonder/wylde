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

So the rule shipped at **error** with a per-file grandfather budget,
``GRANDFATHERED_UNROUTED`` — the 140 sites that existed at the pilot — that
reported zero on today's tree while failing the build on any *new* bypass.
It was a ratchet, not an exemption list: a count *below* budget also flagged,
so an allowlist nobody tightens could not rust open (rule 20's
``_FILE_SIZE_QUEUED_SPLITS`` precedent). Batch by batch it drained to empty.

**The ratchet is now removed (#247 endgame).** Every interactive site is
routed, and the deferred stateful-panel walks have all landed, so the budget
served its purpose and is gone: any bypass is a finding, full stop, with the
per-site ``control-ok`` marker as the only escape hatch. Its companion, rule
61 (:func:`check_every_control_building_crate_is_walked`), added in the same
change, closes the other half — it makes a *walk itself* mandatory for every
control-building crate, so a panel can neither ship an unrouted control nor
ship routed-but-unwalked. Dead handler bodies never had a budget — the tree
has zero, so any new one is red on arrival.

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

import os
import re
from collections import defaultdict
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

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

#: The grandfather ratchet is GONE (#247 endgame).
#:
#: It began at 140 sites / 28 files at the pilot and came down batch by batch —
#: Memory/Changelog, Dashboard/RemoteAccess, Settings (incl. `per_tool_row`, a
#: control rule 59 could not see because its handler is attached by the caller),
#: Workspaces, Models/Devices, Chat, and finally the Shell — until it was
#: drained to empty (part 2 batch 8). The mechanism was then kept, empty, only
#: until the deferred stateful-panel walks landed (the maintainer's "migrated
#: AND walked" condition). They have (Models, Devices, Chat, the Workspaces
#: sub-views + graph, the Shell chrome), so the budget and its ratchet branch
#: are deleted here together with the addition of rule 61
#: (:func:`check_every_control_building_crate_is_walked`).
#:
#: There is now **no budget**: every interactive `.id(` that bypasses the
#: constructor is a finding, full stop, on the PR that adds it. A control that
#: does not route through `control()` never enters the per-frame registry the
#: walk enumerates, so it would ship unproven — which is exactly what #247
#: prevents. The escape hatch for a genuinely non-interactive id is the
#: per-site ``wylde-check: control-ok`` marker, not a file budget.

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
                ctx = raw_lines[line - 1].strip() if line - 1 < len(raw_lines) else ""
                unrouted.setdefault(rel, []).append((line, ctx))

    # ── Every unrouted interactive site is a finding ──
    #
    # No budget (the grandfather ratchet is gone — see GRANDFATHERED_UNROUTED's
    # removal note above). Any interactive `.id(` that bypassed the constructor
    # is reported on the PR that adds it: it never enters the per-frame registry
    # the walk enumerates, so it would ship unproven. One finding per site.
    for rel in sorted(unrouted):
        for line, ctx in unrouted[rel]:
            findings.append(
                Finding(
                    rule=RULE,
                    severity=SEVERITY,
                    file=rel,
                    line=line,
                    message=(
                        "this interactive control gets its id from a bare `.id(...)` "
                        'instead of `wylde_gui_controls::control(el, "id")`. A control '
                        "that does not route through the constructor never enters the "
                        "per-frame registry, so `tests/control_walk.rs` never enumerates "
                        "it and never clicks it — it ships unproven while the suite stays "
                        "green (#247). Replace `div().id(x)` with `control(div(), x)`; if "
                        f"this id is not a clickable control, mark it `// {_OPT_OUT}: "
                        "<reason>`."
                    ),
                    context=ctx,
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


# ── Rule 61: every control-building GUI crate is control-walked ──────────

RULE61 = "every_control_building_crate_is_walked"

#: An ``include_str!("path")`` — how a `control_walk` declares a source whose
#: literal control ids the walk must all paint (`.sources(&[include_str!(…)])`).
_INCLUDE_STR_RE = re.compile(r'include_str!\s*\(\s*"([^"]+)"\s*\)')

#: The two textual fingerprints of a control walk: the constructor call and its
#: living-controls assertion. A crate file carrying either is walk code.
_WALK_MARKER_RE = re.compile(r"\bControlWalk::new\b|\bassert_every_control_lives\b")

#: The gate attribute whose item never ships. A `control()` call inside a
#: ``#[cfg(test)]`` block is a walk *fixture*, not a shipped control, so those
#: blocks are cut before the shipped-control scan.
_CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")

#: The two infrastructure crates that are NOT panels and so require no walk: the
#: `control()` constructor + its static scanner, and the control-walk harness.
#: The scanner necessarily carries the literal string ``control(`` as scan
#: data, and the harness demonstrates the constructor in shipped helper code —
#: neither is a user-facing control a content heuristic can distinguish from a
#: real call, and neither renders UI, so both are excluded by path. Matched as a
#: repo-relative suffix of the crate root.
_RULE61_NON_PANEL_CRATES: Tuple[str, ...] = (
    "Core/GUI/Frontend/Controls",
    "Core/GUI/Frontend/test-support",
)


def _strip_cfg_test(code: str) -> str:
    """Remove ``#[cfg(test)]``-annotated braced items from ``code``.

    A `control()` call in a test module (e.g. an in-crate ``#[cfg(test)] mod
    control_walk``) builds a fixture, not a user-facing control, so it must not
    make the file count as control-building. Brace-matched from the attribute's
    following ``{``; a non-braced gated item (``#[cfg(test)] use …``) just has
    its attribute dropped. A lexical cut, like the rest of this suite — good
    enough for the well-formed tree, and a false *keep* only ever over-counts
    (a louder, safer failure than under-counting).
    """
    result = code
    while True:
        m = _CFG_TEST_RE.search(result)
        if not m:
            return result
        brace = result.find("{", m.end())
        if brace == -1:
            result = result[: m.start()] + result[m.end() :]
            continue
        depth = 0
        end = None
        for j in range(brace, len(result)):
            if result[j] == "{":
                depth += 1
            elif result[j] == "}":
                depth -= 1
                if depth == 0:
                    end = j
                    break
        if end is None:
            return result[: m.start()]
        result = result[: m.start()] + result[end + 1 :]


def _gui_crate_roots() -> List[Path]:
    """Every GUI crate root (a directory with a ``Cargo.toml``) under the GUI
    source roots."""
    return [p.parent for p in _walk((".toml",), GUI_ROOTS) if p.name == "Cargo.toml"]


def check_every_control_building_crate_is_walked() -> List[Finding]:
    """Rule 61 — a crate that builds interactive controls must control-walk them.

    Rule 59's companion, and the other half of the #247 guarantee. Rule 59
    proves every *site* routes through ``control()`` (so it enters the per-frame
    registry a walk enumerates). This proves the *walk exists and sees every
    control-building file*: for each GUI crate whose shipped ``src/`` builds a
    control, there must be a ``control_walk`` (an in-crate ``#[cfg(test)] mod``
    or a ``tests/control_walk.rs``) that declares **every** such file in a
    ``.sources(&[include_str!(…)])``.

    Without this, a panel could route all its controls correctly and still ship
    with no walk at all — or with a walk that silently omits one of its files —
    and both halves would read green. Together the two rules mean a control can
    neither bypass the registry nor sit in a file no walk's literal-id coverage
    assertion ever inspects.

    A crate that builds no shipped control is not required to have a walk (the
    ``control()`` constructor crate, the test-support harness, and the
    focus-surface widget crates like the text input / code editor, whose roots
    are ``.id()`` + ``control-ok`` rather than ``control()``, all fall out here
    for free — no name-based exclusion needed).
    """
    findings: List[Finding] = []

    crate_roots = _gui_crate_roots()
    if not crate_roots:
        return [
            Finding(
                rule=RULE61,
                severity="error",
                file="Core/GUI",
                line=0,
                message=(
                    "rule 61 found no GUI crates (no Cargo.toml under "
                    f"{GUI_ROOTS!r}). If the GUI tree moved, repoint GUI_ROOTS; do "
                    "not leave the rule inspecting nothing."
                ),
            )
        ]

    # Longest root first, so a file is assigned to its NEAREST enclosing crate
    # (a nested crate wins over its parent directory).
    roots_by_depth = sorted(crate_roots, key=lambda r: len(str(r)), reverse=True)

    def crate_of(path: Path) -> Optional[Path]:
        for r in roots_by_depth:
            try:
                path.relative_to(r)
                return r
            except ValueError:
                continue
        return None

    control_files: Dict[Path, Set[str]] = defaultdict(set)
    walk_sources: Dict[Path, Set[str]] = defaultdict(set)
    has_walk: Dict[Path, bool] = defaultdict(bool)

    for f in _walk((".rs",), GUI_ROOTS):
        cr = crate_of(f)
        if cr is None:
            continue
        rel_to_crate = str(f.relative_to(cr)).replace("\\", "/")
        raw = _read_text(f)
        if not raw:
            continue
        code = _strip_comments(raw)

        # Walk code — collect the `.rs` sources it declares (resolved relative
        # to the file holding the `include_str!`, then made crate-relative).
        if _WALK_MARKER_RE.search(code):
            has_walk[cr] = True
            for m in _INCLUDE_STR_RE.finditer(code):
                target = m.group(1)
                if not target.endswith(".rs"):
                    continue
                resolved = os.path.normpath(
                    os.path.join(os.path.dirname(str(f)), target)
                )
                rel = os.path.relpath(resolved, str(cr)).replace("\\", "/")
                if rel.startswith(".."):
                    continue  # points outside this crate — not one of its files
                walk_sources[cr].add(rel)

        # Shipped control-building file — a `control()` call in `src/`, outside
        # any `#[cfg(test)]` fixture block.
        if rel_to_crate.startswith("src/"):
            if _CONTROL_CALL_RE.search(_strip_cfg_test(code)):
                control_files[cr].add(rel_to_crate)

    for cr in sorted(crate_roots, key=lambda r: _to_rel(r)):
        cfiles = control_files.get(cr, set())
        if not cfiles:
            continue  # builds no controls — no walk required
        crate_rel = _to_rel(cr)
        if crate_rel in _RULE61_NON_PANEL_CRATES:
            continue  # constructor/scanner + walk harness — not panels

        if not has_walk.get(cr, False):
            findings.append(
                Finding(
                    rule=RULE61,
                    severity="error",
                    file=crate_rel,
                    line=0,
                    message=(
                        "this GUI crate builds interactive controls (in "
                        f"{sorted(cfiles)}) but has no control_walk. Add a "
                        "`tests/control_walk.rs` (or an in-crate `#[cfg(test)] mod "
                        "control_walk`) that mounts the panel, walks it with "
                        "`ControlWalk`, and asserts every control lives — otherwise "
                        "nothing proves a control in it does anything when clicked "
                        "(#247). If the crate is `wry`/tray-linked and the headless "
                        "job cannot build it, extract its renderers into a "
                        "walkable crate first, as the Shell chrome was."
                    ),
                )
            )
            continue

        declared = walk_sources.get(cr, set())
        for cf in sorted(cfiles):
            if cf not in declared:
                findings.append(
                    Finding(
                        rule=RULE61,
                        severity="error",
                        file=f"{crate_rel}/{cf}",
                        line=0,
                        message=(
                            "this file builds interactive controls but no "
                            "control_walk in the crate declares it in "
                            "`.sources(&[include_str!(…)])`. A walk only "
                            "coverage-checks the sources it is handed, so a control "
                            "id built here escapes `assert_covers_every_literal_id` "
                            "and could be added, or go dead, unseen (#247). Add "
                            f'`include_str!("…/{cf.split("/")[-1]}")` to the walk\'s '
                            "`.sources(&[…])`."
                        ),
                    )
                )

    return findings
