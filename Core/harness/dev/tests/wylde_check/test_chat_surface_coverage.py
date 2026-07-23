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

from .conftest import _import_check, _write

CHAT_SRC = ("Core", "GUI", "Frontend", "Panels", "Chat", "src")
CHAT_TESTS = ("Core", "GUI", "Frontend", "Panels", "Chat", "tests")
SHELL_SRC = ("Core", "GUI", "Shell", "src")

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


def _shell(root: Any, body: str | None = None) -> None:
    """A Shell source with no composer signals.

    Every synthetic tree needs one. The rule now requires each root named in
    `GUI_SCAN_ROOTS` to contribute at least one scanned file, because `Shell`
    sat in the path pattern matching NOTHING for the rule's whole life and the
    scan still reported a clean pass. A fixture tree with no Shell would be a
    fixture that cannot reproduce the shape of the real one.
    """
    _at(
        root,
        SHELL_SRC,
        "nav.rs",
        body or 'pub fn nav_bar() -> Div {\n    div().child("Chat")\n}\n',
    )


def _tree(root: Any, **kw: Any) -> None:
    _chat_panel(root, **{k: v for k, v in kw.items() if k == "variants"})
    _e2e(root, **{k: v for k, v in kw.items() if k != "variants"})
    _shell(root)


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


# ── Scan reach: the Shell (issue #250) ───────────────────────────────
#
# `Shell` sat in `_GUI_SRC_RE` matching NOTHING for the rule's entire life,
# because `.*/src/` needs a path segment between the crate root and `src` and
# `Core/GUI/Shell/src/…` has none. The scan reported 158 files and a clean
# pass while all 13 Shell sources were invisible. These pin both the fix and
# the guard that makes the class of bug loud instead of silent.


def test_a_chat_composer_in_the_shell_is_found(isolated_tree: Any) -> None:
    """The regression this fixes.

    The Shell owns the nav chrome; a chat bar added there is a real place a
    user can type. Before the fix this file was unreachable by the scan, so
    it would have shipped covered by nothing while the rule passed green.
    """
    wc, root = isolated_tree
    _tree(root)
    _shell(
        root,
        "pub fn shell_chat_bar(cx: &mut Context<Shell>) -> Div {\n"
        "    let input = cx.new(|c| {\n"
        "        TextInput::multi_line(c).with_submit_mode(SubmitMode::EnterSubmits)\n"
        "    });\n"
        "    cx.subscribe(&input, |this, _e, ev, cx| {\n"
        "        this.send_user_message(ev.text(), cx);\n"
        "    });\n"
        "    div()\n"
        "}\n",
    )
    findings = wc.check_chat_surfaces_are_e2e_covered()
    flagged = [f for f in findings if f.file == "Core/GUI/Shell/src/nav.rs"]
    assert len(flagged) == 1, (
        "the Shell composer must be flagged; got "
        f"{[(f.file, f.message[:60]) for f in findings]}"
    )
    assert "COVERED_COMPOSER_FILES" in flagged[0].message


def test_a_declared_shell_composer_passes(isolated_tree: Any) -> None:
    """The other direction: once declared, the Shell composer is accepted —
    so the rule is genuinely scanning it rather than flagging it blindly."""
    wc, root = isolated_tree
    _chat_panel(root)
    _e2e(
        root,
        composer_files=f'&["{SCOPE_REL}", "Core/GUI/Shell/src/nav.rs"]',
    )
    _shell(
        root,
        "pub fn shell_chat_bar(cx: &mut Context<Shell>) -> Div {\n"
        "    let input = cx.new(|c| {\n"
        "        TextInput::multi_line(c).with_submit_mode(SubmitMode::EnterSubmits)\n"
        "    });\n"
        "    cx.subscribe(&input, |this, _e, ev, cx| {\n"
        "        this.send_user_message(ev.text(), cx);\n"
        "    });\n"
        "    div()\n"
        "}\n",
    )
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_a_shell_file_without_composer_signals_is_not_flagged(
    isolated_tree: Any,
) -> None:
    """Scanning the Shell must not mean crying wolf over it. This is the real
    tree's shape: 13 Shell sources, none of them a composer."""
    wc, root = isolated_tree
    _tree(root)
    assert wc.check_chat_surfaces_are_e2e_covered() == []


def test_a_named_root_the_pattern_cannot_reach_is_an_error(
    isolated_tree: Any, monkeypatch: Any
) -> None:
    """The guard, exercised against the exact original bug.

    Restoring the old `.*/src/` pattern must now produce a finding rather than
    a silent 158-file "clean" pass. Without this, the next person to touch the
    path form gets no signal at all that they disarmed a root.
    """
    import re as _re

    wc, root = isolated_tree
    _tree(root)
    monkeypatch.setattr(
        wc.rules._chat_surface_coverage,
        "_GUI_SRC_RE",
        _re.compile(r"^Core/GUI/(Frontend|Shell)/.*/src/.+\.rs$"),
    )
    findings = wc.check_chat_surfaces_are_e2e_covered()
    assert len(findings) == 1
    assert findings[0].severity == "error"
    assert findings[0].file == "Core/GUI/Shell"
    assert "matched no file under Core/GUI/Shell" in findings[0].message


def test_every_root_in_gui_scan_roots_is_reachable_in_the_real_tree() -> None:
    """Belt and braces against the fixture lying.

    The guard above runs against a synthetic tree the test itself wrote. This
    one asserts the shipped pattern reaches every named root in the REAL
    checkout — which is the thing that was actually false for two months.
    """
    import re as _re

    wc = _import_check()
    mod = wc.rules._chat_surface_coverage
    from Core.harness.dev.wylde_check._walkers import _to_rel, _walk

    scanned = [
        _to_rel(p) for p in _walk((".rs",), ("Core/GUI",)) if mod._GUI_SRC_RE.match(_to_rel(p))
    ]
    for scan_root in mod.GUI_SCAN_ROOTS:
        n = sum(1 for r in scanned if r.startswith(scan_root + "/"))
        assert n > 0, f"{scan_root} is named in GUI_SCAN_ROOTS but the pattern reaches 0 files"
    assert _re.compile(r"\(\.\*\/\)\?").search(
        mod._GUI_SRC_RE.pattern
    ), "the path form must keep the optional-segment shape that reaches a crate's own src/"
