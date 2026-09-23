"""Tests for rule 56 (``graph_test_serialized_on_db_lock``) — mirrors
prod-side ``wylde_check/rules/_graph_test_isolation.py``.

A binary with two or more ``#[ignore]``d live-graph (``bolt://`` / Neo4j /
Memgraph) tests must serialize every test on a ``DB_LOCK`` and must be run in
the live-graph leg of ``.github/workflows/ci.yml`` — the enforcement half of
the #83 self-collision class (#216/#227).
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

_CRATE_TESTS = ("rust", "crates", "wylde-harness", "tests")


def _bin(root: Any, stem: str, body: str) -> None:
    path = root
    for part in _CRATE_TESTS:
        path = path / part
    _write(path / f"{stem}.rs", body)


def _ci(root: Any, *run_stems: str) -> None:
    """Write a synthetic ci.yml whose live-graph leg runs each named stem
    under ``--ignored`` (mirroring the real leg's per-binary run lines)."""
    lines = [
        "jobs:",
        "  live-graph:",
        "    steps:",
        "      - run: |",
    ]
    for stem in run_stems:
        lines.append(
            f"          cargo test -p wylde-harness --locked --test {stem} "
            f"-- --ignored --nocapture --test-threads=1"
        )
    _write(root / ".github" / "workflows" / "ci.yml", "\n".join(lines) + "\n")


# Two live-graph test bodies. `guarded` toggles the per-test DB_LOCK.
def _two_test_binary(guarded_a: bool, guarded_b: bool, *, decl_lock: bool = True) -> str:
    lock_decl = (
        "static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());\n\n"
        if decl_lock
        else ""
    )
    a = "    let _g = DB_LOCK.lock().await;\n" if guarded_a else ""
    b = "    let _g = DB_LOCK.lock().await;\n" if guarded_b else ""
    return (
        "//! Live graph binary.\n"
        f"{lock_decl}"
        "#[tokio::test]\n"
        '#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]\n'
        "async fn round_trip_a() {\n"
        f"{a}"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
        "\n"
        "#[tokio::test]\n"
        '#[ignore = "requires Neo4j alive on bolt://127.0.0.1:7687"]\n'
        "async fn round_trip_b() {\n"
        f"{b}"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
    )


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_both_guarded_and_in_ci(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _bin(root, "memgraph_pair", _two_test_binary(True, True))
    _ci(root, "memgraph_pair")
    assert wc.check_graph_test_serialized_on_db_lock() == []


def test_pass_db_guard_helper_form(isolated_tree: Any) -> None:
    # The `integration_graph` shape: a helper fn owns the lock.
    wc, root = isolated_tree
    src = (
        "//! Live graph binary using a db_guard() helper.\n"
        "async fn db_guard() -> MutexGuard<'static, ()> {\n"
        "    static DB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();\n"
        "    DB_LOCK.get_or_init(|| Mutex::new(())).lock().await\n"
        "}\n"
        "\n"
        "#[tokio::test]\n"
        '#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687)"]\n'
        "async fn graph_shape() {\n"
        "    let _db = db_guard().await;\n"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
        "\n"
        "#[tokio::test]\n"
        '#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687)"]\n'
        "async fn graph_teardown() {\n"
        "    let _db = db_guard().await;\n"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
    )
    _bin(root, "integration_graph", src)
    _ci(root, "integration_graph")
    assert wc.check_graph_test_serialized_on_db_lock() == []


def test_pass_single_live_test_is_exempt(isolated_tree: Any) -> None:
    # One live test can't self-collide — out of scope even without a lock and
    # even when absent from the CI leg.
    wc, root = isolated_tree
    src = (
        "//! Single live test.\n"
        "#[tokio::test]\n"
        '#[ignore = "requires live Neo4j (bolt://127.0.0.1:7687)"]\n'
        "async fn only_one() {\n"
        "    let c = BoltClient::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
    )
    _bin(root, "integration_symbol_context", src)
    _ci(root)  # not in the leg — still fine, it's single-test
    assert wc.check_graph_test_serialized_on_db_lock() == []


def test_pass_non_ignored_second_test_not_counted(isolated_tree: Any) -> None:
    # memgraph_integration's shape: one ignored live test + one non-ignored
    # negative test → only one live-graph test → out of scope.
    wc, root = isolated_tree
    src = (
        "//! One live + one non-ignored negative test.\n"
        "#[tokio::test]\n"
        '#[ignore = "requires the wylde-memgraph service on \\\\.\\pipe\\wylde-memgraph"]\n'
        "async fn live_smoke() {\n"
        "    let c = Client::new();\n"
        "    assert!(c.health().await.ok);\n"
        "}\n"
        "\n"
        "#[tokio::test]\n"
        "async fn dead_service_errors() {\n"
        "    let c = Client::for_service(\"known-dead\");\n"
        "    assert!(!c.health().await.ok);\n"
        "}\n"
    )
    _bin(root, "memgraph_integration", src)
    _ci(root)
    assert wc.check_graph_test_serialized_on_db_lock() == []


# ── FAIL cases (the synthetic offenders) ─────────────────────────────


def test_fail_one_test_missing_lock(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _bin(root, "memgraph_pair", _two_test_binary(True, False))
    _ci(root, "memgraph_pair")
    findings = wc.check_graph_test_serialized_on_db_lock()
    assert len(findings) == 1, findings
    f = findings[0]
    assert f.rule == "graph_test_serialized_on_db_lock"
    assert f.severity == "error"
    assert f.context == "round_trip_b"
    assert "does not serialize on a DB_LOCK" in f.message


def test_fail_no_lock_declared_flags_every_test(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _bin(root, "memgraph_pair", _two_test_binary(False, False, decl_lock=False))
    _ci(root, "memgraph_pair")
    findings = wc.check_graph_test_serialized_on_db_lock()
    names = sorted(f.context for f in findings)
    assert names == ["round_trip_a", "round_trip_b"], findings


def test_fail_guarded_but_absent_from_ci_leg(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _bin(root, "memgraph_pair", _two_test_binary(True, True))
    _ci(root)  # binary NOT wired into the leg
    findings = wc.check_graph_test_serialized_on_db_lock()
    assert len(findings) == 1, findings
    f = findings[0]
    assert f.line == 0
    assert f.context == "memgraph_pair"
    assert "not run in the live-graph leg" in f.message


def test_fail_in_ci_build_only_without_ignored_run(isolated_tree: Any) -> None:
    # Present on a `--no-run` build line but never run under `--ignored` is
    # still a dead gate — must be flagged.
    wc, root = isolated_tree
    _bin(root, "memgraph_pair", _two_test_binary(True, True))
    _write(
        root / ".github" / "workflows" / "ci.yml",
        "jobs:\n  live-graph:\n    steps:\n      - run: |\n"
        "          cargo test -p wylde-harness --locked --no-run --test memgraph_pair\n",
    )
    findings = wc.check_graph_test_serialized_on_db_lock()
    assert len(findings) == 1, findings
    assert findings[0].context == "memgraph_pair"
    assert "not run in the live-graph leg" in findings[0].message


# ── Fail-before / pass-after: a synthetic offender guarded turns green ─


def test_fail_before_pass_after(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Before: second test unguarded → red.
    _bin(root, "memgraph_pair", _two_test_binary(True, False))
    _ci(root, "memgraph_pair")
    assert len(wc.check_graph_test_serialized_on_db_lock()) == 1
    # After: guard it → green.
    _bin(root, "memgraph_pair", _two_test_binary(True, True))
    assert wc.check_graph_test_serialized_on_db_lock() == []
