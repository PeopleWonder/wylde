"""Tests for rule 59 (``gui_controls_are_wired_and_walkable``) — mirrors
prod-side ``wylde_check/rules/_control_functionality.py``.

The failure the rule exists to catch is a GUI control that ships doing
nothing: an empty handler, or a control that bypasses
``wylde_gui_controls::control()`` and so is never enumerated by
``tests/control_walk.rs`` (#247).

Both enforcement halves are exercised here — dead handler bodies and
not-routed-through-the-constructor — plus the empty-scan guards, because a
rule that inspects nothing reports a pass rather than going red
(#101/#114/#116).

The dead-body half gets the most cases on purpose. During development it
silently found nothing on a deliberately-emptied handler, because an empty
closure body and an unparsable one were both falsy and got the same
``continue``. The real tree's zero dead handlers looked like a clean tree
and was actually a broken detector. These tests pin the distinction.
"""

from __future__ import annotations

from typing import Any

import pytest

from .conftest import _import_check, _write

#: The shipped grandfather table, captured at import time — i.e. before the
#: autouse fixture below empties it for each test. Reading the module
#: attribute inside a test would see the emptied copy, since monkeypatch
#: rebinds the attribute on the one real module object.
REAL_GRANDFATHERED = dict(
    _import_check().rules._control_functionality.GRANDFATHERED_UNROUTED
)

#: The real checkout root, captured before `isolated_tree` repoints
#: `WYLDE_ROOT` at a `tmp_path`. Tests that assert against the SHIPPED tree
#: (rather than a synthetic one) need this.
REAL_ROOT = _import_check().WYLDE_ROOT

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


@pytest.fixture(autouse=True)
def _no_grandfathered(isolated_tree: Any, monkeypatch: pytest.MonkeyPatch) -> None:
    """Empty the grandfather ratchet for the synthetic tree.

    ``GRANDFATHERED_UNROUTED`` records the 140 real sites that existed at the
    #247 pilot. None of those files exist under ``tmp_path``, so leaving the
    table populated would make every test see 28 "budget is stale" findings
    about files it never wrote. Clearing it states the intent directly: a
    synthetic tree is grandfathered for nothing, so these tests exercise the
    rule's judgement rather than the ratchet's bookkeeping. The ratchet gets
    its own tests below, with an explicit table.
    """
    wc, _root = isolated_tree
    monkeypatch.setattr(_rule_module(wc), "GRANDFATHERED_UNROUTED", {})


def _findings(wc: Any) -> list:
    return wc.check_gui_controls_are_wired_and_walkable()


def _messages(wc: Any) -> str:
    return " ".join(f.message for f in _findings(wc))


# ── The passing shape ────────────────────────────────────────────────


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


# ── Half 1: dead handler bodies ──────────────────────────────────────


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
    _panel(root, _wired(handler_body="this.expanded = !this.expanded;\n                cx.notify();"))
    assert _findings(wc) == []


# ── Half 2: bypassing the constructor ────────────────────────────────


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


# ── Severity and the grandfather ratchet ─────────────────────────────


def test_findings_are_errors(isolated_tree: Any) -> None:
    """Error, not warning.

    A warning is not advisory in this repo: the `wylde_check (full rule set)`
    CI job fails on ANY finding. Shipping at WARNING to be gentle on an
    unmigrated tree would red `develop` exactly as hard, just less legibly —
    which is why the unmigrated sites are handled by the ratchet below
    instead.
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


def test_ratchet_passes_a_file_exactly_at_its_budget(isolated_tree: Any) -> None:
    """The 140 sites that existed at the pilot must not red the build — the
    whole point of grandfathering rather than blocking on the migration."""
    wc, root = isolated_tree
    _unrouted_panel(root, 3)
    _rule_module(wc).GRANDFATHERED_UNROUTED[PANEL_REL] = 3
    assert _findings(wc) == []


def test_ratchet_flags_a_new_control_in_a_grandfathered_file(
    isolated_tree: Any,
) -> None:
    """The case #247 exists for, and the reason the budget is per-file rather
    than a blanket file exemption: a NEW dead button in an already-dirty file
    would otherwise ship unnoticed."""
    wc, root = isolated_tree
    _unrouted_panel(root, 4)
    _rule_module(wc).GRANDFATHERED_UNROUTED[PANEL_REL] = 3
    found = _findings(wc)
    assert len(found) == 1
    assert "1 new interactive control" in found[0].message
    assert found[0].severity == "error"


def test_ratchet_flags_a_stale_budget_after_migration(isolated_tree: Any) -> None:
    """Migration progress must tighten the ratchet.

    An allowlist nobody is required to lower rusts open: it would keep
    accepting N sites long after the file has none, and a regression back up
    to N would pass. Under-budget is a one-line edit, so making it red is
    cheap and keeps the number honest.
    """
    wc, root = isolated_tree
    _unrouted_panel(root, 1)
    _rule_module(wc).GRANDFATHERED_UNROUTED[PANEL_REL] = 3
    found = _findings(wc)
    assert len(found) == 1
    assert "stale" in found[0].message and "Lower the entry to 1" in found[0].message


def test_ratchet_tells_you_to_delete_a_fully_migrated_entry(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _panel(root, _wired())  # fully routed through control()
    _rule_module(wc).GRANDFATHERED_UNROUTED[PANEL_REL] = 2
    found = _findings(wc)
    assert len(found) == 1
    assert "delete it" in found[0].message


def test_the_real_grandfather_table_is_drained() -> None:
    """The migration is complete: nothing is grandfathered.

    #247 part 2 drained `GRANDFATHERED_UNROUTED` batch by batch; batch 8 (the
    Shell) took it to empty. Every interactive site in the GUI is now routed
    through `control()`, so a non-empty table would mean a regression — a file
    re-added to the exempt list, or the drain quietly reverted.

    When the endgame lands (delete the ratchet mechanism + require a
    control_walk per panel), this test goes with the dict it guards.

    (While the table was draining this asserted instead that every budgeted
    path still existed on disk — a stale entry granting a budget to a
    deleted/renamed file is the #101/#116 shape. With the table empty there is
    nothing left to go stale, so the invariant becomes simply: stays empty.)
    """
    assert REAL_GRANDFATHERED == {}, (
        "GRANDFATHERED_UNROUTED is no longer empty — the routing migration was "
        f"complete, so this is a regression: {REAL_GRANDFATHERED}. A new control "
        "must use `control()` from the start, not be grandfathered back in."
    )
