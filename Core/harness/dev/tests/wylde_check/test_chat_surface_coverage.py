"""Tests for rule 58 (``chat_surfaces_are_e2e_covered``) — mirrors prod-side
``wylde_check/rules/_chat_surface_coverage.py``.

Chat is the product's primary path and has more than one entry point. The
failure this rule exists to catch is not a red test — it is a *quiet* one:
a new chat surface added without a case in the all-surfaces chat-turn e2e
(#236), leaving a real place a user can type covered by nothing while the
suite stays green.

Two enforcement halves are exercised here — the ``ChatScope`` variant
cross-check and the send-capable-composer source scan — plus the
corpus-missing guards, because a rule that points at a deleted file goes
quiet rather than red (#101/#116).
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

CHAT_SRC = ("Core", "GUI", "Frontend", "Panels", "Chat", "src")
CHAT_TESTS = ("Core", "GUI", "Frontend", "Panels", "Chat", "tests")

SCOPE_REL = "Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs"


def _at(root: Any, parts: tuple, name: str, body: str) -> None:
    path = root
    for part in parts:
        path = path / part
    _write(path / name, body)


def _chat_panel(root: Any, variants: str = "    Global,\n    Docked,\n") -> None:
    """The `ChatScope` enum plus the real panel's composer signals."""
    _at(
        root,
        CHAT_SRC,
        "chat_panel.rs",
        "pub enum ChatScope {\n"
        f"{variants}"
        "}\n"
        "\n"
        "impl ChatPanel {\n"
        "    pub fn new(scope: ChatScope, cx: &mut Context<Self>) -> Self {\n"
        "        let prompt_input = cx.new(|c| {\n"
        "            TextInput::multi_line(c).with_submit_mode(SubmitMode::EnterSubmits)\n"
        "        });\n"
        "        cx.subscribe(&prompt_input, |this, _e, ev, cx| {\n"
        "            this.submit_text(ev.text(), cx);\n"
        "        });\n"
        "    }\n"
        "}\n",
    )


def _e2e(
    root: Any,
    covered: str = "&[spec(ChatScope::Global), spec(ChatScope::Docked)]",
    composer_files: str = f'&["{SCOPE_REL}"]',
) -> None:
    _at(
        root,
        CHAT_TESTS,
        "chat_turn_e2e.rs",
        f"const COVERED: &[SurfaceSpec] = {covered};\n"
        f"const COVERED_COMPOSER_FILES: &[&str] = {composer_files};\n",
    )


def _tree(root: Any, **kw: Any) -> None:
    _chat_panel(root, **{k: v for k, v in kw.items() if k == "variants"})
    _e2e(root, **{k: v for k, v in kw.items() if k != "variants"})


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_every_scope_covered(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _tree(root)
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_pass_search_field_is_not_a_chat_composer(isolated_tree: Any) -> None:
    """`EnterSubmits` alone is a search box, not a chat bar.

    The Models panel's model-search field is exactly this shape. Flagging it
    would make the rule cry wolf on every filter input in the GUI.
    """
    wc, root = isolated_tree
    _tree(root)
    _at(
        root,
        ("Core", "GUI", "Frontend", "Panels", "Models", "src"),
        "models_panel.rs",
        "let search = TextInput::single_line(cx)\n"
        "    .with_submit_mode(SubmitMode::EnterSubmits);\n"
        "// Enter runs a model search; no turn is started.\n",
    )
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_pass_turn_path_without_a_composer(isolated_tree: Any) -> None:
    """Reaching the turn path is not enough — a cancel/plumbing module that
    calls into chat but owns no input is not an entry point."""
    wc, root = isolated_tree
    _tree(root)
    _at(
        root,
        CHAT_SRC,
        "ipc.rs",
        'pub async fn start_turn_with_model() { call("chat.start_turn").await; }\n',
    )
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_pass_composer_mentioned_only_in_a_comment(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _tree(root)
    _at(
        root,
        ("Core", "GUI", "Frontend", "Panels", "Tools", "src"),
        "tools_panel.rs",
        "// The Chat panel uses SubmitMode::EnterSubmits and calls\n"
        "// send_user_message; this panel does neither.\n"
        "let filter = TextInput::single_line(cx);\n",
    )
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_pass_test_sources_are_out_of_scope(isolated_tree: Any) -> None:
    """A fixture composer inside a test is not a shipped entry point."""
    wc, root = isolated_tree
    _tree(root)
    _at(
        root,
        CHAT_TESTS,
        "some_other_test.rs",
        "let input = TextInput::multi_line(cx)\n"
        "    .with_submit_mode(SubmitMode::EnterSubmits);\n"
        "panel.send_user_message(text, cx);\n",
    )
    assert wc.check_chat_surfaces_are_e2e_covered() == []


# ── FAIL cases ───────────────────────────────────────────────────────


def test_fail_new_scope_variant_not_in_covered(isolated_tree: Any) -> None:
    """THE headline case: a third chat surface exists and nothing drives it."""
    wc, root = isolated_tree
    _tree(root, variants="    Global,\n    Docked,\n    Sidebar,\n")
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert len(findings) == 1
    assert "ChatScope::Sidebar" in findings[0].message
    assert findings[0].severity == "error"


def test_fail_covered_names_a_scope_that_no_longer_exists(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _chat_panel(root, variants="    Global,\n")
    _e2e(root)
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert any("ChatScope::Docked" in f.message and "stale" in f.message for f in findings)


def test_fail_new_panel_grows_its_own_chat_bar(isolated_tree: Any) -> None:
    """The case the exhaustive `ChatScope` match is structurally blind to:
    a new composer that adds no variant at all."""
    wc, root = isolated_tree
    _tree(root)
    _at(
        root,
        ("Core", "GUI", "Frontend", "Panels", "Dashboard", "src"),
        "quick_ask.rs",
        "let ask = TextInput::multi_line(cx)\n"
        "    .with_submit_mode(SubmitMode::EnterSubmits);\n"
        "cx.subscribe(&ask, |this, _e, ev, cx| this.send_user_message(ev.text(), cx));\n",
    )
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert len(findings) == 1
    assert findings[0].file.endswith("Dashboard/src/quick_ask.rs")
    assert "COVERED_COMPOSER_FILES" in findings[0].message


def test_fail_declared_composer_file_lost_its_send_wiring(isolated_tree: Any) -> None:
    """A declared file with no composer left means either the list rotted or
    — worse — the surface silently lost its send."""
    wc, root = isolated_tree
    _at(root, CHAT_SRC, "chat_panel.rs", "pub enum ChatScope {\n    Global,\n}\n")
    _e2e(root, covered="&[spec(ChatScope::Global)]")
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert any("no send-capable chat composer was found there" in f.message for f in findings)


def test_fail_empty_composer_file_list_does_not_pass_vacuously(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _chat_panel(root)
    _e2e(root, composer_files="&[]")
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert any("vacuously" in f.message for f in findings)


# ── Corpus guards (a missing target must go RED, not quiet) ──────────


def test_fail_e2e_test_deleted(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _chat_panel(root)
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert len(findings) == 1
    assert "covered by nothing" in findings[0].message


def test_fail_scope_definition_missing(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _e2e(root)
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert len(findings) == 1
    assert "cannot enumerate chat surfaces" in findings[0].message


def test_fail_scope_enum_shape_changed_beyond_recognition(isolated_tree: Any) -> None:
    """If the enum can no longer be parsed the rule must say so, not pass."""
    wc, root = isolated_tree
    _at(root, CHAT_SRC, "chat_panel.rs", "pub struct ChatScope;\n")
    _e2e(root)
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert any("checking nothing" in f.message for f in findings)
