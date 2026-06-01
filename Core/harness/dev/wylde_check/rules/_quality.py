"""Code-quality rules: file-size cap, tests/ ``__init__.py`` presence,
bare / silently-swallowed ``except`` blocks, manifest-dir sandboxing in
Lifecycle / harness tests.

Split from ``_runtime.py`` to keep individual rule submodules under the
``file_size_limit`` cap (rule 20) — these rules are about code hygiene
rather than runtime/lifecycle convention."""

from __future__ import annotations

import ast
import re
import sys as _sys
from typing import List, Tuple

from .. import Finding
from .._config import ACTIVE_ROOTS
from .._walkers import _is_excluded, _is_test_path, _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 20: flat 700-LOC limit on Python files ─────────────────────


_FILE_SIZE_LIMIT_LOC = 700


# Files that exceeded the cap at the time rule 20 was introduced and
# whose splits are queued as separate refactor tasks.  Each entry is a
# real piece of architectural debt — the rule still fires on every NEW
# file going forward; only these documented predecessors are skipped.
# the Wylde user curates this list — remove an entry once its split lands.
_FILE_SIZE_QUEUED_SPLITS: Tuple[str, ...] = (
    # _runtime.py hosts 11 runtime/lifecycle rule functions in one file
    # (the file_size_limit / test_init_present / no_bare_except split
    # to _quality.py was the first carve).  At 702 lines it is two
    # lines past the cap; a second carve along its natural seams
    # (manifest-shape rules vs. spawn/path rules vs. startup-sequence
    # rules) is the queued follow-up.
    "Core/harness/dev/wylde_check/rules/_runtime.py",
    # __init__.py sat at exactly the 700 cap before rule 51 landed, so the
    # one-rule import + dispatcher entry + __all__ entry pushed it ~10 lines
    # over (2026-05-31 Dashboard cold-start-crash slice).  The natural carve
    # is to lift the dispatcher (`_RULES` / `run_all` / `check_one_file`)
    # into a sibling `_dispatch.py`, or to drop the ~295-line module-docstring
    # rule catalog that already duplicates docs/wylde_check_rules.md.  Queued.
    "Core/harness/dev/wylde_check/__init__.py",
)


def check_file_size_limit() -> List[Finding]:
    """Every active Python file must be at most 700 lines long.

    the Wylde user's call: one flat cap, no production/test split.  When a file
    blows the limit, the right move is almost always to split it along
    its natural seams (the wylde_check package itself is the worked
    example).  Counts raw lines including blank lines and comments —
    file *length* is what hurts editing and review, not LOC density.

    The :data:`_FILE_SIZE_QUEUED_SPLITS` allowlist documents files that
    were already oversized when the rule landed.  Each entry is tracked
    as a queued split task; the Wylde user removes entries as splits ship so the
    cap re-engages on the now-clean tree.
    """
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        if rel in _FILE_SIZE_QUEUED_SPLITS:
            continue
        text = _read_text(path)
        if not text:
            continue
        lines = text.count("\n") + (0 if text.endswith("\n") else 1)
        if lines > _FILE_SIZE_LIMIT_LOC:
            out.append(
                Finding(
                    rule="file_size_limit",
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        f"File is {lines} lines long; the flat cap is "
                        f"{_FILE_SIZE_LIMIT_LOC}.  Split along its natural "
                        f"seams (one rule per submodule, one route group "
                        f"per file, etc.)."
                    ),
                )
            )
    return out


# ── Rule 21: tests/ folders carry an __init__.py ────────────────────


def check_test_init_present() -> List[Finding]:
    """Every Python ``tests/`` folder under an active root must contain
    an ``__init__.py``.  Without it, pytest's rootdir discovery resolves
    sibling ``tests/`` folders by name only and the wrong ``conftest``
    or test module gets loaded — the same misroute that caused the
    bogus "passlib regression" miss-report.

    Skipped for non-Python ``tests/`` dirs (e.g. Rust integration tests
    under ``rust/crates/*/tests/`` — Cargo uses its own conventions and
    has no use for ``__init__.py``).
    """
    out: List[Finding] = []
    pkg_root = _pkg.WYLDE_ROOT
    seen_dirs: set = set()
    for root in ACTIVE_ROOTS:
        base = pkg_root / root
        if not base.exists():
            continue
        for tests_dir in base.rglob("tests"):
            if not tests_dir.is_dir():
                continue
            if _is_excluded(tests_dir):
                continue
            key = tests_dir.resolve()
            if key in seen_dirs:
                continue
            seen_dirs.add(key)
            has_python = any(
                child.is_file() and child.suffix == ".py"
                for child in tests_dir.iterdir()
            )
            if not has_python:
                continue
            if not (tests_dir / "__init__.py").exists():
                out.append(
                    Finding(
                        rule="test_init_present",
                        severity="error",
                        file=_to_rel(tests_dir),
                        line=0,
                        message=(
                            "tests/ folder has no __init__.py.  Without it, "
                            "pytest rootdir discovery can merge sibling "
                            "tests/ trees and load the wrong conftest — add "
                            "an empty __init__.py to mark the package."
                        ),
                    )
                )
    return out


# ── Rule 24: no bare / silently-swallowed except ────────────────────


def _except_body_swallows(node: ast.ExceptHandler) -> bool:
    """True when the handler body neither logs nor re-raises.

    Allow:
      * ``raise`` (with or without arg) anywhere in the body
      * ``logger.exception`` / ``.error`` / ``.warning`` / ``.warn`` /
        ``.critical`` / ``.info`` / ``.debug`` calls on a logger-shaped
        receiver (``logger``, ``log``, ``logging``, ``_logger``, ``LOG``,
        ``LOGGER``, or ``<obj>.logger`` / ``<obj>.log``)
      * the handler binds the exception with ``as <name>`` AND ``<name>``
        is referenced in the body (saved to an attribute, printed,
        passed into a constructor, etc.) — that's deliberate recording
        of context, not a silent swallow
      * a body that's only ``pass`` is the canonical swallow — flag it
    """
    has_raise = False
    has_log = False
    captured_name = node.name  # the ``as <name>`` binding, or None
    captured_used = False
    for child in ast.walk(node):
        if isinstance(child, ast.Raise):
            has_raise = True
        elif isinstance(child, ast.Call):
            func = child.func
            if isinstance(func, ast.Attribute):
                attr = func.attr
                if attr in (
                    "exception",
                    "error",
                    "warning",
                    "warn",
                    "critical",
                    "info",
                    "debug",
                ):
                    parent = func.value
                    if isinstance(parent, ast.Name) and parent.id in (
                        "logger",
                        "log",
                        "logging",
                        "_logger",
                        "LOG",
                        "LOGGER",
                    ):
                        has_log = True
                    elif isinstance(parent, ast.Attribute) and parent.attr in (
                        "logger",
                        "log",
                    ):
                        has_log = True
        elif (
            captured_name is not None
            and isinstance(child, ast.Name)
            and child.id == captured_name
            and not isinstance(child.ctx, ast.Store)
        ):
            captured_used = True
    return not (has_raise or has_log or captured_used)


def _is_short_intentional_swallow(handler: ast.ExceptHandler) -> bool:
    """True when the except handler body is a single statement.

    Single-statement recovery is the canonical intentional best-effort
    shape — ``pass``, ``return <fallback>``, ``x = {}``, nested
    fallback ``try:``, fallback import.  The rule only flags
    *multi-statement* silent-swallow handlers, where complex recovery
    logic without any logging is the actual concern.

    The team uses ``# noqa: BLE001`` for the rare multi-statement
    intentional case (e.g. nested cleanup with multiple resource
    closes); we honor that marker.
    """
    return len(handler.body) == 1


_BLE001_NOQA_RE = re.compile(r"#\s*noqa[^\n]*\bBLE001\b")
_BARE_EXCEPT_OK_RE = re.compile(r"#\s*wylde-check:\s*bare-except-ok")


def _except_line_has_suppression(text_lines: List[str], lineno: int) -> bool:
    """Honor inline ``# noqa: BLE001`` (ruff's existing convention) and
    ``# wylde-check: bare-except-ok`` on the ``except`` line itself."""
    if lineno <= 0 or lineno > len(text_lines):
        return False
    line = text_lines[lineno - 1]
    return bool(_BLE001_NOQA_RE.search(line) or _BARE_EXCEPT_OK_RE.search(line))


def check_no_bare_except() -> List[Finding]:
    """Flag bare ``except:`` and silent-swallow ``except Exception:``
    blocks in active code.

    A handler "swallows" when its body neither re-raises nor logs the
    exception.  Tests are exempt — they often catch and inspect.  The
    checker itself is exempt because its dispatcher's catch-all is the
    documented mechanism that converts a broken rule into a finding.

    Two inline suppressions are honoured:
      * ``# noqa: BLE001`` — ruff's existing bare-except marker.  If the
        team already audited the callsite for ruff, we don't second-
        guess it.
      * ``# wylde-check: bare-except-ok`` — the rule-specific marker
        matching the pattern other rules use.
    """
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        if _is_test_path(rel):
            continue
        if "/dev/wylde_check/" in rel:
            continue
        text = _read_text(path)
        if not text:
            continue
        try:
            tree = ast.parse(text)
        except SyntaxError:
            continue
        text_lines = text.splitlines()
        for node in ast.walk(tree):
            if not isinstance(node, ast.ExceptHandler):
                continue
            if _except_line_has_suppression(text_lines, node.lineno):
                continue
            # Bare except is always a finding.
            if node.type is None:
                out.append(
                    Finding(
                        rule="no_bare_except",
                        severity="error",
                        file=rel,
                        line=node.lineno,
                        message=(
                            "Bare ``except:`` catches BaseException incl. "
                            "KeyboardInterrupt / SystemExit.  Catch a "
                            "specific class, or use ``except Exception:`` "
                            "AND log / re-raise."
                        ),
                    )
                )
                continue
            # Only flag the broad ``except Exception`` / ``except BaseException``
            # forms that silently swallow.  Specific exception classes
            # (FileNotFoundError, ValueError, …) are deliberate handling.
            exc_type = node.type
            type_names: List[str] = []
            if isinstance(exc_type, ast.Name):
                type_names.append(exc_type.id)
            elif isinstance(exc_type, ast.Tuple):
                for elt in exc_type.elts:
                    if isinstance(elt, ast.Name):
                        type_names.append(elt.id)
            if not any(n in ("Exception", "BaseException") for n in type_names):
                continue
            if not _except_body_swallows(node):
                continue
            # Single-statement recovery is the canonical intentional
            # best-effort shape — skip those.  Multi-statement silent
            # swallows still trip (those are where complex recovery
            # hides real failures).
            if _is_short_intentional_swallow(node):
                continue
            out.append(
                Finding(
                    rule="no_bare_except",
                    severity="error",
                    file=rel,
                    line=node.lineno,
                    message=(
                        "Multi-statement ``except Exception:`` block "
                        "neither re-raises nor logs.  Silent swallow "
                        "hides real failures; log with "
                        "``logger.exception(...)``, re-raise after "
                        "recording context, or simplify the try body to a "
                        "single best-effort call."
                    ),
                )
            )
    return out


# ── Rule 32: manifest dir is sandboxed in Lifecycle / harness tests ──
#
# Tests under ``Core/Lifecycle/tests/`` and ``Core/harness/tests/`` that
# touch the manifest layer must NOT read the real ``data/manifests/``
# directory. The reaper at ``Core/Lifecycle/daemon_state/_orphan_sweep.py``
# kills any pid it finds alive in a manifest there; a test that runs the
# reaper unsandboxed will kill real wylde services. The 2026-05-25
# Phase 11.B verification gate caught exactly this — a synthetic-pid
# test happened to collide with the real wylde-gateway pid and the
# reaper terminated it.
#
# Two test shapes trip the rule:
#
#   1. A test names ``_MANIFEST_DIR`` but never patches it. Even reading
#      the constant in test code (to e.g. list files for assertion) is
#      a smell — production-path reads belong behind a fixture.
#
#   2. A test path-literally references ``data/manifests/`` (any form:
#      string, joined Path, glob) without first calling
#      ``monkeypatch.setattr`` against an ``_MANIFEST_DIR`` symbol.
#
# Exemptions:
#   * ``Core/Lifecycle/tests/conftest.py`` — the autouse-fixture
#     definition itself names ``_MANIFEST_DIR`` to patch it.
#   * Other ``conftest.py`` files in the watched tree are exempt for
#     the same reason; the fixture they define IS the sandbox.

# Test trees under which manifest sandboxing matters. The reaper kill
# path lives in ``Core/Lifecycle`` and its callers in ``Core/harness``
# (via memory + daemon pipe actions); these are the trees where an
# accidental real-manifest read can wreck the user's running stack.
_MANIFEST_TEST_TREES: Tuple[str, ...] = (
    "Core/Lifecycle/tests/",
    "Core/harness/tests/",
)


def _names_manifest_dir(text: str) -> bool:
    """True when ``text`` references the ``_MANIFEST_DIR`` symbol."""
    return "_MANIFEST_DIR" in text


_DATA_MANIFESTS_PATH_RE = re.compile(
    r"""(?xi)
    ['"]                # quoted path literal …
    [^'"\n]*?           # … containing …
    data
    [\\/]               # cross-platform separator
    manifests
    [\\/'"]             # trailing slash, end-quote, or close-paren
    """
)


def _references_real_manifest_dir(text: str) -> bool:
    """True when ``text`` contains a path literal pointing at
    ``data/manifests/`` (production path) — ignoring matches that sit
    inside obvious comments and docstrings.

    Best-effort string-level scan; we don't try to AST-trace where the
    literal flows, just whether one appears at all. False positives are
    cheap to suppress via the ``# wylde-check: manifest-sandbox-ok``
    marker on the offending line.
    """
    return bool(_DATA_MANIFESTS_PATH_RE.search(text))


_MANIFEST_SANDBOX_OK_RE = re.compile(r"#\s*wylde-check:\s*manifest-sandbox-ok")


def _line_has_sandbox_ok(text_lines: List[str], lineno: int) -> bool:
    if lineno <= 0 or lineno > len(text_lines):
        return False
    return bool(_MANIFEST_SANDBOX_OK_RE.search(text_lines[lineno - 1]))


def _patches_manifest_dir_via_monkeypatch(tree: ast.AST) -> bool:
    """True when the module ASTs contains at least one call shaped like
    ``monkeypatch.setattr(<obj>, "_MANIFEST_DIR", ...)``.

    We don't try to validate the target object — that would force a
    full import-resolution pass and the practical risk is a test that
    *never* patches the constant at all, which a syntactic scan catches
    just as well.
    """
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        if not isinstance(func, ast.Attribute):
            continue
        if func.attr != "setattr":
            continue
        # Targeted: monkeypatch.setattr(<obj>, "_MANIFEST_DIR", <new>)
        # Accept both positional and keyword-string forms.
        for arg in list(node.args) + [kw.value for kw in node.keywords]:
            if isinstance(arg, ast.Constant) and arg.value == "_MANIFEST_DIR":
                return True
    return False


def check_manifest_sandbox_required() -> List[Finding]:
    """Tests that touch the manifest layer must sandbox ``_MANIFEST_DIR``.

    Reading the real ``data/manifests/`` from a test is a foot-gun: the
    Lifecycle reaper kills any live pid it finds in a manifest there, so
    a test that runs the reaper unsandboxed terminates real wylde
    services. The 2026-05-25 Phase 11.B gate caught one — the synthetic
    pid in ``test_reap_kills_live_alive_pid`` collided with the real
    wylde-gateway pid and the reaper killed it.

    The rule flags any test file under :data:`_MANIFEST_TEST_TREES`
    that names ``_MANIFEST_DIR`` or references a ``data/manifests/``
    path literal without also calling
    ``monkeypatch.setattr(..., "_MANIFEST_DIR", ...)`` somewhere in the
    same module. The package-level ``conftest.py`` defining the autouse
    sandbox is exempt — its job is to NAME the constant in order to
    patch it.

    Suppression: per-line ``# wylde-check: manifest-sandbox-ok`` on the
    offending name reference (rare — almost never the right call).
    """
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        if not any(rel.startswith(tree) for tree in _MANIFEST_TEST_TREES):
            continue
        if _is_excluded(path):
            continue
        # conftest.py files in these trees are where the sandbox is
        # *defined* — naming _MANIFEST_DIR there is the whole point.
        if rel.rsplit("/", 1)[-1] == "conftest.py":
            continue
        text = _read_text(path)
        if not text:
            continue
        # Cheap pre-filter: skip files that don't mention either
        # trigger. Avoids parsing every test file in the tree.
        if not (_names_manifest_dir(text) or _references_real_manifest_dir(text)):
            continue
        try:
            tree = ast.parse(text, filename=str(path))
        except SyntaxError:
            continue
        if _patches_manifest_dir_via_monkeypatch(tree):
            continue
        # No monkeypatch — but check the autouse-conftest sandbox: if a
        # sibling conftest.py in this test's directory patches
        # _MANIFEST_DIR autousely, the test inherits it and is safe.
        test_dir = path.parent
        sibling_conftest = test_dir / "conftest.py"
        if sibling_conftest.exists():
            conftest_text = _read_text(sibling_conftest)
            try:
                conftest_tree = ast.parse(conftest_text, filename=str(sibling_conftest))
            except SyntaxError:
                conftest_tree = None
            if (
                conftest_tree is not None
                and _patches_manifest_dir_via_monkeypatch(conftest_tree)
                # autouse=True must also appear — otherwise the
                # conftest defines a fixture tests can opt into but
                # doesn't enforce it. We accept "autouse=True" by
                # substring to keep the scan AST-light.
                and "autouse=True" in conftest_text
            ):
                continue
        text_lines = text.splitlines()
        # Find the first triggering line for the finding's lineno —
        # makes the report point at the actual offending site rather
        # than line 1.
        finding_lineno = 0
        for idx, line in enumerate(text_lines, start=1):
            if "_MANIFEST_DIR" in line or _DATA_MANIFESTS_PATH_RE.search(line):
                if _line_has_sandbox_ok(text_lines, idx):
                    continue
                finding_lineno = idx
                break
        if finding_lineno == 0:
            # Every triggering line is suppressed — accept that.
            continue
        out.append(
            Finding(
                rule="manifest_sandbox_required",
                severity="error",
                file=rel,
                line=finding_lineno,
                message=(
                    "Test references _MANIFEST_DIR or a data/manifests/ "
                    "path literal but never patches the constant. The "
                    "reaper at Core/Lifecycle/daemon_state/_orphan_sweep.py "
                    "will kill any live pid it finds in a manifest under "
                    "the real dir — an unsandboxed test can terminate "
                    "real wylde services. Add an autouse fixture (see "
                    "Core/Lifecycle/tests/conftest.py::sandboxed_manifest_dir) "
                    "or monkeypatch.setattr(<mod>, \"_MANIFEST_DIR\", tmp_path) "
                    "inside the test fixture."
                ),
            )
        )
    return out
