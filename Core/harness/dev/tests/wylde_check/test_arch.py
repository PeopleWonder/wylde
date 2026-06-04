"""Tests for architectural rules (no_internal_http, manifest_paths,
import_paths, dead_service_refs, memory_layer_boundaries,
service_owns_its_state) — mirrors prod-side wylde_check/rules/_arch.py.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 1: no internal HTTP ──────────────────────────────────────────


def test_no_internal_http_flags_python_call(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "evil.py",
        "import requests\nrequests.post('http://127.0.0.1:8005/api/foo', json={})\n",
    )
    findings = wc.check_no_internal_http()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "no_internal_http"
    assert f.severity == "error"
    assert f.file == "Core/harness/evil.py"
    assert f.line == 2
    assert "127.0.0.1" in f.context


def test_no_internal_http_no_longer_exempts_gateway(isolated_tree: Any) -> None:
    # The Gateway exemption was pruned once the strangler deleted the
    # Gateway Python source (rust-only collapse); no .py remains under the
    # prefix, so the exemption matched nothing and was removed. A synthetic
    # Gateway Python file must now be flagged like any other internal HTTP.
    wc, root = isolated_tree
    _write(
        root / "Gateway" / "routes" / "egress.py",
        "import requests\nrequests.post('http://127.0.0.1:8005/api/foo', json={})\n",
    )
    findings = wc.check_no_internal_http()
    assert len(findings) == 1, (
        "Gateway is no longer exempt; internal HTTP in Gateway Python must flag"
    )
    assert findings[0].rule == "no_internal_http"
    assert findings[0].file == "Gateway/routes/egress.py"


def test_no_internal_http_exempts_ollama_client(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "backend" / "ollama_client.py",
        "import requests\nrequests.post('http://127.0.0.1:11434/api/chat', json={})\n",
    )
    assert wc.check_no_internal_http() == []


def test_no_internal_http_skips_tests_dir(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "tests" / "test_something.py",
        "import requests\nrequests.post('http://127.0.0.1:8005/api/foo')\n",
    )
    assert wc.check_no_internal_http() == []


# ── Rule 2: single manifest write path per service ────────────────────


def test_manifest_paths_flags_double_write(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Daemon claims to manage wylde-voice via _write_daemon_manifest.
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        '_write_daemon_manifest("wylde-voice", pid=os.getpid())\n',
    )
    # Voice/run.py ALSO calls write_manifest — the violation.
    _write(
        root / "Voice" / "run.py",
        'write_manifest(service_name="wylde-voice", port=0)\n',
    )
    findings = wc.check_manifest_paths()
    assert len(findings) == 1
    assert findings[0].rule == "manifest_paths"
    assert findings[0].file == "Voice/run.py"


def test_manifest_paths_clean_when_daemon_owns_alone(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon_state.py",
        '_write_daemon_manifest("wylde-voice", pid=os.getpid())\n',
    )
    _write(
        root / "Voice" / "run.py",
        "# Voice is daemon-managed; manifest written by daemon.\n",
    )
    assert wc.check_manifest_paths() == []


# ── Rule 5: import path consistency ───────────────────────────────────


def test_import_paths_flags_wylde_core(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "mod.py", "from Wylde.Core.shared import ipc\n")
    findings = wc.check_import_paths()
    assert len(findings) == 1
    assert findings[0].rule == "import_paths"
    assert findings[0].file == "Core/harness/mod.py"


def test_import_paths_clean_bare_core(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "mod.py", "from Core.shared import ipc\n")
    assert wc.check_import_paths() == []


def test_import_paths_exempts_tests(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "tests" / "test_x.py",
        "from Wylde.Core.shared import ipc\n",
    )
    assert wc.check_import_paths() == []


# ── Rule 6: dead service references ───────────────────────────────────


def test_dead_service_refs_flags_known_dead_name(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "mod.py",
        "SVC = 'wylde-orchestrator'  # dead reference\n",
    )
    findings = wc.check_dead_service_refs()
    assert len(findings) >= 1
    assert any(f.rule == "dead_service_refs" for f in findings)
    assert any("wylde-orchestrator" in f.message for f in findings)


def test_dead_service_refs_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "mod.py", "SVC = 'wylde-harness'  # live name\n")
    assert wc.check_dead_service_refs() == []


def test_dead_service_refs_skips_legacy_dirs(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "_legacy" / "foo.py", "x = 'wylde-orchestrator'\n")
    assert wc.check_dead_service_refs() == []


def test_dead_service_refs_fires_on_rust(isolated_tree: Any) -> None:
    """Rule 6 walks .rs files too — Rust crates citing dead names get
    flagged just like Python."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "main.rs",
        'const TARGET: &str = "wylde-orchestrator";\n',
    )
    findings = wc.check_dead_service_refs()
    assert len(findings) == 1
    assert findings[0].rule == "dead_service_refs"
    assert findings[0].file == "rust/crates/wylde-foo/src/main.rs"
    assert "wylde-orchestrator" in findings[0].message


def test_dead_service_refs_honours_rust_marker(isolated_tree: Any) -> None:
    """The inline marker suppresses the Rust line just like the Python
    one (the rule is host-language agnostic for suppression)."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "main.rs",
        'const TARGET: &str = "wylde-orchestrator";  // wylde-check: dead-ref-ok\n',
    )
    assert wc.check_dead_service_refs() == []


# ── Rule 22: memory-layer boundaries ────────────────────────────────


def test_memory_layer_boundaries_flags_outside_caller(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "evil.py",
        'p = "memory/long_term/store.json"\n',
    )
    findings = wc.check_memory_layer_boundaries()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "memory_layer_boundaries"
    assert f.severity == "error"
    assert f.file == "Core/Lifecycle/evil.py"
    assert "memory/long_term" in f.message


def test_memory_layer_boundaries_allows_inside_layer(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "memory" / "long_term.py",
        'PATH = "memory/long_term/store.json"\n',
    )
    assert wc.check_memory_layer_boundaries() == []


def test_memory_layer_boundaries_skips_comments(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "ok.py",
        "# stores into memory/long_term/ when wired\n",
    )
    assert wc.check_memory_layer_boundaries() == []


# ── Rule 25: service owns its state ─────────────────────────────────


def test_service_owns_its_state_flags_cross_service(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Gateway" / "auth" / "evil.py",
        'PATH = "device_gate/data/approved.json"\n',
    )
    findings = wc.check_service_owns_its_state()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "service_owns_its_state"
    assert f.severity == "error"
    assert f.file == "Gateway/auth/evil.py"
    assert "device_gate" in f.message


def test_service_owns_its_state_allows_self_access(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "device_gate" / "store.py",
        'PATH = "device_gate/data/approved.json"\n',
    )
    assert wc.check_service_owns_its_state() == []


def test_service_owns_its_state_exempts_lifecycle_daemon(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon.py",
        'GATE_PATH = "device_gate/data/approved.json"\n'
        'VOICE_DATA = "Voice/data/state.json"\n',
    )
    # Lifecycle daemon legitimately knows about every service's state.
    assert wc.check_service_owns_its_state() == []
