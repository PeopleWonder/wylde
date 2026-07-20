"""Tests for quality rules (file_size_limit) — mirrors prod-side
wylde_check/rules/_quality.py.

Rule 20 was repointed from Python to Rust on 2026-07-20; the fixtures
below write synthetic ``.rs`` files into the tmp_path Rust tree the rule
now walks (``rust/crates/*/src/**`` + ``Core/GUI/**``).  Rules 21
(test_init_present), 24 (no_bare_except) and 32 (manifest_sandbox_required)
were retired in the same pass, and their tests removed with them.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 20: file size limit (Rust) ─────────────────────────────────


def test_file_size_limit_flags_oversized_rust(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # 750-line Rust file in a crate src tree is over the 700-LOC cap.
    big = "let x = 1;\n" * 750
    _write(root / "rust" / "crates" / "wylde-foo" / "src" / "big.rs", big)
    findings = wc.check_file_size_limit()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "file_size_limit"
    assert f.severity == "error"
    assert f.file == "rust/crates/wylde-foo/src/big.rs"
    assert "750" in f.message


def test_file_size_limit_flags_oversized_gui_rust(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    big = "let x = 1;\n" * 900
    _write(root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "foo.rs", big)
    findings = wc.check_file_size_limit()
    assert len(findings) == 1
    assert findings[0].file == "Core/GUI/Frontend/Panels/Foo/src/foo.rs"


def test_file_size_limit_allows_under_cap_rust(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "small.rs",
        "let x = 1;\n" * 500,
    )
    assert wc.check_file_size_limit() == []


def test_file_size_limit_ignores_python(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # The rule no longer walks Python — an oversized .py file is invisible.
    _write(root / "Core" / "harness" / "big.py", "x = 1\n" * 5000)
    assert wc.check_file_size_limit() == []


def test_file_size_limit_skips_target_build_output(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Cargo build output under target/ is excluded even when oversized.
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "target" / "gen.rs",
        "let x = 1;\n" * 5000,
    )
    assert wc.check_file_size_limit() == []
