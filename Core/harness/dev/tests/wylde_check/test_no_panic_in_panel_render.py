"""Tests for rule 51 (``no_panic_in_panel_render``) — mirrors prod-side
``wylde_check/rules/_no_panic_in_panel_render.py``.

Two ``snapshot.gpus.first().unwrap()`` calls in the Dashboard panel's
render path took down the whole gpui shell on cold start (the VRAM broker
inventory hadn't landed, so ``gpus`` was empty and ``.unwrap()`` panicked
with exit code 101). Panels share the one event loop, so any panic
primitive in panel code is a latent shell-killer — exactly what this rule
catches.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

PANEL = ("Core", "GUI", "Frontend", "Panels", "Dashboard", "src")


def _panel(root: Any, name: str, body: str) -> None:
    path = root
    for part in PANEL:
        path = path / part
    _write(path / name, body)


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_unwrap_or(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "ok.rs",
        "fn render(&self) -> SharedString {\n"
        '    let gpu = self.snapshot.gpus.first().map(|g| g.name.clone())\n'
        '        .unwrap_or("—".into());\n'
        "    gpu\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_unwrap_or_else(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "ok2.rs",
        "fn name(&self) -> String {\n"
        "    self.gpus.first().map(|g| g.name.clone())\n"
        "        .unwrap_or_else(|| default_name())\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_if_let_some(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "iflet.rs",
        "fn render(&self) {\n"
        "    if let Some(gpu) = self.snapshot.gpus.first() {\n"
        "        draw(gpu);\n"
        "    }\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_match(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "match.rs",
        "fn render(&self) {\n"
        "    match self.snapshot.gpus.first() {\n"
        "        Some(g) => draw(g),\n"
        "        None => draw_placeholder(),\n"
        "    }\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_question_mark(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "qmark.rs",
        "fn first_gpu(&self) -> Option<&Gpu> {\n"
        "    let g = self.snapshot.gpus.first()?;\n"
        "    Some(g)\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_inside_cfg_test(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "cfgtest.rs",
        "fn prod(&self) { let _ = self.gpus.first().map(|g| g.id); }\n"
        "\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    #[test]\n"
        "    fn t() {\n"
        "        let g = vec![1].first().unwrap();\n"
        '        let h = some().expect("must be set");\n'
        "        panic!(\"boom\");\n"
        "    }\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_const_item(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "constitem.rs",
        "const MAX: usize = [1usize, 2, 3].first().copied().unwrap();\n"
        "fn render(&self) { draw(MAX); }\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_valid_opt_out(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "optout.rs",
        "fn render(&self) {\n"
        "    // INVARIANT: rebuilt every frame from a non-empty source\n"
        "    // wylde-check: panel-panic-allowed\n"
        "    let g = self.always_present.first().unwrap();\n"
        "    draw(g);\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_valid_opt_out_same_line(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "optout2.rs",
        "fn render(&self) {\n"
        "    // SAFETY: invariant established by the type's constructor\n"
        "    let g = self.always.first().unwrap(); "
        "// wylde-check: panel-panic-allowed\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_outside_panels_not_scanned(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Bare unwrap, but in Shell (not Panels/<x>/src) — must NOT be scanned.
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "fn f() { let g = v.first().unwrap(); panic!(\"x\"); }\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_comment_only(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "comments.rs",
        "fn render(&self) {\n"
        "    // self.gpus.first().unwrap() would panic on cold start\n"
        "    /// historically used .expect() per the old design\n"
        "    let g = self.gpus.first().map(|g| g.id).unwrap_or(0);\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


def test_pass_string_literal_mention(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "strings.rs",
        "fn render(&self) {\n"
        '    let msg = "the broker may unwrap to None and panic!";\n'
        '    let raw = r#"do not todo! or unreachable! here"#;\n'
        "    draw(msg);\n"
        "    draw(raw);\n"
        "}\n",
    )
    assert wc.check_no_panic_in_panel_render() == []


# ── FAIL cases (findings expected) ───────────────────────────────────


def test_fail_unwrap_in_render(isolated_tree: Any) -> None:
    # Mirrors the Dashboard cold-start crash.
    wc, root = isolated_tree
    _panel(
        root,
        "dash.rs",
        "fn render(&self) -> SharedString {\n"
        "    let gpu = self.snapshot.gpus.first().unwrap();\n"
        "    gpu.name.clone().into()\n"
        "}\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 1
    assert found[0].rule == "no_panic_in_panel_render"
    assert found[0].severity == "error"
    assert found[0].line == 2


def test_fail_expect_in_handler(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "handler.rs",
        "fn on_click(&mut self) {\n"
        '    let cfg = self.cfg.as_ref().expect("config must be loaded");\n'
        "    apply(cfg);\n"
        "}\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 1
    assert found[0].line == 2


def test_fail_panic_macro(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "boom.rs",
        "fn render(&self) {\n"
        "    if self.broken {\n"
        '        panic!("dashboard is in a broken state");\n'
        "    }\n"
        "}\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 1
    assert found[0].line == 3


def test_fail_unreachable_macro(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "unreach.rs",
        "fn render(&self, kind: Kind) {\n"
        "    match kind {\n"
        "        Kind::A => draw_a(),\n"
        "        _ => unreachable!(),\n"
        "    }\n"
        "}\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 1
    assert found[0].line == 4


def test_fail_todo_and_unimplemented(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "stubs.rs",
        "fn render(&self) { todo!() }\n"
        "fn handle(&self) { unimplemented!() }\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 2
    assert {f.line for f in found} == {1, 2}


def test_fail_opt_out_without_justification(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "lazyoptout.rs",
        "fn render(&self) {\n"
        "    let g = self.gpus.first().unwrap(); "
        "// wylde-check: panel-panic-allowed\n"
        "}\n",
    )
    found = wc.check_no_panic_in_panel_render()
    assert len(found) == 1
    assert found[0].line == 2
    assert "justification" in found[0].message.lower()


# ── dispatcher wiring ────────────────────────────────────────────────


def test_rule51_registered_in_dispatcher(isolated_tree: Any) -> None:
    wc, _ = isolated_tree
    result = wc.run_all(only=["no_panic_in_panel_render"])
    assert result["ok"] is True
    assert "no_panic_in_panel_render" in result["data"]["summary"]["by_rule"]
