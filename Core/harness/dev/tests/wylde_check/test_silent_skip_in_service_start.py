"""Tests for rule 52 (``silent_skip_in_service_start``) — mirrors prod-side
``wylde_check/rules/_silent_skip_in_service_start.py``.

A silent ``return Ok(())`` in a lifecycle ``start_<service>`` function skips
a spawn with nothing in the daemon log to say why — exactly the
stale-manifest outage that left harness / extension_bridge / ollama /
trainer_worker / trainer dark on 2026-05-31. Every skip must log a reason.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

SERVICES = (
    "rust",
    "crates",
    "wylde-lifecycle",
    "src",
    "state",
)


def _services(root: Any, body: str) -> None:
    path = root
    for part in SERVICES:
        path = path / part
    _write(path / "services.rs", body)


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_alive_guard_logs_before_return(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_harness() -> Result<()> {\n"
        "    if is_service_alive(HARNESS) {\n"
        "        let pid = manifest_pid(HARNESS).unwrap_or(0);\n"
        '        tracing::info!("{}: already alive (manifest pid={}); skipping spawn", HARNESS, pid);\n'
        "        return Ok(());\n"
        "    }\n"
        "    record_spawn(HARNESS, pid, \"rust\");\n"
        "    Ok(())\n"
        "}\n",
    )
    assert wc.check_silent_skip_in_service_start() == []


def test_pass_nospawn_branch_logs(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_voice() -> Result<()> {\n"
        "    if nospawn_enabled() {\n"
        "        nospawn_record(VOICE, \"rust\");\n"
        '        tracing::info!("voice: NO-SPAWN — would-have-spawned recorded; no child forked");\n'
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    assert wc.check_silent_skip_in_service_start() == []


def test_pass_successful_spawn_tail_not_flagged(isolated_tree: Any) -> None:
    # The tail `Ok(())` is an expression, not a `return Ok` — never matched.
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_gateway() -> Result<()> {\n"
        "    let child = spawn_rust_binary(GATEWAY, &bin)?;\n"
        "    let pid = child.id().unwrap_or(0);\n"
        '        tracing::info!("daemon: spawned gateway pid={}", pid);\n'
        "    record_spawn(GATEWAY, pid, \"rust\");\n"
        "    set_service_proc(GATEWAY, child);\n"
        "    Ok(())\n"
        "}\n",
    )
    assert wc.check_silent_skip_in_service_start() == []


def test_pass_non_start_fn_silent_return_ignored(isolated_tree: Any) -> None:
    # stop_service legitimately returns Ok early without logging — only
    # start_* functions are governed.
    wc, root = isolated_tree
    _services(
        root,
        "async fn stop_service(name: &str, grace: Duration) -> Result<()> {\n"
        "    forget_spawn(name);\n"
        "    if nospawn_enabled() {\n"
        "        nospawn_take(name);\n"
        "        return Ok(());\n"
        "    }\n"
        "    let Some(child) = take_service_proc(name) else {\n"
        "        return Ok(());\n"
        "    };\n"
        "    graceful_stop(name, child, grace).await\n"
        "}\n",
    )
    assert wc.check_silent_skip_in_service_start() == []


def test_pass_opt_out_marker(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_trainer() -> Result<()> {\n"
        "    if is_service_alive(TRAINER) {\n"
        "        // wylde-check: silent-skip-allowed\n"
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    assert wc.check_silent_skip_in_service_start() == []


def test_pass_format_string_braces_do_not_unbalance(isolated_tree: Any) -> None:
    # The `{}` in the tracing format string must be stripped, or the brace
    # tracking would lose the block and false-fire / false-pass.
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_ollama() -> Result<()> {\n"
        "    if is_service_alive(OLLAMA) {\n"
        '        tracing::info!("{}: already alive (manifest pid={}); skipping spawn", OLLAMA, pid);\n'
        "        return Ok(());\n"
        "    }\n"
        "    if other_branch() {\n"
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    # First branch logs (OK); the SECOND branch is silent → exactly one find.
    found = wc.check_silent_skip_in_service_start()
    assert len(found) == 1
    assert found[0].line == 7


# ── FAIL cases (findings expected) ───────────────────────────────────


def test_fail_bare_alive_guard(isolated_tree: Any) -> None:
    # The exact bug: stale-manifest alive guard returns Ok with no log.
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_harness() -> Result<()> {\n"
        "    if is_service_alive(HARNESS) {\n"
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    found = wc.check_silent_skip_in_service_start()
    assert len(found) == 1
    assert found[0].rule == "silent_skip_in_service_start"
    assert found[0].severity == "error"
    assert found[0].line == 3


def test_fail_tracing_in_sibling_block_not_this_one(isolated_tree: Any) -> None:
    # A tracing call in a SIBLING block must not satisfy the return's block.
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_vpn() -> Result<()> {\n"
        "    if a {\n"
        '        tracing::info!("logged here");\n'
        "    }\n"
        "    if is_service_alive(VPN) {\n"
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    found = wc.check_silent_skip_in_service_start()
    assert len(found) == 1
    assert found[0].line == 6


def test_fail_return_ok_with_value_also_flagged(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_device_gate() -> Result<()> {\n"
        "    if is_service_alive(DEVICE_GATE) {\n"
        "        return Ok(skip_marker());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    found = wc.check_silent_skip_in_service_start()
    assert len(found) == 1
    assert found[0].line == 3


def test_fail_commented_tracing_does_not_count(isolated_tree: Any) -> None:
    # A `// tracing::...` comment must not satisfy the log requirement.
    wc, root = isolated_tree
    _services(
        root,
        "async fn start_memgraph() -> Result<()> {\n"
        "    if is_service_alive(MEMGRAPH) {\n"
        "        // tracing::info!(\"would log but commented out\");\n"
        "        return Ok(());\n"
        "    }\n"
        "    Ok(())\n"
        "}\n",
    )
    found = wc.check_silent_skip_in_service_start()
    assert len(found) == 1
    assert found[0].line == 4


# ── dispatcher wiring ────────────────────────────────────────────────


def test_rule52_registered_in_dispatcher(isolated_tree: Any) -> None:
    wc, _ = isolated_tree
    result = wc.run_all(only=["silent_skip_in_service_start"])
    assert result["ok"] is True
    assert "silent_skip_in_service_start" in result["data"]["summary"]["by_rule"]
