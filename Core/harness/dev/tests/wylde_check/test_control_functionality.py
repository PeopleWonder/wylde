"""Tests for the two #247 control rules, mirroring prod-side
``wylde_check/rules/_control_functionality.py``:

* **rule 59** (``gui_controls_are_wired_and_walkable``) — the static half: a
  dead handler body, or an interactive site that bypasses
  ``wylde_gui_controls::control()`` and so is never enumerated by
  ``tests/control_walk.rs``.
* **rule 61** (``every_control_building_crate_is_walked``) — the companion: a
  GUI crate that builds controls must have a ``control_walk`` that declares
  every one of its control-building sources.

The grandfather ratchet rule 59 shipped with was drained to empty and then
removed in the #247 endgame (the routing migration is complete and every panel
is now walked), so there is no per-file budget any more: any bypass is a
finding, full stop. These tests pin that, plus the empty-scan guards — a rule
that inspects nothing must go red, not report a pass (#101/#114/#116).

The dead-body half gets the most cases on purpose. During development it
silently found nothing on a deliberately-emptied handler, because an empty
closure body and an unparsable one were both falsy and got the same
``continue``. These tests pin the distinction.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


PANEL_SRC = ("Core", "GUI", "Frontend", "Panels", "Tools", "src")
PANEL_REL = "Core/GUI/Frontend/Panels/Tools/src/tools_panel.rs"


def _panel(root: Any, body: str) -> None:
    path = root
    for part in PANEL_SRC:
        path = path / part
    _write(path / "tools_panel.rs", body)


def _wired(handler_body: str = "ToolsPanel::spawn_refresh(cx);") -> str:
    """A control routed through the constructor with a live handler."""
    return (
        "fn refresh_button(cx: &mut Context<ToolsPanel>) -> Stateful<Div> {\n"
        '    control(div(), "tools-refresh")\n'
        "        .cursor_pointer()\n"
        "        .on_mouse_down(\n"
        "            MouseButton::Left,\n"
        "            cx.listener(|_this: &mut ToolsPanel, _ev, _w, cx| {\n"
        f"                {handler_body}\n"
        "            }),\n"
        "        )\n"
        "}\n"
    )


def _rule_module(wc: Any) -> Any:
    """The rule's own module, reached through the package rather than through
    ``sys.modules``: the suite is importable as either ``Core.harness…`` or
    ``Wylde.Core.harness…`` (see ``conftest``), so a hard-coded module key is
    right in one layout and a ``KeyError`` in the other."""
    return wc.rules._control_functionality


def _findings(wc: Any) -> list:
    return wc.check_gui_controls_are_wired_and_walkable()


def _messages(wc: Any) -> str:
    return " ".join(f.message for f in _findings(wc))


# ── Rule 59, the passing shape ───────────────────────────────────────


def test_pass_control_routed_through_constructor_with_a_live_handler(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _panel(root, _wired())
    assert _findings(wc) == []


def test_pass_a_function_with_no_handler_is_not_a_control_site(
    isolated_tree: Any,
) -> None:
    """A bare ``.id()`` on a scroll container is not an interactive control.

    Without this the rule would fire on every ``uniform_list`` handle in the
    tree and be turned off within a week.
    """
    wc, root = isolated_tree
    _panel(
        root,
        _wired()
        + "fn scroll_area() -> Stateful<Div> {\n"
        '    div().id("log-scroll").overflow_y_scroll()\n'
        "}\n",
    )
    assert _findings(wc) == []


def test_pass_handler_mentioned_only_in_a_comment(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        _wired()
        + "fn notes() {\n"
        "    // The old version called .on_mouse_down(..) with div().id(x) here.\n"
        "    /* .on_click( ... ) with a bare .id( ... ) too. */\n"
        "}\n",
    )
    assert _findings(wc) == []


# ── Rule 59, half 1: dead handler bodies ─────────────────────────────


def test_flags_an_empty_handler_body(isolated_tree: Any) -> None:
    """The regression that motivated these tests: an empty body must not be
    confused with a body the parser could not read."""
    wc, root = isolated_tree
    _panel(root, _wired(handler_body=""))
    assert "does nothing" in _messages(wc)


def test_flags_a_handler_that_only_notifies(isolated_tree: Any) -> None:
    """`cx.notify()` alone repaints state the handler never changed — the
    classic looks-wired-is-dead button."""
    wc, root = isolated_tree
    _panel(root, _wired(handler_body="cx.notify();"))
    assert "does nothing" in _messages(wc)


def test_flags_a_todo_handler(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(root, _wired(handler_body="todo!();"))
    assert "does nothing" in _messages(wc)


def test_pass_a_handler_that_does_real_work(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(root, _wired(handler_body="this.spawn_toggle(name.clone(), true, cx);"))
    assert _findings(wc) == []


def test_pass_a_handler_that_notifies_after_doing_work(isolated_tree: Any) -> None:
    """`cx.notify()` is only damning when it is the *whole* body."""
    wc, root = isolated_tree
    _panel(
        root,
        _wired(handler_body="this.expanded = !this.expanded;\n                cx.notify();"),
    )
    assert _findings(wc) == []


# ── Rule 59, half 2: bypassing the constructor (no budget) ───────────


def test_flags_an_interactive_site_with_a_bare_id(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "fn refresh_button(cx: &mut Context<ToolsPanel>) -> Stateful<Div> {\n"
        '    div().id("tools-refresh")\n'
        "        .on_mouse_down(\n"
        "            MouseButton::Left,\n"
        "            cx.listener(|_this: &mut ToolsPanel, _ev, _w, cx| {\n"
        "                ToolsPanel::spawn_refresh(cx);\n"
        "            }),\n"
        "        )\n"
        "}\n",
    )
    assert "does not route through the constructor" in _messages(wc)


def test_flags_a_partially_migrated_function(isolated_tree: Any) -> None:
    """One control migrated and one not must still flag — otherwise a
    half-done migration reads as done."""
    wc, root = isolated_tree
    _panel(
        root,
        "fn row(cx: &mut Context<ToolsPanel>) -> Div {\n"
        '    let a = control(div(), "a").on_mouse_down(\n'
        "        MouseButton::Left,\n"
        "        cx.listener(|_t: &mut ToolsPanel, _e, _w, cx| { ToolsPanel::spawn_refresh(cx); }),\n"
        "    );\n"
        '    let b = div().id("b").on_mouse_down(\n'
        "        MouseButton::Left,\n"
        "        cx.listener(|_t: &mut ToolsPanel, _e, _w, cx| { ToolsPanel::spawn_refresh(cx); }),\n"
        "    );\n"
        "    div().child(a).child(b)\n"
        "}\n",
    )
    assert "does not route through the constructor" in _messages(wc)


def test_pass_opt_out_marker_suppresses_a_non_interactive_id(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "fn list(cx: &mut Context<ToolsPanel>) -> Div {\n"
        "    // wylde-check: control-ok: the scroll handle is not a clickable control.\n"
        '    let scroller = div().id("scroll");\n'
        '    let btn = control(div(), "b").on_mouse_down(\n'
        "        MouseButton::Left,\n"
        "        cx.listener(|_t: &mut ToolsPanel, _e, _w, cx| { ToolsPanel::spawn_refresh(cx); }),\n"
        "    );\n"
        "    scroller.child(btn)\n"
        "}\n",
    )
    assert _findings(wc) == []


def test_findings_are_errors(isolated_tree: Any) -> None:
    """Error, not warning.

    A warning is not advisory in this repo: the `wylde_check (full rule set)`
    CI job fails on ANY finding. Shipping at WARNING to be gentle on an
    unmigrated tree would red `develop` exactly as hard, just less legibly.
    """
    wc, root = isolated_tree
    _panel(root, _wired(handler_body=""))
    found = _findings(wc)
    assert found and all(f.severity == "error" for f in found)


def _unrouted_panel(root: Any, n: int) -> None:
    """A panel with `n` interactive controls, none routed through control()."""
    lines = ["fn row(cx: &mut Context<ToolsPanel>) -> Div {", "    div()"]
    for i in range(n):
        lines += [
            f'        .child(div().id("c{i}").on_mouse_down(',
            "            MouseButton::Left,",
            "            cx.listener(|_t: &mut ToolsPanel, _e, _w, cx| "
            "{ ToolsPanel::spawn_refresh(cx); }),",
            "        ))",
        ]
    lines.append("}")
    _panel(root, "\n".join(lines) + "\n")


def test_every_unrouted_site_is_flagged_with_no_budget(isolated_tree: Any) -> None:
    """The ratchet is gone: N unrouted controls in a file are N findings.

    While the grandfather budget existed, a file could carry its pilot-era
    sites without failing; now every bypass is reported on the PR that adds it,
    with the per-site ``control-ok`` marker as the only escape hatch.
    """
    wc, root = isolated_tree
    _unrouted_panel(root, 3)
    found = _findings(wc)
    assert len(found) == 3
    assert all("does not route through the constructor" in f.message for f in found)
    assert all(f.severity == "error" for f in found)


def test_the_grandfather_ratchet_mechanism_is_gone(isolated_tree: Any) -> None:
    """The #247 endgame deleted the ratchet. The module must not carry
    ``GRANDFATHERED_UNROUTED`` any more, so a per-file budget cannot be
    re-introduced (a control silently grandfathered back in) without this
    test — and rule 61 — noticing.
    """
    wc, _root = isolated_tree
    assert not hasattr(_rule_module(wc), "GRANDFATHERED_UNROUTED")


# ── Rule 61: every control-building crate is control-walked ───────────

R61_CRATE = ("Core", "GUI", "Frontend", "Panels", "Widget")
R61_CRATE_REL = "Core/GUI/Frontend/Panels/Widget"


def _r61(wc: Any) -> list:
    return wc.check_every_control_building_crate_is_walked()


def _make_crate(root: Any, *, src: dict, extra_files: dict = None) -> None:
    """A synthetic GUI crate at ``Panels/Widget``: a ``Cargo.toml``, the given
    ``src/`` files (rel path -> content), and any ``extra_files`` (crate-rel
    path -> content, e.g. a ``tests/control_walk.rs``)."""
    base = root
    for part in R61_CRATE:
        base = base / part
    _write(base / "Cargo.toml", '[package]\nname = "wylde-panel-widget"\n')
    for rel, content in src.items():
        _write(base / "src" / rel, content)
    for rel, content in (extra_files or {}).items():
        _write(base / rel, content)


_A_CONTROL = 'fn button() -> Div { control(div(), "widget-go") }\n'


def _walk_declaring(*rel_sources: str) -> str:
    """A minimal control-walk that declares the given crate-relative sources."""
    includes = "".join(f'        include_str!("{s}"),\n' for s in rel_sources)
    return (
        "#[gpui::test]\n"
        "fn every_widget_control_does_something(cx: &mut TestAppContext) {\n"
        "    ControlWalk::new(window, &fake)\n"
        "        .sources(&[\n" + includes + "        ])\n"
        "        .run(cx)\n"
        "        .assert_every_control_lives();\n"
        "}\n"
    )


def test_r61_pass_crate_walk_declares_its_control_source(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _make_crate(
        root,
        src={"panel.rs": _A_CONTROL},
        extra_files={"tests/control_walk.rs": _walk_declaring("../src/panel.rs")},
    )
    assert _r61(wc) == []


def test_r61_flags_a_control_building_crate_with_no_walk(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _make_crate(root, src={"panel.rs": _A_CONTROL})
    found = _r61(wc)
    assert len(found) == 1
    assert "has no control_walk" in found[0].message
    assert found[0].file == R61_CRATE_REL
    assert found[0].severity == "error"


def test_r61_flags_a_control_source_the_walk_omits(isolated_tree: Any) -> None:
    """A walk that declares one control file but not a sibling that also builds
    controls leaves that sibling's ids outside the coverage assertion."""
    wc, root = isolated_tree
    _make_crate(
        root,
        src={"panel.rs": _A_CONTROL, "extra.rs": _A_CONTROL},
        extra_files={"tests/control_walk.rs": _walk_declaring("../src/panel.rs")},
    )
    found = _r61(wc)
    assert len(found) == 1
    assert "no control_walk in the crate declares it" in found[0].message
    assert found[0].file.endswith("src/extra.rs")


def test_r61_pass_crate_that_builds_no_controls(isolated_tree: Any) -> None:
    """A crate whose src builds no ``control()`` (e.g. a focus-surface widget
    routed through ``.id()`` + ``control-ok``) needs no walk."""
    wc, root = isolated_tree
    _make_crate(root, src={"panel.rs": "fn root() -> Div { div().id(\"x\") }\n"})
    assert _r61(wc) == []


def test_r61_a_cfg_test_control_is_not_a_shipped_control(isolated_tree: Any) -> None:
    """A ``control()`` inside a ``#[cfg(test)]`` block is a walk fixture, not a
    shipped control, so it neither requires nor satisfies a walk requirement."""
    wc, root = isolated_tree
    _make_crate(
        root,
        src={
            "panel.rs": (
                "fn root() -> Div { div() }\n"
                "#[cfg(test)]\n"
                "mod control_walk {\n"
                '    fn fixture() { control(div(), "only-in-test"); }\n'
                "}\n"
            )
        },
    )
    assert _r61(wc) == []


def test_r61_in_crate_walk_satisfies_the_requirement(isolated_tree: Any) -> None:
    """The walk can live in-crate (a ``#[cfg(test)] mod`` in the same src file),
    declaring the file via a relative ``include_str!``."""
    wc, root = isolated_tree
    _make_crate(
        root,
        src={
            "panel.rs": (
                _A_CONTROL
                + "#[cfg(test)]\n"
                "mod control_walk {\n"
                "    #[gpui::test]\n"
                "    fn every_widget_control_does_something(cx: &mut TestAppContext) {\n"
                "        ControlWalk::new(window, &fake)\n"
                '            .sources(&[include_str!("panel.rs")])\n'
                "            .run(cx)\n"
                "            .assert_every_control_lives();\n"
                "    }\n"
                "}\n"
            )
        },
    )
    assert _r61(wc) == []


def test_r61_empty_scan_guard_no_gui_crates_is_a_finding(isolated_tree: Any) -> None:
    """A rule that inspects nothing must go red, not pass (#114/#116)."""
    wc, _root = isolated_tree  # tmp_path has no GUI crates at all
    found = _r61(wc)
    assert len(found) == 1
    assert "no GUI crates" in found[0].message
