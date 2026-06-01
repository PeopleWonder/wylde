"""Tests for rule 50 (``no_bare_tokio_in_panel_src``) — mirrors prod-side
``wylde_check/rules/_no_bare_tokio.py``.

A bare tokio primitive reached from a gpui panel panics at startup
("no reactor running") because gpui has no tokio runtime — exactly the
chat_panel.rs:544 consent-reconnect crash this rule was added to catch.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

PANEL = ("Core", "GUI", "Frontend", "Panels", "Chat", "src")


def _panel(root: Any, name: str, body: str) -> None:
    path = root
    for part in PANEL:
        path = path / part
    _write(path / name, body)


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_cx_spawn(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "ok.rs",
        "fn refresh(&self, cx: &mut Context<Self>) {\n"
        "    cx.spawn(async move { do_work().await; });\n"
        "    cx.background_executor().timer(Duration::from_secs(1));\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_bare_tokio_inside_cfg_test(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "cfgtest.rs",
        "fn prod() { cx.spawn(async {}); }\n"
        "\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    #[test]\n"
        "    fn t() {\n"
        "        tokio::time::sleep(Duration::from_secs(1));\n"
        "        tokio::spawn(async {});\n"
        "    }\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_opt_out_marker_line_above(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "optout.rs",
        "fn f() {\n"
        "    // wylde-check: tokio-runtime-provided\n"
        "    tokio::spawn(async {});\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_opt_out_marker_same_line(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "optout2.rs",
        "fn f() {\n"
        "    tokio::time::sleep(d); // wylde-check: tokio-runtime-provided\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_outside_panels_not_scanned(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # Bare tokio, but in Shell (not Panels/<x>/src) — must NOT be scanned.
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "fn f() { tokio::time::sleep(d); tokio::spawn(async {}); }\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_import_only(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "imports.rs",
        "use tokio::time::sleep;\n"
        "use tokio::task::spawn_blocking;\n"
        "fn f() { cx.spawn(async {}); }\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_comment_only(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "comments.rs",
        "fn f() {\n"
        "    // tokio::spawn(async {}) would panic here\n"
        "    /// uses tokio::time::sleep per the old design\n"
        "    cx.spawn(async {});\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_tokio_main_fn(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "mainfn.rs",
        "#[tokio::main]\n"
        "async fn main() {\n"
        "    tokio::time::sleep(Duration::from_secs(1)).await;\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


def test_pass_handle_guard(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "guard.rs",
        "fn f() {\n"
        "    if tokio::runtime::Handle::try_current().is_ok() {\n"
        "        tokio::spawn(async {});\n"
        "    }\n"
        "}\n",
    )
    assert wc.check_no_bare_tokio_in_panel_src() == []


# ── FAIL cases (findings expected) ───────────────────────────────────


def test_fail_bare_sleep_non_test_fn(isolated_tree: Any) -> None:
    # Mirrors the chat_panel.rs:544 consent-reconnect-backoff panic.
    wc, root = isolated_tree
    _panel(
        root,
        "reconnect.rs",
        "fn spawn_reconnect(&self) {\n"
        "    loop {\n"
        "        tokio::time::sleep(Duration::from_secs(backoff));\n"
        "    }\n"
        "}\n",
    )
    found = wc.check_no_bare_tokio_in_panel_src()
    assert len(found) == 1
    assert found[0].rule == "no_bare_tokio_in_panel_src"
    assert found[0].severity == "error"
    assert found[0].line == 3


def test_fail_tokio_spawn_in_handler(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "handler.rs",
        "fn on_click(&mut self) {\n"
        "    tokio::spawn(async move { send().await; });\n"
        "}\n",
    )
    found = wc.check_no_bare_tokio_in_panel_src()
    assert len(found) == 1
    assert found[0].line == 2


def test_fail_spawn_blocking_in_render(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "render.rs",
        "fn render(&mut self, cx: &mut Context<Self>) -> impl IntoElement {\n"
        "    tokio::task::spawn_blocking(|| heavy());\n"
        "    div()\n"
        "}\n",
    )
    found = wc.check_no_bare_tokio_in_panel_src()
    assert len(found) == 1
    assert found[0].line == 2


def test_fail_runtime_construction(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "boot.rs",
        "fn boot() {\n"
        "    let rt = Runtime::new().unwrap();\n"
        "    let rt2 = Builder::new_multi_thread().enable_all().build().unwrap();\n"
        "}\n",
    )
    found = wc.check_no_bare_tokio_in_panel_src()
    assert len(found) == 2


# ── dispatcher wiring ────────────────────────────────────────────────


def test_rule50_registered_in_dispatcher(isolated_tree: Any) -> None:
    wc, _ = isolated_tree
    result = wc.run_all(only=["no_bare_tokio_in_panel_src"])
    assert result["ok"] is True
    assert "no_bare_tokio_in_panel_src" in result["data"]["summary"]["by_rule"]
