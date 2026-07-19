"""Tests for the four Rust-side rules introduced in W1.9-W1.12:
import_paths_rust, no_silent_error_swallow_rust,
logging_setup_only_rust, no_external_process_spawn_rust.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 26: import_paths_rust ───────────────────────────────────────


def test_import_paths_rust_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "use wylde_shared::ipc::call_action;\nuse crate::config::Config;\n",
    )
    assert wc.check_import_paths_rust() == []


def test_import_paths_rust_flags_cross_crate(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "use wylde_gateway::routes::handler;\n",
    )
    findings = wc.check_import_paths_rust()
    assert len(findings) == 1
    assert findings[0].rule == "import_paths_rust"
    assert "wylde_gateway" in findings[0].message
    assert findings[0].severity == "error"


def test_import_paths_rust_allows_own_crate(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-vram-broker" / "src" / "lib.rs",
        "use wylde_vram_broker::registry::registry;\n",
    )
    assert wc.check_import_paths_rust() == []


def test_import_paths_rust_flags_deep_super(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "a" / "b" / "c.rs",
        "use super::super::sibling::thing;\n",
    )
    findings = wc.check_import_paths_rust()
    assert len(findings) == 1
    assert "super::super" in findings[0].message


# ── Rule 27: no_silent_error_swallow_rust ────────────────────────────


def test_no_silent_error_swallow_flags_let_underscore(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n    let _ = atomic_write(&path, &data);\n}\n",
    )
    findings = wc.check_no_silent_error_swallow_rust()
    assert len(findings) == 1
    assert "let _" in findings[0].message
    assert findings[0].severity == "error"


def test_no_silent_error_swallow_flags_dot_ok(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x(p: &Path) {\n    std::fs::remove_file(p).ok();\n}\n",
    )
    findings = wc.check_no_silent_error_swallow_rust()
    assert len(findings) == 1
    assert ".ok()" in findings[0].message


def test_no_silent_error_swallow_marker_suppresses(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n    let _ = atomic_write(&path, &data);  // wylde-check: discard-result-ok\n}\n",
    )
    assert wc.check_no_silent_error_swallow_rust() == []


def test_no_silent_error_swallow_skips_non_result_let_underscore(
    isolated_tree: Any,
) -> None:
    """Heuristic: let _ = Vec::new() etc. is not a Result discard."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n    let _ = tokio::spawn(async {});\n    let _ = Vec::<u8>::new();\n}\n",
    )
    assert wc.check_no_silent_error_swallow_rust() == []


# ── Rule 28: logging_setup_only_rust ─────────────────────────────────


def test_logging_setup_rust_flags_direct_init(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n    tracing_subscriber::fmt().init();\n}\n",
    )
    findings = wc.check_logging_setup_only_rust()
    assert len(findings) == 1
    assert findings[0].rule == "logging_setup_only_rust"
    assert findings[0].severity == "error"


def test_logging_setup_rust_exempts_canonical(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-shared" / "src" / "logging.rs",
        "fn x() {\n    let _ = tracing_subscriber::fmt().try_init();\n}\n",
    )
    findings = wc.check_logging_setup_only_rust()
    assert findings == []


def test_logging_setup_rust_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "use wylde_shared::logging::configure_logging;\n"
        'fn x() { configure_logging(Some("foo"), tracing::Level::INFO); }\n',
    )
    assert wc.check_logging_setup_only_rust() == []


# ── Rule 29: no_external_process_spawn_rust ──────────────────────────


def test_process_spawn_rust_flags_std_command(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        'fn x() {\n    let _c = std::process::Command::new("ls");\n}\n',
    )
    findings = wc.check_no_external_process_spawn_rust()
    assert len(findings) == 1
    assert findings[0].rule == "no_external_process_spawn_rust"
    assert findings[0].severity == "error"


def test_process_spawn_rust_flags_tokio_command(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        'fn x() {\n    let _c = tokio::process::Command::new("ls");\n}\n',
    )
    findings = wc.check_no_external_process_spawn_rust()
    assert len(findings) == 1


def test_process_spawn_rust_allows_lifecycle_crate(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-lifecycle" / "src" / "spawn.rs",
        'fn x() {\n    let _c = tokio::process::Command::new("voice");\n}\n',
    )
    assert wc.check_no_external_process_spawn_rust() == []


def test_process_spawn_rust_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() { /* no spawn */ }\n",
    )
    assert wc.check_no_external_process_spawn_rust() == []


# ── Rule 54: no_unbounded_log_sink_rust ──────────────────────────────


def test_unbounded_log_sink_flags_raw_append(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "sink.rs",
        "fn x() {\n"
        "    let _f = OpenOptions::new().create(true).append(true).open(p);\n"
        "}\n",
    )
    findings = wc.check_no_unbounded_log_sink_rust()
    assert len(findings) == 1
    assert findings[0].rule == "no_unbounded_log_sink_rust"
    assert findings[0].severity == "error"


def test_unbounded_log_sink_flags_tokio_append(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "sink.rs",
        "async fn x() {\n"
        "    let _f = tokio::fs::OpenOptions::new().append(true).open(p).await;\n"
        "}\n",
    )
    findings = wc.check_no_unbounded_log_sink_rust()
    assert len(findings) == 1


def test_unbounded_log_sink_exempts_rotation_factory(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # The canonical logging module IS the factory — its append is allowed.
    _write(
        root / "rust" / "crates" / "wylde-shared" / "src" / "logging.rs",
        "fn open() {\n"
        "    let _f = OpenOptions::new().create(true).append(true).open(p);\n"
        "}\n",
    )
    assert wc.check_no_unbounded_log_sink_rust() == []


def test_unbounded_log_sink_marker_suppresses(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "data.rs",
        "fn x() {\n"
        "    // not a log — a resumable download temp file\n"
        "    let _f = OpenOptions::new().append(true).open(p); "
        "// wylde-check: unbounded-append-ok\n"
        "}\n",
    )
    assert wc.check_no_unbounded_log_sink_rust() == []


def test_unbounded_log_sink_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "sink.rs",
        "fn x() {\n"
        "    rotating_sink(&path).write_line(&line)?;\n"
        "}\n",
    )
    assert wc.check_no_unbounded_log_sink_rust() == []
