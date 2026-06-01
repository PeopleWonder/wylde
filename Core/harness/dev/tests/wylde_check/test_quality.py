"""Tests for quality rules (file_size_limit, test_init_present,
no_bare_except) — mirrors prod-side wylde_check/rules/_quality.py.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 20: file size limit ────────────────────────────────────────


def test_file_size_limit_flags_oversized(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # 750-line file is over the 700-LOC cap.
    big = "x = 1\n" * 750
    _write(root / "Core" / "harness" / "big.py", big)
    findings = wc.check_file_size_limit()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "file_size_limit"
    assert f.severity == "error"
    assert f.file == "Core/harness/big.py"
    assert "750" in f.message


def test_file_size_limit_allows_under_cap(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "small.py", "x = 1\n" * 500)
    assert wc.check_file_size_limit() == []


def test_file_size_limit_skips_legacy(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "_legacy" / "huge.py", "x = 1\n" * 5000)
    # _legacy/ is excluded from active-tree walks.
    assert wc.check_file_size_limit() == []


# ── Rule 21: tests/ folders carry __init__.py ──────────────────────


def test_test_init_present_flags_missing(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "shared" / "tests" / "test_foo.py", "def test_a(): pass\n")
    findings = wc.check_test_init_present()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "test_init_present"
    assert f.severity == "error"
    assert f.file == "Core/shared/tests"


def test_test_init_present_clean_when_init_exists(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "shared" / "tests" / "__init__.py", "")
    _write(root / "Core" / "shared" / "tests" / "test_foo.py", "def test_a(): pass\n")
    assert wc.check_test_init_present() == []


def test_test_init_present_skips_legacy_tests_dirs(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "_legacy" / "core" / "tests" / "test_old.py", "")
    assert wc.check_test_init_present() == []


# ── Rule 24: no bare / silent-swallow except ───────────────────────


def test_no_bare_except_flags_bare(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "evil.py",
        "def f():\n    try:\n        x = 1\n    except:\n        pass\n",
    )
    findings = wc.check_no_bare_except()
    assert len(findings) == 1
    assert findings[0].rule == "no_bare_except"
    assert "Bare" in findings[0].message


def test_no_bare_except_flags_multi_statement_swallow(isolated_tree: Any) -> None:
    """Single-statement except bodies (the canonical intentional
    best-effort shape) are NOT flagged; only multi-statement silent
    swallows are."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "evil.py",
        "def f():\n"
        "    try:\n"
        "        x = 1\n"
        "    except Exception:\n"
        "        y = 1\n"
        "        z = 2\n"
        "        return None\n",
    )
    findings = wc.check_no_bare_except()
    assert len(findings) == 1
    assert (
        "swallow" in findings[0].message.lower()
        or "neither" in findings[0].message.lower()
    )


def test_no_bare_except_allows_single_statement_pass(isolated_tree: Any) -> None:
    """Single-statement ``pass`` (canonical cleanup) is intentional."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "ok.py",
        "def f():\n    try:\n        x.close()\n    except Exception:\n        pass\n",
    )
    assert wc.check_no_bare_except() == []


def test_no_bare_except_allows_captured_exception_used(isolated_tree: Any) -> None:
    """Even multi-statement bodies are intentional when the bound
    exception is referenced (deliberate context recording)."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "ok.py",
        "def f():\n"
        "    try:\n"
        "        import sounddevice as sd\n"
        "    except Exception as exc:\n"
        "        sd = None\n"
        "        _IMPORT_ERROR = exc\n",
    )
    assert wc.check_no_bare_except() == []


def test_no_bare_except_allows_log(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "ok.py",
        "def f():\n"
        "    try:\n"
        "        x = 1\n"
        "    except Exception:\n"
        "        logger.exception('oops')\n",
    )
    assert wc.check_no_bare_except() == []


def test_no_bare_except_honors_noqa_marker(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "ok.py",
        "def f():\n"
        "    try:\n"
        "        x = 1\n"
        "    except Exception:  # noqa: BLE001\n"
        "        pass\n",
    )
    assert wc.check_no_bare_except() == []


def test_no_bare_except_skips_specific_exception(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "ok.py",
        "def f():\n"
        "    try:\n"
        "        x = 1\n"
        "    except FileNotFoundError:\n"
        "        pass\n",
    )
    assert wc.check_no_bare_except() == []
