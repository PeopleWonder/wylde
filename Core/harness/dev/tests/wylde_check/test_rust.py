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
    """THREE-or-more `super::` hops is the "module graph is wrong" smell the
    finding message describes; that is the level the rule flags."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "a" / "b" / "c" / "d.rs",
        "use super::super::super::sibling::thing;\n",
    )
    findings = wc.check_import_paths_rust()
    assert len(findings) == 1
    assert "super::super" in findings[0].message


def test_import_paths_rust_allows_two_level_super(isolated_tree: Any) -> None:
    """Two `super::` hops (a nested module reaching a grandparent's sibling,
    including from an inline `#[cfg(test)]` module) is ordinary Rust — not the
    "three or more levels up" case the message calls out."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "a" / "b.rs",
        "fn t() { let _ = super::super::config::DEFAULT; }\n",
    )
    assert wc.check_import_paths_rust() == []


def test_import_paths_rust_allows_shared_surface_crate(isolated_tree: Any) -> None:
    """Pure-library / client crates on the shared-surface allowlist are
    importable like wylde_shared (using the surface, not bypassing a pipe)."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "use wylde_workspaces_client::WorkspacesClient;\n"
        "use wylde_stack::service_name;\n",
    )
    assert wc.check_import_paths_rust() == []


def test_import_paths_rust_per_edge_exemption(isolated_tree: Any) -> None:
    """The wylde-gateway → wylde-harness facade edge is a documented carve-out;
    the same import from any other crate is still flagged."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-gateway" / "src" / "routes" / "settings.rs",
        "use wylde_harness::settings::actions::handle_get_overrides;\n",
    )
    assert wc.check_import_paths_rust() == []
    _write(
        root / "rust" / "crates" / "wylde-other" / "src" / "lib.rs",
        "use wylde_harness::settings::actions::handle_get_overrides;\n",
    )
    findings = wc.check_import_paths_rust()
    assert len(findings) == 1
    assert findings[0].file.endswith("wylde-other/src/lib.rs")


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


def test_no_silent_error_swallow_skips_bound_ok(isolated_tree: Any) -> None:
    """`let prev = expr.ok();` KEEPS the Option (Result→Option conversion) —
    the value is retained, not swallowed — so a bound `.ok()` is not flagged.
    A bare `expr.ok();` still is."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n"
        '    let prev = std::env::var("WYLDE_X").ok();\n'
        "    self.cached = compute().ok();\n"
        "    bare_call().ok();\n"
        "}\n",
    )
    findings = wc.check_no_silent_error_swallow_rust()
    assert len(findings) == 1
    assert findings[0].line == 4  # only the bare `bare_call().ok();`


def test_no_silent_error_swallow_marker_on_adjacent_line(isolated_tree: Any) -> None:
    """rustfmt parks an overflowing trailing marker comment on the following
    line; the discard opt-out is still honoured when the marker sits on the
    line directly below (or above) the statement."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() {\n"
        "    let _ = reply.send(really_long_value);\n"
        "    // best-effort reply (wylde-check: discard-result-ok)\n"
        "}\n",
    )
    assert wc.check_no_silent_error_swallow_rust() == []


def test_no_silent_error_swallow_skips_propagating_question_mark(
    isolated_tree: Any,
) -> None:
    """`let _ = expr?;` PROPAGATES the error via `?` (only the Ok value is
    dropped), so it is not a silent swallow."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        "fn x() -> Result<(), E> {\n    let _ = state::load()?;\n    Ok(())\n}\n",
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


def test_logging_setup_rust_inline_marker_suppresses(isolated_tree: Any) -> None:
    """An MCP stdio server must log to stderr (stdout is its JSON-RPC channel),
    so it cannot use configure_logging's stdout writer — opt out inline."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-ext-study" / "src" / "main.rs",
        "fn init() {\n"
        "    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr)"
        ".try_init();  // wylde-check: logging-init-ok\n}\n",
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


def test_process_spawn_rust_inline_marker_suppresses(isolated_tree: Any) -> None:
    """A single justified spawn opts out with `// wylde-check: external-spawn-ok`
    without widening the crate allowlist (which would wave through every spawn
    in the crate)."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-workspaces" / "src" / "blame.rs",
        'fn x() {\n    // local git blame, no lifecycle-pipe equivalent\n'
        '    let _o = std::process::Command::new("git");  '
        "// wylde-check: external-spawn-ok\n}\n",
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
