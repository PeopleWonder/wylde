"""Tests for runtime hygiene rules (logging_setup_only,
no_external_subprocess, spawn_paths_exist, run_py_entry_point,
pipe_name_convention, run_py_startup_sequence,
shutdown_handler_marks_stopped) — mirrors prod-side
wylde_check/rules/_runtime.py.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 13: logging setup is centralized ────────────────────────────


def test_logging_setup_only_flags_basicconfig(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "evil.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
    )
    findings = wc.check_logging_setup_only()
    assert len(findings) == 1
    assert findings[0].rule == "logging_setup_only"
    assert findings[0].severity == "error"


def test_logging_setup_only_allows_source_of_truth(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "shared" / "logging_setup.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
    )
    assert wc.check_logging_setup_only() == []


def test_logging_setup_only_allows_tests(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "tests" / "test_x.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
    )
    assert wc.check_logging_setup_only() == []


# ── Rule 14: subprocess restriction ──────────────────────────────────


def test_no_external_subprocess_flags_random_module(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Gateway" / "evil.py",
        "import subprocess\nsubprocess.Popen(['echo'])\n",
    )
    findings = wc.check_no_external_subprocess()
    assert len(findings) == 1
    assert findings[0].rule == "no_external_subprocess"


def test_no_external_subprocess_allows_lifecycle(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        "import subprocess\nsubprocess.Popen(['python', '-m', 'X'])\n",
    )
    assert wc.check_no_external_subprocess() == []


def test_no_external_subprocess_allows_tool_runtimes(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "tooling" / "tools" / "git" / "_git_lib.py",
        "import subprocess\nsubprocess.run(['git', 'status'])\n",
    )
    assert wc.check_no_external_subprocess() == []


# ── Rule 15: spawn-command paths exist ───────────────────────────────


def test_spawn_paths_exist_clean_when_module_resolves(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "run.py", '"""voice run module"""\n')
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        'cmd = [sys.executable, "-m", "Voice.run"]\n',
    )
    assert wc.check_spawn_paths_exist() == []


def test_spawn_paths_exist_flags_missing_module(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        'cmd = [sys.executable, "-m", "Ghost.run"]\n',
    )
    findings = wc.check_spawn_paths_exist()
    assert len(findings) == 1
    assert "Ghost.run" in findings[0].message


def test_spawn_paths_exist_flags_missing_script(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        'cmd = ["python", "missing/script.py"]\n',
    )
    findings = wc.check_spawn_paths_exist()
    assert len(findings) == 1
    assert "missing/script.py" in findings[0].message


# ── Rule 16: run.py entry-point naming ───────────────────────────────


def test_run_py_entry_point_flags_deprecated_pattern(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "voice_run.py", "# legacy entry\n")
    findings = wc.check_run_py_entry_point()
    assert len(findings) == 1
    assert findings[0].rule == "run_py_entry_point"
    assert "voice_run.py" in findings[0].message


def test_run_py_entry_point_clean_when_run_py_exists(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "run.py", "# entry\n")
    assert wc.check_run_py_entry_point() == []


def test_run_py_entry_point_ignores_unlisted_folders(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Some random folder — not in SERVICE_FOLDERS — must be ignored.
    _write(root / "RandomDir" / "voice_run.py", "# random\n")
    assert wc.check_run_py_entry_point() == []


# ── Rule 17: pipe name convention ────────────────────────────────────


def test_pipe_name_convention_flags_underscore_in_pipe_path(isolated_tree: Any) -> None:
    """Underscores are only flagged when they appear in a Windows
    named-pipe path; otherwise legitimate Python identifiers like
    ``wylde_root`` and ``wylde_check`` would trip the rule."""
    wc, root = isolated_tree
    _write(
        root / "Voice" / "evil.py",
        'PATH = r"\\\\.\\pipe\\wylde_voice"  # typo — should be dash!\n',
    )
    findings = wc.check_pipe_name_convention()
    assert len(findings) == 1
    assert findings[0].rule == "pipe_name_convention"


def test_pipe_name_convention_ignores_python_identifiers(isolated_tree: Any) -> None:
    """A function named ``wylde_root`` or tool named ``wylde_check`` is
    NOT a pipe name and must not trip the rule."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "dev" / "x.py",
        "def wylde_root(): pass\nWYLDE_CHECK = 'wylde_check'\n",
    )
    # The checker itself is skipped in active code; this synthetic
    # file under Core/harness/dev/ is allowed because its content
    # doesn't include a pipe path.
    findings = wc.check_pipe_name_convention()
    assert findings == []


def test_pipe_name_convention_flags_uppercase(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "evil.py", 'PIPE_NAME = "wylde-Voice"\n')
    findings = wc.check_pipe_name_convention()
    assert len(findings) == 1
    assert "wylde-Voice" in findings[0].message


def test_pipe_name_convention_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "ok.py", 'PIPE_NAME = "wylde-voice"\n')
    assert wc.check_pipe_name_convention() == []


def test_pipe_name_convention_fires_on_rust(isolated_tree: Any) -> None:
    """Rule 17 walks .rs files too — uppercase / typo'd pipe names in
    Rust source get flagged just like Python."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        'const PIPE: &str = "wylde-Voice";\n',
    )
    findings = wc.check_pipe_name_convention()
    assert len(findings) == 1
    assert findings[0].rule == "pipe_name_convention"
    assert "wylde-Voice" in findings[0].message
    assert findings[0].file.endswith(".rs")


def test_pipe_name_convention_clean_rust(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "lib.rs",
        'const PIPE: &str = "wylde-voice";\n',
    )
    assert wc.check_pipe_name_convention() == []


# ── Rule 18: run.py startup sequence ─────────────────────────────────


def test_run_py_startup_sequence_clean_when_all_present(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "run.py",
        "from Core.shared.logging_setup import configure_logging\n"
        "configure_logging(service='wylde-voice')\n"
        "write_manifest()\n"
        "start_heartbeat()\n"
        "serve_forever()\n",
    )
    findings = wc.check_run_py_startup_sequence()
    assert findings == []


def test_run_py_startup_sequence_warns_on_missing_steps(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "run.py",
        "configure_logging()\nserve_forever()\n",
    )
    findings = wc.check_run_py_startup_sequence()
    # Expect warnings for write_manifest and start_heartbeat being missing.
    rules = [f.rule for f in findings]
    assert rules == ["run_py_startup_sequence"] * len(findings)
    messages = " | ".join(f.message for f in findings)
    assert "write_manifest" in messages
    assert "start_heartbeat" in messages


# ── Rule 19: shutdown handler marks stopped ──────────────────────────


def test_shutdown_handler_marks_stopped_warns_no_signal(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Voice" / "run.py", "def main(): pass\n")
    findings = wc.check_shutdown_handler_marks_stopped()
    assert len(findings) == 1
    assert findings[0].rule == "shutdown_handler_marks_stopped"
    assert findings[0].severity == "warning"


def test_shutdown_handler_marks_stopped_warns_signal_without_cleanup(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "run.py",
        "import signal\nsignal.signal(signal.SIGTERM, lambda *_: None)\n",
    )
    findings = wc.check_shutdown_handler_marks_stopped()
    assert len(findings) == 1
    assert "manifest-cleanup" in findings[0].message


def test_shutdown_handler_marks_stopped_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "run.py",
        "import signal\n"
        "def _handler(*_): mark_stopped()\n"
        "signal.signal(signal.SIGTERM, _handler)\n",
    )
    assert wc.check_shutdown_handler_marks_stopped() == []


# ── Rule 31: shutdown reaps manifest orphans ─────────────────────────


# Repointed for #116.  These tests used to build a synthetic
# ``Core/Lifecycle/daemon_state/__init__.py`` and assert against a Python
# ``ast`` walk looking for a ``reap*orphan*`` call inside
# ``stop_all_daemon_managed``.  The Rust cutover deleted that whole tree,
# so the rule was parsing a file that could not exist and passing.
#
# The guarantee did not disappear with the Python daemon — it MOVED.
# Teardown no longer reaps (``stop_all_daemon_managed`` now only halts
# the recurring sweep so an in-flight tick cannot rewrite a manifest
# mid-shutdown); the one-shot reap runs on the BOOT path instead.  So
# these tests keep their original intent and follow it to its new home:
# the sweep must be defined in ``state/orphan_sweep.rs`` and called from
# ``daemon.rs`` before the first ``start_<service>()``.
#
# Missing-file, comment-only, ordering, and ``stop_``-prefix cases live
# in ``test_dead_path_rules.py`` and are not duplicated here.
_SWEEP_REL = "rust/crates/wylde-lifecycle/src/state/orphan_sweep.rs"
_DAEMON_REL = "rust/crates/wylde-lifecycle/src/daemon.rs"


def _write_sweep(root: Any, src: str) -> None:
    """Drop the module that must DEFINE the orphan sweep."""
    _write(root / _SWEEP_REL, src)


def _write_daemon(root: Any, src: str) -> None:
    """Drop the daemon boot path that must CALL the orphan sweep."""
    _write(root / _DAEMON_REL, src)


def test_shutdown_reaps_manifest_orphans_clean_with_bare_call(
    isolated_tree: Any,
) -> None:
    """The canonical shape: a bare ``boot_orphan_sweep()`` call on the
    boot path, ahead of the first service launch, satisfies the rule."""
    wc, root = isolated_tree
    _write_sweep(root, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write_daemon(
        root,
        "// Phase 2b-sweep\n"
        "let report = boot_orphan_sweep();\n"
        "start_gateway().await;\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert findings == []


def test_shutdown_reaps_manifest_orphans_clean_with_attribute_call(
    isolated_tree: Any,
) -> None:
    """A path-qualified call like ``crate::state::boot_orphan_sweep()``
    is equally valid — the rule keys off the function identifier, not the
    module prefix, so the sweep can be re-exported or moved between
    modules without churning the rule."""
    wc, root = isolated_tree
    _write_sweep(root, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write_daemon(
        root,
        "let report = crate::state::boot_orphan_sweep();\n"
        "start_gateway().await;\n",
    )
    assert wc.check_shutdown_reaps_manifest_orphans() == []


def test_shutdown_reaps_manifest_orphans_flags_missing_call(
    isolated_tree: Any,
) -> None:
    """The orphan defect itself: the daemon boots services without ever
    sweeping, so a manifest left alive-marked by an ungraceful prior exit
    survives the restart and its service stays dark."""
    wc, root = isolated_tree
    _write_sweep(root, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write_daemon(
        root,
        "// Boots the tracked roster, but nothing reconciles the manifest\n"
        "// dir against reality first.\n"
        "start_gateway().await;\n"
        "start_voice().await;\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
    assert findings[0].rule == "shutdown_reaps_manifest_orphans"
    assert findings[0].severity == "error"
    assert "never calls a manifest orphan sweep" in findings[0].message


def test_shutdown_reaps_manifest_orphans_flags_missing_function(
    isolated_tree: Any,
) -> None:
    """If the sweep module exists but declares no public sweep function,
    the rule emits a structural-rename finding rather than passing.

    A present-but-empty file is the shape a refactor leaves behind, and
    it is exactly what a substring/loader check would read as "fine"
    (#116).
    """
    wc, root = isolated_tree
    _write_sweep(root, "pub fn some_other_helper() -> () {}\n")
    _write_daemon(root, "boot_orphan_sweep();\nstart_gateway().await;\n")
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
    assert findings[0].severity == "error"
    assert "declares no public orphan-sweep function" in findings[0].message


def test_shutdown_reaps_manifest_orphans_accepts_alternate_name(
    isolated_tree: Any,
) -> None:
    """The rule is name-pattern-bound, not name-specific — any
    ``*orphan*sweep*`` / ``*sweep*orphan*``-shaped identifier counts, so
    the implementation can be renamed without churning the rule."""
    wc, root = isolated_tree
    _write_sweep(root, "pub fn sweep_boot_orphans() -> BootSweepReport {}\n")
    _write_daemon(root, "sweep_boot_orphans();\nstart_gateway().await;\n")
    assert wc.check_shutdown_reaps_manifest_orphans() == []


def test_shutdown_reaps_manifest_orphans_rejects_unrelated_call(
    isolated_tree: Any,
) -> None:
    """An unrelated call sitting where the sweep belongs does NOT satisfy
    the rule — the name pattern guards against accidental drift, so a
    reordering refactor that drops the sweep cannot be papered over by
    whatever call happens to remain on the boot path."""
    wc, root = isolated_tree
    _write_sweep(root, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write_daemon(
        root,
        "flush_log_buffers();  // important, but reaps nothing\n"
        "start_gateway().await;\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
    assert "never calls a manifest orphan sweep" in findings[0].message
