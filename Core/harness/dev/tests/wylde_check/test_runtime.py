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


_SHUTDOWN_REL = "Core/Lifecycle/daemon_state/__init__.py"


def test_shutdown_reaps_manifest_orphans_clean_with_bare_call(
    isolated_tree: Any,
) -> None:
    """The canonical fix shape: a bare ``reap_manifest_orphans()`` call
    inside ``stop_all_daemon_managed`` satisfies the rule."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "def stop_all_daemon_managed():\n"
        "    # tracked stops elided\n"
        "    reaped = reap_manifest_orphans()\n"
        "    return {'reaped': reaped}\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert findings == []


def test_shutdown_reaps_manifest_orphans_clean_with_attribute_call(
    isolated_tree: Any,
) -> None:
    """An attribute call like ``_orphan_sweep.reap_manifest_orphans()``
    is also valid — the rule keys off the rightmost identifier."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "from . import _orphan_sweep\n"
        "def stop_all_daemon_managed():\n"
        "    _orphan_sweep.reap_manifest_orphans()\n"
        "    return {}\n",
    )
    assert wc.check_shutdown_reaps_manifest_orphans() == []


def test_shutdown_reaps_manifest_orphans_flags_missing_call(
    isolated_tree: Any,
) -> None:
    """The shutdown-orphan defect itself: ``stop_all_daemon_managed``
    only walks in-memory Popen handles, never reaps the manifest dir."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "def stop_all_daemon_managed():\n"
        "    # walks _gateway_proc / _voice_proc / etc — but no\n"
        "    # manifest-walking safety net for orphans from prior\n"
        "    # crashed daemon sessions.\n"
        "    _stop_gateway()\n"
        "    _stop_voice()\n"
        "    return {}\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
    assert findings[0].rule == "shutdown_reaps_manifest_orphans"
    assert findings[0].severity == "error"
    assert "does not call a manifest-orphan reaper" in findings[0].message


def test_shutdown_reaps_manifest_orphans_flags_missing_function(
    isolated_tree: Any,
) -> None:
    """If the canonical function isn't there at all, the rule emits a
    structural-rename finding rather than silently passing."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "def some_other_function():\n    pass\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
    assert "stop_all_daemon_managed" in findings[0].message


def test_shutdown_reaps_manifest_orphans_accepts_alternate_name(
    isolated_tree: Any,
) -> None:
    """The rule is name-pattern-bound, not name-specific — any
    ``reap*orphan*``-shaped identifier counts so the implementation can
    rename without churning the rule."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "def stop_all_daemon_managed():\n"
        "    _reap_live_orphans()  # alternate naming, still matches\n"
        "    return {}\n",
    )
    assert wc.check_shutdown_reaps_manifest_orphans() == []


def test_shutdown_reaps_manifest_orphans_rejects_unrelated_call(
    isolated_tree: Any,
) -> None:
    """An unrelated call that happens to live next to the spot where
    the reaper should sit does NOT satisfy the rule — pattern guards
    against accidental drift."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN_REL,
        "def stop_all_daemon_managed():\n"
        "    _flush_log_buffers()  # important, but not the reaper\n"
        "    return {}\n",
    )
    findings = wc.check_shutdown_reaps_manifest_orphans()
    assert len(findings) == 1
