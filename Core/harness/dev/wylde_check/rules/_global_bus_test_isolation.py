"""Rule 60: a unit test that touches a process-global broadcast bus must
either own its channel or serialize on a test-module guard.

The failure this exists to prevent
----------------------------------

``cargo test`` runs a binary's tests on parallel threads in **one process**.
A ``static SENDER: OnceLock<broadcast::Sender<_>>`` is therefore shared by
every test in the crate at once: one test's publish lands in another test's
receiver, and an assertion about "the first event I receive" silently
becomes an assertion about "no sibling test published during my window" —
a scheduling coincidence, not a property.

That is what #246 was. ``wylde-workspaces``'s watcher published every
settled delta to a process-global ``event_bus()``, and
``delta_event_is_broadcast_on_dispatch`` asserted on the first event off a
``subscribe()``. Sibling tests in the same binary ran their own watcher
loops and dispatched their own paths, so the test failed ~17% of the time
at ``--test-threads=8`` (0/30 serial) — reddening PR #244, which touched an
entirely different crate, and training everyone to hit re-run.

Why rule 56 did not catch it
----------------------------

This is the same class as rule 56 (``graph_test_serialized_on_db_lock``,
#226) and the same umbrella (#83): *several tests in one binary contending
on one shared resource with nothing serializing them*. Rule 56 misses this
instance twice over, and both misses are structural rather than
accidental:

1. **Scope.** Rule 56 walks ``rust/crates/**/tests/*.rs`` — integration-test
   binaries only. #246 lived in a ``#[cfg(test)] mod tests`` inside
   ``src/``, which no self-collision rule looked at.

2. **The single-toucher carve-out.** Rule 56 deliberately skips a binary
   with fewer than two live-graph tests, on the reasoning that "one test
   can't self-collide". For a *bus* that reasoning does not hold, and #246
   is the counter-example: exactly **one** test called ``subscribe()``. Its
   colliders were tests that never mentioned the bus at all — they merely
   ran watcher loops, and the loop published. So this rule has no
   minimum-count carve-out: a single bus-touching test is in scope,
   because the other end of the collision is ordinary product code that
   any sibling test may drive.

What it enforces
----------------

For every Rust source that **defines** a process-global broadcast bus (a
``static … : OnceLock/Lazy/OnceCell<broadcast::Sender<…>>``, or a function
returning ``&'static broadcast::Sender<…>``), each ``#[cfg(test)]`` test in
that file that *touches* the bus must be isolated or serialized:

* **Touches** — the test body names the bus static, calls a bus accessor
  (``sender()`` / ``event_bus()`` / ``subscribe()``), or calls any
  same-file top-level function that transitively does (``publish()``,
  ``publish_active_conversation()``, and — the #246 shape — a ``run_loop``
  that published internally). The transitive arm is the point: the test
  that collides need not mention the bus.

* **Isolated (preferred)** — the test, or a test-module helper it calls,
  constructs its own ``broadcast::channel(…)``. The bus is injected rather
  than reached for, so no sibling can reach the test's channel however the
  tests are scheduled. This is what #246 shipped: ``run_loop`` takes its
  sender, production passes a clone of the global, tests pass a private
  one.

* **Serialized (acceptable)** — the test acquires a test-module-local
  ``Mutex`` guard, directly or via a same-module helper. This is the
  ``DB_LOCK`` pattern rule 56 enforces, and the ``TEST_GUARD`` / ``guard()``
  shape already used by ``conversation_bus.rs`` and ``model_bus.rs``. It
  costs parallelism and it is a convention every new test must remember,
  which is why injection is preferred where the callers can take a sender.

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import List, Optional, Set, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk
from ._tracker_ref import tracker_pointer

# ── Constants ────────────────────────────────────────────────────────

# The self-collision class (#83) this rule is the other arm of (rule 56 owns
# the `tests/`-binary arm). Its diagnosis home is a SELF-EXPIRING tracker doc,
# so the pointer is presence-gated: `tracker_pointer` returns "" once the doc
# is gone and the message simply loses a sentence. Do NOT register the doc in
# ``_selfcheck.RULE_TARGET_SPECS`` — that would red the build on the day the
# tracker is designed to disappear. See ``_tracker_ref``.
_CLASS_TRACKER = "self-collision-class"

# Roots that ship Rust with unit tests in `src/`. Integration binaries under
# `tests/` are rule 56's half of the class.
_ROOTS: Tuple[str, ...] = ("rust/crates", "Core/GUI")

# A process-global broadcast sender: the `static` cell, or an accessor whose
# return type hands out a `&'static` sender. The static may be declared
# inside its accessor (the `event_bus()` shape), so neither is anchored to
# column 0.
_BUS_STATIC_RE = re.compile(
    r"\bstatic\s+(?P<name>[A-Z_][A-Z0-9_]*)\s*:\s*"
    r"(?:OnceLock|OnceCell|Lazy)\s*<\s*(?:tokio::sync::)?broadcast::Sender\s*<"
)
_BUS_ACCESSOR_RE = re.compile(
    r"fn\s+(?P<name>\w+)\s*\([^)]*\)\s*->\s*&'static\s+(?:tokio::sync::)?broadcast::Sender\s*<"
)

# `#[cfg(test)] mod <name> {` — an inline unit-test module.
_CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_MOD_HEAD_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>\w+)\s*\{")
# `#[cfg(test)] mod <name>;` — a *file-backed* test module. Following this is
# load-bearing: a bus file large enough to trip rule 20's 700-line cap will
# have its tests split into a sibling file (as `watcher/tests.rs` was), and a
# rule that only understood the inline form would go quiet at exactly that
# moment — the #101/#116 "gate goes quiet rather than red" decay.
_MOD_DECL_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>\w+)\s*;")

_TEST_ATTR_RE = re.compile(r"#\[\s*(?:tokio::)?test\b")

# A fn head at a known indent (rustfmt, gated by G6, keeps items and their
# closing brace at the same column).
def _fn_head_re(indent: int) -> re.Pattern:
    return re.compile(
        r"^ {%d}(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
        r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+(?P<name>\w+)" % indent
    )


# Isolation: the test owns a channel it constructed.
_OWN_CHANNEL_RE = re.compile(r"\bbroadcast::channel\s*\(")

# Serialization: a `Mutex` guard local to the test module.
_LOCK_CALL_RE = re.compile(r"\.lock\s*\(")
_MUTEX_STATIC_RE = re.compile(
    r"\bstatic\s+(?P<name>[A-Z_][A-Z0-9_]*)\s*:\s*(?:std::sync::)?Mutex\s*<"
)

# Per-test opt-out, on the test's attribute block or its first lines.
_OPT_OUT = "wylde-check: global-bus-test-ok"


def _strip_comments(src: str) -> List[str]:
    """Drop ``//`` and ``/* … */`` comments, preserving line count, so a
    commented-out guard never counts and a doc line naming the bus never
    arms the rule. (Same treatment as rule 56.)"""
    out: List[str] = []
    in_block = False
    for raw in src.splitlines():
        buf: List[str] = []
        i, n = 0, len(raw)
        while i < n:
            if in_block:
                end = raw.find("*/", i)
                if end == -1:
                    i = n
                    break
                i, in_block = end + 2, False
                continue
            if raw.startswith("//", i):
                break
            if raw.startswith("/*", i):
                in_block = True
                i += 2
                continue
            buf.append(raw[i])
            i += 1
        # Keep the raw line's leading whitespace even when fully commented,
        # so indent-based item extents stay aligned.
        out.append("".join(buf))
    return out


class _Item:
    __slots__ = ("name", "attrs", "body", "start_line")

    def __init__(self, name: str, attrs: List[str], body: str, start_line: int):
        self.name = name
        self.attrs = attrs
        self.body = body
        self.start_line = start_line

    @property
    def is_test(self) -> bool:
        return any(_TEST_ATTR_RE.search(a) for a in self.attrs)


def _parse_items(lines: List[str], lo: int, hi: int, indent: int) -> List[_Item]:
    """Every ``fn`` item between ``[lo, hi)`` whose head sits at exactly
    ``indent`` columns, each carrying its preceding attribute block."""
    head_re = _fn_head_re(indent)
    pad = " " * indent
    close = pad + "}"
    items: List[_Item] = []
    attrs: List[str] = []
    i = lo
    while i < hi:
        line = lines[i]
        stripped = line.strip()
        if not stripped:
            i += 1
            continue
        if stripped.startswith("#[") and len(line) - len(line.lstrip()) == indent:
            attrs.append(stripped)
            i += 1
            continue
        m = head_re.match(line)
        if m:
            body_lines = [line]
            i += 1
            while i < hi and lines[i].rstrip() != close:
                body_lines.append(lines[i])
                i += 1
            if i < hi:
                body_lines.append(lines[i])
                i += 1
            items.append(_Item(m.group("name"), attrs, "\n".join(body_lines), i))
            # `start_line` should point at the head, not the tail.
            items[-1].start_line = i - len(body_lines) + 1
            attrs = []
            continue
        # Any other item at this indent ends a dangling attribute run.
        if len(line) - len(line.lstrip()) == indent:
            attrs = []
        i += 1
    return items


def _find_test_module(lines: List[str]) -> Optional[Tuple[int, int, int]]:
    """``(first_body_line, end_line_exclusive, item_indent)`` of the inline
    ``#[cfg(test)] mod … { … }`` block, or ``None``."""
    for i, line in enumerate(lines):
        if not _CFG_TEST_RE.search(line):
            continue
        # The `mod` head is the next non-blank, non-attribute line.
        j = i + 1
        while j < len(lines) and (
            not lines[j].strip() or lines[j].strip().startswith("#[")
        ):
            j += 1
        if j >= len(lines):
            continue
        m = _MOD_HEAD_RE.match(lines[j])
        if not m:
            continue
        mod_indent = len(lines[j]) - len(lines[j].lstrip())
        close = " " * mod_indent + "}"
        k = j + 1
        while k < len(lines) and lines[k].rstrip() != close:
            k += 1
        return (j + 1, k, mod_indent + 4)
    return None


def _file_backed_test_mods(lines: List[str], bus_path: Path) -> List[Path]:
    """Sibling files declared by a ``#[cfg(test)] mod <name>;`` in the bus
    file. ``foo/mod.rs`` and ``foo.rs`` both resolve children under ``foo/``.
    """
    out: List[Path] = []
    parent = (
        bus_path.parent
        if bus_path.name == "mod.rs"
        else bus_path.parent / bus_path.stem
    )
    for i, line in enumerate(lines):
        if not _CFG_TEST_RE.search(line):
            continue
        j = i + 1
        while j < len(lines) and (
            not lines[j].strip() or lines[j].strip().startswith("#[")
        ):
            j += 1
        if j >= len(lines):
            continue
        m = _MOD_DECL_RE.match(lines[j])
        if not m:
            continue
        name = m.group("name")
        for cand in (parent / f"{name}.rs", parent / name / "mod.rs"):
            if cand.exists():
                out.append(cand)
                break
    return out


def _callers_of(names: Set[str], body: str) -> bool:
    return any(re.search(r"\b" + re.escape(n) + r"\s*\(", body) for n in names)


def _transitive_touchers(
    items: List[_Item], seeds: Set[str], statics: Set[str]
) -> Set[str]:
    """Fixpoint over top-level fns: a fn touches the bus if it names a bus
    static, calls a known toucher, or calls one transitively."""
    touching = set(seeds)
    changed = True
    while changed:
        changed = False
        for it in items:
            if it.name in touching:
                continue
            names_static = any(
                re.search(r"\b" + re.escape(s) + r"\b", it.body) for s in statics
            )
            if names_static or _callers_of(touching, it.body):
                touching.add(it.name)
                changed = True
    return touching


def check_global_bus_test_isolation() -> List[Finding]:
    """Unit tests touching a process-global broadcast bus own their channel
    or serialize on a test-module guard.

    See the module docstring for the full contract and for why rule 56 does
    not cover this half of the #83 class. Fires an ``error`` per unguarded
    bus-touching test.
    """
    out: List[Finding] = []

    for path in _walk((".rs",), roots=_ROOTS):
        rel = _to_rel(path)
        if "/tests/" in rel or rel.rsplit("/", 1)[-1].startswith("test_"):
            continue  # integration binaries are rule 56's half
        text = _read_text(path)
        if not text or "broadcast::Sender" not in text:
            continue

        lines = _strip_comments(text)
        src = "\n".join(lines)

        bus_statics = {m.group("name") for m in _BUS_STATIC_RE.finditer(src)}
        bus_accessors = {m.group("name") for m in _BUS_ACCESSOR_RE.finditer(src)}
        if not bus_statics and not bus_accessors:
            continue

        # Test modules attached to this bus: the inline `mod tests { … }` and
        # any file-backed `mod tests;` sibling. Each yields the lines to parse,
        # the item indent, the reporting path, and its own raw text.
        scopes: List[Tuple[str, List[str], str, int, int, int]] = []
        tmod = _find_test_module(lines)
        if tmod is not None:
            body_lo, body_hi, item_indent = tmod
            scopes.append((rel, lines, text, body_lo, body_hi, item_indent))
        for sib in _file_backed_test_mods(lines, path):
            sib_text = _read_text(sib)
            if not sib_text:
                continue
            sib_lines = _strip_comments(sib_text)
            scopes.append(
                (_to_rel(sib), sib_lines, sib_text, 0, len(sib_lines), 0)
            )
        if not scopes:
            continue  # a bus with no unit tests can't self-collide here

        # Top-level (non-test-module) fns of the BUS file, for the transitive
        # touch analysis — the sibling's tests call into these by `super::`.
        top_end = tmod[0] if tmod is not None else len(lines)
        top_items = _parse_items(lines, 0, top_end, 0)
        seeds = set(bus_accessors) | {"subscribe"}
        base_touchers = _transitive_touchers(top_items, seeds, bus_statics)

        for rel_report, mlines, mtext, body_lo, body_hi, item_indent in scopes:
            out.extend(
                _check_scope(
                    rel_report,
                    mlines,
                    mtext,
                    body_lo,
                    body_hi,
                    item_indent,
                    set(base_touchers),
                    bus_statics,
                )
            )

    return out


def _check_scope(
    rel: str,
    lines: List[str],
    text: str,
    body_lo: int,
    body_hi: int,
    item_indent: int,
    touchers: Set[str],
    bus_statics: Set[str],
) -> List[Finding]:
    """Findings for one test module (inline block or file-backed sibling)."""
    out: List[Finding] = []

    # Test-module items: the tests themselves plus their helpers.
    mod_items = _parse_items(lines, body_lo, body_hi, item_indent)
    helpers = [it for it in mod_items if not it.is_test]

    # A helper isolates if it builds a channel; it serializes if it locks a
    # test-module Mutex. Both propagate to the tests that call them.
    mod_src = "\n".join(lines[body_lo:body_hi])
    mutex_statics = {m.group("name") for m in _MUTEX_STATIC_RE.finditer(mod_src)}

    def _locks(body: str) -> bool:
        return bool(_LOCK_CALL_RE.search(body)) and any(
            re.search(r"\b" + re.escape(s) + r"\b", body) for s in mutex_statics
        )

    # Touch propagates through test-module helpers as well: a test that merely
    # calls `spawn_loop()` reaches the bus if the loop publishes to it. That
    # arm is what makes the *publishing* siblings visible, not just the one
    # test that reads — in #246 the reader was the only test naming the bus,
    # and every collider came in through a helper.
    for _ in range(len(helpers) + 1):
        for h in helpers:
            if h.name in touchers:
                continue
            if any(
                re.search(r"\b" + re.escape(s) + r"\b", h.body) for s in bus_statics
            ) or _callers_of(touchers, h.body):
                touchers.add(h.name)

    isolating = {h.name for h in helpers if _OWN_CHANNEL_RE.search(h.body)}
    serializing = {h.name for h in helpers if _locks(h.body)}
    # A helper that calls an isolating/serializing helper counts too.
    for _ in range(len(helpers)):
        for h in helpers:
            if h.name not in isolating and _callers_of(isolating, h.body):
                isolating.add(h.name)
            if h.name not in serializing and _callers_of(serializing, h.body):
                serializing.add(h.name)

    raw_lines = text.splitlines()
    for t in (it for it in mod_items if it.is_test):
        # Opt-out on the fn head or the attribute lines just above it. Read
        # from the RAW text: the marker lives in a `//` comment, which the
        # stripped copy has thrown away.
        window = raw_lines[max(0, t.start_line - 4) : t.start_line]
        if any(_OPT_OUT in ln for ln in window):
            continue

        names_static = any(
            re.search(r"\b" + re.escape(s) + r"\b", t.body) for s in bus_statics
        )
        if not (names_static or _callers_of(touchers, t.body)):
            continue  # doesn't reach the bus

        if _OWN_CHANNEL_RE.search(t.body) or _callers_of(isolating, t.body):
            continue  # owns its channel
        if _locks(t.body) or _callers_of(serializing, t.body):
            continue  # serialized on a test-module guard

        out.append(
            Finding(
                rule="global_bus_test_isolation",
                severity="error",
                file=rel,
                line=t.start_line,
                message=(
                    f"unit test `{t.name}` reaches a process-global "
                    f"broadcast bus without isolating or serializing. "
                    f"`cargo test` runs this binary's tests on parallel "
                    f"threads in one process, so a sibling test's publish "
                    f"lands in this test's receiver and any "
                    f"first-event/ordering assertion becomes a scheduling "
                    f"coincidence — the #83 self-collision class (#246, "
                    f"which reddened the unrelated PR #244). Prefer "
                    f"injecting the sender so the test owns a "
                    f"`broadcast::channel(…)` of its own; failing that, "
                    f"serialize on a test-module `Mutex` guard (the "
                    f"`TEST_GUARD`/`guard()` shape in "
                    f"`Pipe/src/conversation_bus.rs`, rule 56's DB_LOCK "
                    f"pattern)." + tracker_pointer(_CLASS_TRACKER)
                ),
                context=t.name,
            )
        )

    return out
