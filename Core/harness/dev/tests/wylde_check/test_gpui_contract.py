"""Tests for the three panel↔harness-contract rules introduced 2026-05-29
(rules 38-40): panel_verbs_exist_in_harness_registry, nav_targets_exist,
required_services_includes_called_services.

Mirrors prod-side ``wylde_check/rules/_gpui_contract.py``.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write


# ── Shared fixture builders ──────────────────────────────────────────


def _seed_harness_registry(root: Any, rust_verbs: list[str]) -> None:
    """Drop the harness pipe registry: a Rust ``ALL_PIPE_ACTIONS`` array
    at ``rust/crates/wylde-harness/src/pipe/mod.rs``.

    Two things changed here for #116.  The path was ``src/pipe.rs``,
    which the crate outgrew when ``pipe`` became a module directory; and
    there was a second, Python ``_ACTIONS`` half at
    ``Core/harness/pipe/__init__.py`` that the rule unioned in.  The Rust
    cutover deleted the Python tree, so there is exactly one registry
    now, and it is mandatory — every rule-38 test must seed it or the
    rule correctly reports that it cannot run.
    """
    body = ",\n".join(f'    "{v}"' for v in rust_verbs)
    _write(
        root / "rust" / "crates" / "wylde-harness" / "src" / "pipe" / "mod.rs",
        "pub const ALL_PIPE_ACTIONS: &[&str] = &[\n" + body + ",\n];\n",
    )


def _seed_panel(
    root: Any,
    panel_name: str,
    *,
    service: str = "core",
    panel_id: str = "foo",
    required_services: list[str] | None = None,
    ipc_body: str = "",
) -> None:
    """Drop a minimal first-party panel manifest + ipc.rs under
    ``Core/GUI/Frontend/Panels/<panel_name>/``."""
    manifest = {
        "schema_version": 2,
        "service": service,
        "panels": [
            {
                "id": panel_id,
                "title": panel_id.title(),
                "required_services": required_services or [],
                "source": {
                    "kind": "gpui_view",
                    "factory": f"wylde_panel_{panel_id}::Panel::view",
                },
            }
        ],
    }
    base = root / "Core" / "GUI" / "Frontend" / "Panels" / panel_name
    _write(base / "manifest.json", json.dumps(manifest))
    _write(base / "src" / "ipc.rs", ipc_body)


# ── Rule 38: panel_verbs_exist_in_harness_registry ───────────────────


def test_rule38_clean_when_verb_in_registry(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            "    let _ = wylde_gui_pipe::call(\n"
            "        SVC_HARNESS,\n"
            '        "POST",\n'
            '        "/__action__",\n'
            '        Some(json!({ "action": "chat.start_turn", "payload": {} })),\n'
            "    );\n"
            "}\n"
        ),
    )
    assert wc.check_panel_verbs_exist_in_harness_registry() == []


def test_rule38_flags_typo_verb(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            "    let _ = wylde_gui_pipe::call(\n"
            "        SVC_HARNESS,\n"
            '        "POST",\n'
            '        "/__action__",\n'
            '        Some(json!({ "action": "chat.start_tunr", "payload": {} })),\n'
            "    );\n"
            "}\n"
        ),
    )
    findings = wc.check_panel_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert findings[0].rule == "panel_verbs_exist_in_harness_registry"
    assert "chat.start_tunr" in findings[0].message
    assert findings[0].severity == "error"


def test_rule38_flags_unknown_stream_call_verb(isolated_tree: Any) -> None:
    """The ``stream_call`` form carries its verb in arg-1 rather than in
    a json! envelope; it must be checked against the registry too."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.stream_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::stream_call(SVC_HARNESS, "models.set_default", json!({}));\n'
            "}\n"
        ),
    )
    findings = wc.check_panel_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert "models.set_default" in findings[0].message


def test_rule38_flags_declared_service_whose_registry_loads_empty(
    isolated_tree: Any,
) -> None:
    """A service listed in ``RUST_SERVICE_REGISTRIES`` whose registry
    loads empty is a broken gate, not a clean service (#116).

    This test previously asserted the opposite — that a call to
    ``wylde-ollama`` with no ollama registry present was silently
    skipped.  That skip was the bug in miniature: ``wylde-ollama`` IS
    declared in ``RUST_SERVICE_REGISTRIES``, so the engine believes it
    knows that service's verb surface.  Finding none means the crate was
    restructured out from under the rule, and every panel call to it is
    unchecked.  Reporting a pass there is reporting on work not done.

    The genuine skip — a service with no declared registry at all — is
    covered by ``test_rule38_skips_service_without_discoverable_registry``.
    """
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_OLLAMA: &str = "wylde-ollama";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::stream_call(SVC_OLLAMA, "ollama.pull", json!({}));\n'
            "}\n"
        ),
    )
    findings = wc.check_panel_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert findings[0].severity == "error"
    assert "wylde-ollama" in findings[0].message
    assert "loaded empty" in findings[0].message


def test_rule38_skips_dynamic_action_arg(isolated_tree: Any) -> None:
    """When the action key is built from a parameter rather than a literal,
    the rule has nothing to check — it should silently pass."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn run(name: &str) {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", Some(json!({\n'
            '        "action": name,\n'
            '        "payload": {},\n'
            "    })));\n"
            "}\n"
        ),
    )
    assert wc.check_panel_verbs_exist_in_harness_registry() == []


def test_rule38_python_registry_no_longer_accepted(isolated_tree: Any) -> None:
    """A verb backed only by the old Python registry must now FIRE (#116).

    This replaces ``test_rule38_accepts_python_only_verb``, whose
    docstring read "Python is part of the live surface".  It was true
    when written and is not any more: the Rust cutover deleted
    ``Core/harness/pipe/`` entirely.  A panel calling a verb that only
    ever existed there reaches a service that does not serve it, which
    is a runtime ``no_action`` — precisely what this rule guards.
    """
    wc, root = isolated_tree
    # Rust registry present but WITHOUT the verb: the post-cutover world.
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            "    let _ = wylde_gui_pipe::call(\n"
            "        SVC_HARNESS,\n"
            '        "POST",\n'
            '        "/__action__",\n'
            '        Some(json!({ "action": "conversations.new", "payload": {} })),\n'
            "    );\n"
            "}\n"
        ),
    )
    findings = wc.check_panel_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert findings[0].severity == "error"
    assert "conversations.new" in findings[0].message


# ── Rule 38 tightening: non-harness service registries ───────────────


def _seed_service_registry(root: Any, crate_name: str, verbs: list[str]) -> None:
    """Drop a synthetic service crate exposing ``const ALL_ACTIONS: [&str; N] = [...]``."""
    body = ",\n".join(f'    "{v}"' for v in verbs)
    src = (
        f"const ALL_ACTIONS: [&str; {len(verbs)}] = [\n"
        f"{body}\n"
        "];\n"
    )
    _write(
        root / "rust" / "crates" / crate_name / "src" / "service.rs",
        src,
    )


def test_rule38_clean_when_verb_in_extension_bridge(isolated_tree: Any) -> None:
    """Verbs from non-harness service registries are now indexed too."""
    wc, root = isolated_tree
    # The harness registry is mandatory for the rule to run at all (#116),
    # so seed it even though this test is about the bridge.
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_service_registry(root, "wylde-extension-bridge", ["ext.list", "ext.tools.call"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_EXT: &str = "wylde-extension-bridge";\n'
            "fn _x() {\n"
            "    let _ = wylde_gui_pipe::call(\n"
            "        SVC_EXT,\n"
            '        "POST",\n'
            '        "/__action__",\n'
            '        Some(json!({ "action": "ext.list", "payload": {} })),\n'
            "    );\n"
            "}\n"
        ),
    )
    assert wc.check_panel_verbs_exist_in_harness_registry() == []


def test_rule38_flags_unknown_ollama_verb(isolated_tree: Any) -> None:
    """A verb absent from a *populated* service registry is a real
    mismatch — distinct from the empty-registry case above, which is a
    broken gate rather than a bad call."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_service_registry(root, "wylde-ollama", ["ollama.pull"])
    _seed_panel(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_OLLAMA: &str = "wylde-ollama";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::stream_call(SVC_OLLAMA, "ollama.ghost", json!({}));\n'
            "}\n"
        ),
    )
    findings = wc.check_panel_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert "ollama.ghost" in findings[0].message
    assert "wylde-ollama" in findings[0].message


def test_rule38_skips_service_without_discoverable_registry(isolated_tree: Any) -> None:
    """``wylde-vpn`` is absent from ``RUST_SERVICE_REGISTRIES`` entirely —
    calls to it remain out of scope (no false flags).

    The distinction that matters post-#116: *undeclared* is a genuine
    skip, because the engine never claimed to know this service's verbs.
    *Declared but empty* is a broken gate and errors — see
    ``test_rule38_flags_declared_service_whose_registry_loads_empty``.
    """
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["chat.start_turn"])
    _seed_panel(
        root,
        "RemoteAccess",
        ipc_body=(
            'pub const SVC_VPN: &str = "wylde-vpn";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_VPN, "GET", "/api/link/status", None);\n'
            "}\n"
        ),
    )
    # No registry for wylde-vpn → rule skips, no findings.
    assert wc.check_panel_verbs_exist_in_harness_registry() == []


# ── Rule 39 tightening: const propagation ────────────────────────────


def test_rule39_clean_when_target_via_const(isolated_tree: Any) -> None:
    """``request_nav(NAV_TARGET)`` where NAV_TARGET is a file-local
    const string must be checked the same as a literal."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        'pub const NAV_TARGET: &str = "core/foo";\n'
        "fn _x() {\n"
        "    let _ = wylde_gui_pipe::request_nav(NAV_TARGET);\n"
        "}\n",
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_flags_const_with_unknown_value(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        'const NAV_TARGET: &str = "core/ghost";\n'
        "fn _x() {\n"
        "    let _ = wylde_gui_pipe::request_nav(NAV_TARGET);\n"
        "}\n",
    )
    findings = wc.check_nav_targets_exist()
    assert len(findings) == 1
    assert "core/ghost" in findings[0].message


def test_rule39_skips_non_const_ident(isolated_tree: Any) -> None:
    """If IDENT isn't a const-bound string, fall back to skip — runtime-
    built keys aren't statically checkable."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "fn _x(nav_key: &str) {\n"
        "    let _ = wylde_gui_pipe::request_nav(nav_key);\n"
        "}\n",
    )
    assert wc.check_nav_targets_exist() == []


# ── Rule 40 tightening: over-declaration warning ─────────────────────


def test_rule40_flags_over_declaration_as_warning(isolated_tree: Any) -> None:
    """``required_services`` lists ``wylde-foo`` but the panel never
    calls it — warning severity."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=["wylde-harness", "wylde-vpn"],
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None);\n'
            "}\n"
        ),
    )
    findings = wc.check_required_services_includes_called_services()
    assert len(findings) == 1
    assert findings[0].severity == "warning"
    assert "wylde-vpn" in findings[0].message
    assert "doesn't call" in findings[0].message


def test_rule40_emits_both_directions(isolated_tree: Any) -> None:
    """Same panel can be both under-declared (ERROR) and over-declared
    (WARNING) at once."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        # Lists vpn (unused) but missing ollama (called).
        required_services=["wylde-harness", "wylde-vpn"],
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            'pub const SVC_OLLAMA: &str = "wylde-ollama";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None);\n'
            '    let _ = wylde_gui_pipe::stream_call(SVC_OLLAMA, "ollama.pull", json!({}));\n'
            "}\n"
        ),
    )
    findings = wc.check_required_services_includes_called_services()
    severities = {f.severity for f in findings}
    assert "error" in severities
    assert "warning" in severities
    msgs = " ".join(f.message for f in findings)
    assert "wylde-ollama" in msgs
    assert "wylde-vpn" in msgs


# ── Rule 39: nav_targets_exist ───────────────────────────────────────


def test_rule39_clean_when_target_is_registered(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        'fn _x() { let _ = wylde_gui_pipe::request_nav("core/foo"); }\n',
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_flags_unknown_target(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "panel.rs",
        'fn _x() { let _ = wylde_gui_pipe::request_nav("core/ghost"); }\n',
    )
    findings = wc.check_nav_targets_exist()
    assert len(findings) == 1
    assert findings[0].rule == "nav_targets_exist"
    assert "core/ghost" in findings[0].message
    assert findings[0].severity == "error"


def test_rule39_skips_variable_arg(isolated_tree: Any) -> None:
    """Runtime-built keys aren't statically resolvable — only string
    literals are subject to the rule."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "fn _x(k: &str) { let _ = wylde_gui_pipe::request_nav(k); }\n",
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_ignores_legacy_tauri_tree(isolated_tree: Any) -> None:
    """The legacy Svelte/Tauri tree is out of scope for the gpui rules."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "src-tauri" / "src" / "lib.rs",
        'fn _x() { let _ = wylde_gui_pipe::request_nav("core/unknown"); }\n',
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_ignores_nav_bus_source(isolated_tree: Any) -> None:
    """The nav_bus source itself naturally references its own API in
    doc + tests; the rule should not chase it."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Frontend" / "Pipe" / "src" / "nav_bus.rs",
        '#[test] fn _t() { let _ = request_nav("core/ghost"); }\n',
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_ignores_module_doc_examples(isolated_tree: Any) -> None:
    """Module-level docs that show ``request_nav("core/<id>")`` as an
    example must not false-fire the rule."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "main.rs",
        '//! Panels fire `wylde_gui_pipe::request_nav("core/<id>")` to nav.\n'
        'fn _x() { let _ = wylde_gui_pipe::request_nav("core/foo"); }\n',
    )
    assert wc.check_nav_targets_exist() == []


def test_rule39_ignores_block_comment_example(isolated_tree: Any) -> None:
    """Block-comment examples are also stripped."""
    wc, root = isolated_tree
    _seed_panel(root, "Foo", panel_id="foo")
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "main.rs",
        '/* example: request_nav("core/ghost") */\n'
        'fn _x() { let _ = wylde_gui_pipe::request_nav("core/foo"); }\n',
    )
    assert wc.check_nav_targets_exist() == []


# ── Rule 40: required_services_includes_called_services ──────────────


def test_rule40_clean_when_manifest_lists_every_service(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=["wylde-harness", "wylde-ollama"],
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            'pub const SVC_OLLAMA: &str = "wylde-ollama";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None);\n'
            '    let _ = wylde_gui_pipe::stream_call(SVC_OLLAMA, "ollama.pull", json!({}));\n'
            "}\n"
        ),
    )
    assert wc.check_required_services_includes_called_services() == []


def test_rule40_flags_missing_service(isolated_tree: Any) -> None:
    """Calling ``wylde-vpn`` while only declaring ``wylde-harness`` now
    surfaces *both* an under-declared ERROR (for vpn, never declared)
    AND an over-declared WARNING (for harness, declared but uncalled)."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=["wylde-harness"],
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            'pub const SVC_VPN: &str = "wylde-vpn";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_VPN, "GET", "/api/link/status", None);\n'
            "}\n"
        ),
    )
    findings = wc.check_required_services_includes_called_services()
    by_sev = {f.severity for f in findings}
    assert by_sev == {"error", "warning"}
    errors = [f for f in findings if f.severity == "error"]
    warnings = [f for f in findings if f.severity == "warning"]
    assert len(errors) == 1
    assert len(warnings) == 1
    assert errors[0].rule == "required_services_includes_called_services"
    assert "wylde-vpn" in errors[0].message
    assert "wylde-harness" in warnings[0].message


def test_rule40_over_declaration_emits_warning(isolated_tree: Any) -> None:
    """Over-declaration without any under-declaration emits warnings
    (one per extra service) and no errors."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=["wylde-harness", "wylde-ollama", "wylde-gateway"],
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None);\n'
            "}\n"
        ),
    )
    findings = wc.check_required_services_includes_called_services()
    assert all(f.severity == "warning" for f in findings)
    assert len(findings) == 2  # wylde-ollama + wylde-gateway
    extras = sorted(
        s
        for f in findings
        for s in ("wylde-ollama", "wylde-gateway")
        if s in f.message
    )
    assert "wylde-ollama" in extras
    assert "wylde-gateway" in extras


def test_rule40_resolves_literal_service_arg(isolated_tree: Any) -> None:
    """Some helpers pass the service as an inline string literal rather
    than a constant — both paths should resolve."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=[],
        ipc_body=(
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call("wylde-harness", "POST", "/__action__", None);\n'
            "}\n"
        ),
    )
    findings = wc.check_required_services_includes_called_services()
    assert len(findings) == 1
    assert "wylde-harness" in findings[0].message


def test_rule40_honours_manifest_opt_out(isolated_tree: Any) -> None:
    """A manifest may declare ``wylde_check_opt_outs: [rule_name]`` to
    opt the panel out of this rule (used for documented soft-fail
    designs like the Dashboard panel)."""
    wc, root = isolated_tree
    base = root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo"
    manifest = {
        "schema_version": 2,
        "service": "core",
        "wylde_check_opt_outs": ["required_services_includes_called_services"],
        "panels": [
            {
                "id": "foo",
                "title": "Foo",
                "required_services": [],
                "source": {"kind": "gpui_view", "factory": "wylde_panel_foo::Panel::view"},
            }
        ],
    }
    _write(base / "manifest.json", json.dumps(manifest))
    _write(
        base / "src" / "ipc.rs",
        (
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None);\n'
            "}\n"
        ),
    )
    assert wc.check_required_services_includes_called_services() == []


def test_rule40_skips_unresolvable_service_token(isolated_tree: Any) -> None:
    """A parameter-passed service can't be resolved — skip it silently
    so we don't false-flag."""
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Foo",
        required_services=[],
        ipc_body=(
            "async fn helper(svc: &str) {\n"
            '    let _ = wylde_gui_pipe::call(svc, "POST", "/__action__", None);\n'
            "}\n"
        ),
    )
    assert wc.check_required_services_includes_called_services() == []
