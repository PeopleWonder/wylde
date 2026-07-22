"""Rule 56: multi-test ``bolt://`` binaries must serialize on a ``DB_LOCK``
and must actually run in the live-graph CI leg.

The failure this exists to prevent
----------------------------------

The live-graph tests all target ONE shared Neo4j, and ``cargo test`` runs a
binary's tests multi-threaded by default. Two ignored live tests in the same
binary therefore contend on graph-*global* state — ``ensure_schema``,
``stats()`` counts, and the graph-wide orphan-entity prune inside
``delete_workspace`` — even when each test namespaces its own workspace id.
The symptom is a non-deterministic ``ok:false`` operation failure, a
different test failing on each run.

The fix, established across ``memgraph_live`` / ``memgraph_bolt_integration``
/ ``memgraph_parity_integration`` / ``integration_graph`` (#216, #227), is a
process-wide ``DB_LOCK`` (a ``tokio::sync::Mutex``) acquired at the top of
every test body, so the serialization is a property of the *test* rather than
of how it happens to be invoked (``--test-threads=1`` in CI is only a
belt-and-suspenders second guard).

That class has now recurred three times, each time by the same omission: a
new multi-test live-graph binary was added, or an existing one grew a second
test, without the lock. Nothing turned red — the lock is a convention a
reviewer has to remember, and CI, having no live stack on the ``backend``
leg, runs these ``#[ignore]``d tests only in the dedicated ``--ignored``
job. This rule makes the convention structural.

What it enforces
----------------

For every Rust integration-test binary (``rust/crates/**/tests/*.rs``) that
contains **two or more** *live-graph* tests — a ``#[test]`` / ``#[tokio::test]``
that is also ``#[ignore]``d with a reason naming the graph DB
(``bolt://`` / ``Neo4j`` / ``Memgraph``):

* **Serialization (in-code).** Every one of those tests must acquire the
  binary's ``DB_LOCK`` in its body — either directly
  (``let _g = DB_LOCK.lock().await;``) or through a same-file guard helper
  that does (the ``db_guard()`` form ``integration_graph`` uses). A test that
  opens the shared DB without the lock is flagged.

* **CI coverage (structural), bolt-only binaries.** A *bolt-only* live-graph
  binary — one that reaches the graph purely over Bolt — must be run in the
  live-graph leg of ``.github/workflows/ci.yml`` (a ``--test <stem> …
  --ignored`` invocation). A bolt-only live-graph binary CI never runs is a
  dead gate: its serialization is unverified and a regression in it can't be
  caught.

  A **pipe-vs-bolt parity** binary is exempt from this second arm.
  ``memgraph_parity_integration`` drives the ``wylde-memgraph`` service over
  its named pipe *and* over Bolt, asserting ``pipe.ok && bolt.ok`` on every
  test. The live-graph leg stands up only the vendored Neo4j (Bolt), not that
  pipe service, so the binary cannot pass there — it needs the full local
  stack, which is exactly why it is ``#[ignore]``d and why the #83 audit found
  it outside the leg. Forcing it in would only ever be red. Its DB_LOCK is
  still enforced by the first arm, so its dev ``--ignored`` runs stay
  serialized (#227). The tell for "needs the pipe service" is
  :data:`_PIPE_SERVICE_TELL_RE` (``pipe_client`` / ``WYLDE_MEMGRAPH_SERVICE``).

A single-test live-graph binary can't self-collide, so it is intentionally
not in scope (``integration_symbol_context`` / ``integration_symbols_find`` /
the ingest/watcher live tests). ``memgraph_integration`` has exactly one
``#[ignore]``d live test (its second test is a non-ignored negative case that
targets a known-dead service and never touches the shared DB), so it is out
of scope too.

Like the rest of the suite the rule walks the active tree read-only and emits
``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
import sys as _sys
from typing import List, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# Resolve the TOP package object so ``monkeypatch.setattr(wc, "WYLDE_ROOT",
# tmp_path)`` in the unit suite flows through to the CI-workflow read below
# (mirrors ``_selfcheck._pkg``).
_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Constants ────────────────────────────────────────────────────────

# The live-graph CI leg lives here; the rule reads it to confirm each
# in-scope binary is actually run. Registered in
# ``_selfcheck.RULE_TARGET_SPECS`` so a rename turns THIS gate red instead of
# quietly disarming the CI-coverage half.
_CI_WORKFLOW_REL = ".github/workflows/ci.yml"

# A test is "live-graph" when it is #[ignore]d with a reason naming the graph
# database. Matched against the joined attribute block preceding the fn.
_DB_TELL_RE = re.compile(r"bolt://|neo4j|memgraph", re.IGNORECASE)

# A binary "requires the memgraph PIPE service" (not just Bolt) when it drives
# the `wylde-memgraph` service over its named pipe — the pipe-vs-bolt PARITY
# shape (`memgraph_parity_integration`: `pipe_client()`, asserts `pipe.ok &&
# bolt.ok`). The live-graph CI leg stands up only the vendored Neo4j (Bolt),
# not that pipe service, so such a binary CANNOT run there — it needs the full
# local stack, which is why it is `#[ignore]`d. The CI-coverage arm therefore
# applies to BOLT-ONLY binaries only; a pipe-parity binary is exempt from it
# (the DB_LOCK arm still applies, so its dev `--ignored` runs stay serialized).
_PIPE_SERVICE_TELL_RE = re.compile(r"\bpipe_client\b|WYLDE_MEMGRAPH_SERVICE")

# Test attribute (`#[test]`, `#[tokio::test]`, `#[tokio::test(flavor = …)]`).
_TEST_ATTR_RE = re.compile(r"#\[\s*(?:tokio::)?test\b")

# `#[ignore]` / `#[ignore = "…"]`.
_IGNORE_ATTR_RE = re.compile(r"#\[\s*ignore\b")

# A top-level item head: `fn f`, `async fn f`, `pub async fn f`, etc. rustfmt
# (gated by G6) puts every top-level fn — and its closing brace — at column 0,
# so item extents are found by "fn head at col 0 … up to the next `}` at col
# 0" without needing a brace/string lexer.
_FN_HEAD_RE = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?fn\s+(?P<name>\w+)"
)

# Direct lock acquisition. A guard helper is detected structurally (any
# non-test item whose body performs a `DB_LOCK` `.lock()`), so both the
# `DB_LOCK.lock().await` and `db_guard().await` forms are covered.
_DIRECT_LOCK_RE = re.compile(r"\bDB_LOCK\b")
_LOCK_CALL_RE = re.compile(r"\.lock\s*\(")

# In-scope binaries live under a `tests/` dir in the Rust crates tree.
_RUST_TEST_PATH_RE = re.compile(r"^rust/crates/[^/]+/tests/[^/]+\.rs$")


def _strip_comments(src: str) -> List[str]:
    """Return the source lines with ``//`` line comments and ``/* … */``
    block comments removed, so a commented-out lock acquisition never counts
    as a guard and a doc line naming a DB never arms a region. String-literal
    contents are kept (the tokens we search for never legitimately appear
    inside one) — and item extents come from column-0 ``}`` lines, not brace
    counting, so string braces are irrelevant.
    """
    out: List[str] = []
    in_block = False
    for raw in src.splitlines():
        buf = []
        i = 0
        n = len(raw)
        while i < n:
            if in_block:
                end = raw.find("*/", i)
                if end == -1:
                    i = n
                    break
                i = end + 2
                in_block = False
                continue
            if raw.startswith("//", i):
                break
            if raw.startswith("/*", i):
                in_block = True
                i += 2
                continue
            buf.append(raw[i])
            i += 1
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

    @property
    def is_live_graph_test(self) -> bool:
        if not self.is_test:
            return False
        joined = " ".join(self.attrs)
        return bool(_IGNORE_ATTR_RE.search(joined) and _DB_TELL_RE.search(joined))

    @property
    def is_lock_helper(self) -> bool:
        # A non-test item that itself performs a DB_LOCK .lock() — the
        # `db_guard()` shape.
        return (
            not self.is_test
            and _DIRECT_LOCK_RE.search(self.body) is not None
            and _LOCK_CALL_RE.search(self.body) is not None
        )


def _parse_items(lines: List[str]) -> List[_Item]:
    """Split comment-stripped source into top-level items, each carrying its
    preceding attribute block and its body text."""
    items: List[_Item] = []
    attrs: List[str] = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        stripped = line.strip()
        if not stripped:
            i += 1
            continue
        # Column-0 outer attribute (`#[...]`, not the inner `#![...]`).
        if line.startswith("#[") or (line.startswith("#") and stripped.startswith("#[")):
            attrs.append(stripped)
            i += 1
            continue
        m = _FN_HEAD_RE.match(line)
        if m and not line[0].isspace():
            name = m.group("name")
            start_line = i + 1  # 1-based
            body_lines = [line]
            i += 1
            while i < n and not lines[i].startswith("}"):
                body_lines.append(lines[i])
                i += 1
            if i < n:  # the closing `}` at column 0
                body_lines.append(lines[i])
                i += 1
            items.append(_Item(name, attrs, "\n".join(body_lines), start_line))
            attrs = []
            continue
        # Any other column-0 code line (static / const / use / struct …)
        # ends a pending attribute run that wasn't a fn's.
        if not line[0].isspace():
            attrs = []
        i += 1
    return items


def _body_is_guarded(body: str, helper_names: Tuple[str, ...]) -> bool:
    if _DIRECT_LOCK_RE.search(body) and _LOCK_CALL_RE.search(body):
        return True
    for h in helper_names:
        if re.search(r"\b" + re.escape(h) + r"\s*\(", body):
            return True
    return False


def _ci_runs_binary(ci_text: str, stem: str) -> bool:
    """True iff the live-graph leg runs ``stem`` under ``--ignored``."""
    needle = re.compile(r"--test\s+" + re.escape(stem) + r"\b")
    for line in ci_text.splitlines():
        if needle.search(line) and "--ignored" in line:
            return True
    return False


def check_graph_test_serialized_on_db_lock() -> List[Finding]:
    """Every multi-test ``bolt://`` binary serializes each test on a
    ``DB_LOCK`` and is run in the live-graph CI leg.

    See the module docstring for the full contract. Fires an ``error`` per
    unguarded live-graph test, and one per in-scope binary missing from the
    CI leg.
    """
    out: List[Finding] = []

    ci_path = _pkg.WYLDE_ROOT / _CI_WORKFLOW_REL
    ci_text = _read_text(ci_path) if ci_path.exists() else ""

    for path in _walk((".rs",), roots=("rust/crates",)):
        rel = _to_rel(path)
        if not _RUST_TEST_PATH_RE.match(rel):
            continue
        text = _read_text(path)
        if not text:
            continue

        lines = _strip_comments(text)
        items = _parse_items(lines)
        live_tests = [it for it in items if it.is_live_graph_test]
        # Single-test binaries can't self-collide — out of scope.
        if len(live_tests) < 2:
            continue

        helper_names = tuple(it.name for it in items if it.is_lock_helper)

        for t in live_tests:
            if not _body_is_guarded(t.body, helper_names):
                out.append(
                    Finding(
                        rule="graph_test_serialized_on_db_lock",
                        severity="error",
                        file=rel,
                        line=t.start_line,
                        message=(
                            f"live-graph test `{t.name}` in a multi-`bolt://` "
                            f"binary ({len(live_tests)} live tests share one "
                            f"Neo4j) does not serialize on a DB_LOCK. Without a "
                            f"per-test `let _g = DB_LOCK.lock().await;` (or a "
                            f"`db_guard()` helper) the tests contend on "
                            f"graph-global state (ensure_schema / stats() / the "
                            f"delete_workspace orphan-prune) when run "
                            f"multi-threaded — the #83 self-collision class "
                            f"(#216/#227). Acquire the lock at the top of the "
                            f"test body."
                        ),
                        context=t.name,
                    )
                )

        # CI-coverage arm — bolt-only binaries only. A pipe-vs-bolt parity
        # binary needs the wylde-memgraph pipe service the bolt-only leg can't
        # boot, so requiring it there would only ever be red; it's exempt.
        if _PIPE_SERVICE_TELL_RE.search(text):
            continue

        stem = rel.rsplit("/", 1)[-1][: -len(".rs")]
        if not _ci_runs_binary(ci_text, stem):
            out.append(
                Finding(
                    rule="graph_test_serialized_on_db_lock",
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        f"multi-test `bolt://` binary `{stem}` is not run in "
                        f"the live-graph leg of {_CI_WORKFLOW_REL}: no "
                        f"`--test {stem} … --ignored` invocation found. A "
                        f"live-graph binary CI never runs is a dead gate — its "
                        f"serialization is unverified and a regression can't be "
                        f"caught (the #83 audit found `memgraph_parity_integration` "
                        f"in exactly this state). Add it to the live-graph job's "
                        f"`--no-run` build and `--ignored` run steps."
                    ),
                    context=stem,
                )
            )

    return out
