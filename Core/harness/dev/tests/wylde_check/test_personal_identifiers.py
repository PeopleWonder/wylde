"""Tests for rule 55 (``no_personal_identifiers``) — mirrors prod-side
``wylde_check/rules/_personal_identifiers.py``.

This repo is public. The 2026-05-31 scrub drove the maintainer's name and
home-directory paths to zero by hand and recorded "0 remaining"; seven
weeks later the tree held ~175 name occurrences and 11 personal paths
again, because nothing failed in between. These tests are the "something
fails" half — each FAIL case below is a shape that actually regrew.

The name half is matched by salted digest, so the tests construct the
denied token the same way the fixtures do (never as a literal), keeping
the test file itself clean.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

# The denied token, assembled so this file never contains it literally —
# the same discipline the rule itself follows.
DENIED = "A" + "aron"
DENIED_SURNAME = "R" + "oberts"


# ── PASS cases (no findings) ─────────────────────────────────────────


def test_pass_placeholder_paths(isolated_tree: Any) -> None:
    """Templated home paths are exactly what the fix looks like."""
    wc, root = isolated_tree
    _write(
        root / "docs" / "ok.md",
        "Install to `C:\\Users\\<you>\\Tools`, or `%USERPROFILE%\\Tools`.\n"
        "On Linux that is `/home/user/tools` or `$HOME/tools`.\n"
        "CI uses `/home/runner/work` and `C:\\Users\\runneradmin`.\n",
    )
    assert wc.check_no_personal_identifiers() == []


def test_pass_role_word_instead_of_name(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "docs" / "roles.md",
        "The maintainer confirmed the decision; the Wylde user sees a prompt.\n",
    )
    assert wc.check_no_personal_identifiers() == []


def test_pass_neutral_sample_name_in_fixture(isolated_tree: Any) -> None:
    """Test fixtures need *a* name — just not the maintainer's."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "x" / "src" / "profile.rs",
        '        let p = profile(json!({"name": "Sam"}));\n'
        '        assert_eq!(p.name.as_deref(), Some("Sam"));\n',
    )
    assert wc.check_no_personal_identifiers() == []


def test_pass_opt_out_marker(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "docs" / "vendored.md",
        "Upstream's own error text, quoted verbatim:\n"
        "<!-- wylde-check: personal-identifier-ok -->\n"
        "    error: cannot open /home/buildbot/upstream/x.c\n",
    )
    assert wc.check_no_personal_identifiers() == []


def test_pass_rule_source_is_not_self_flagged(isolated_tree: Any) -> None:
    """The rule's own file discusses the patterns; it must not flag itself."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "dev" / "wylde_check" / "rules"
        / "_personal_identifiers.py",
        f'BAD = "C:\\\\Users\\\\{DENIED}"  # discussed, not leaked\n',
    )
    assert wc.check_no_personal_identifiers() == []


# ── FAIL cases (the drift that actually happened) ────────────────────


def test_fail_windows_home_path(isolated_tree: Any) -> None:
    """The .gitignore / CHANGELOG / docs shape that regrew 11 times."""
    wc, root = isolated_tree
    _write(
        root / "docs" / "drift.md",
        f"Junction: `C:\\Users\\{DENIED}\\Wylde\\Core\\docs\\plans`\n",
    )
    found = wc.check_no_personal_identifiers()
    assert len(found) >= 1
    assert any(f.rule == "no_personal_identifiers" for f in found)
    assert any(f.severity == "error" for f in found)
    assert any(f.line == 1 for f in found)


def test_fail_posix_home_path(isolated_tree: Any) -> None:
    """A personal POSIX home path trips *both* halves of the rule — the
    path check on the segment, the name check on the same token — which
    is the intended belt-and-braces, not a double-report bug."""
    wc, root = isolated_tree
    _write(root / "tools" / "run.sh", f"cd /home/{DENIED.lower()}/wylde || exit 1\n")
    found = wc.check_no_personal_identifiers()
    assert {f.rule for f in found} == {"no_personal_identifiers"}
    assert len(found) == 2
    assert all(f.severity == "error" for f in found)


def test_fail_name_in_prose(isolated_tree: Any) -> None:
    """The ~175-occurrence class: crediting decisions by personal name."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "x" / "src" / "config.rs",
        f"/// Never inject silently ({DENIED}'s lock): show the menu first.\n",
    )
    found = wc.check_no_personal_identifiers()
    assert len(found) == 1
    assert found[0].severity == "error"


def test_fail_name_in_test_fixture(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "x" / "src" / "ipc.rs",
        f'            "name": "{DENIED}",\n',
    )
    assert len(wc.check_no_personal_identifiers()) == 1


def test_fail_surname(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "GUI" / "Cargo.toml", f'authors = ["{DENIED_SURNAME}"]\n')
    assert len(wc.check_no_personal_identifiers()) == 1


def test_fail_name_is_case_insensitive(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "docs" / "lower.md", f"the {DENIED.lower()} rig runs it\n")
    assert len(wc.check_no_personal_identifiers()) == 1


def test_finding_context_never_echoes_the_name(isolated_tree: Any) -> None:
    """A gate that prints the secret into CI logs defeats itself."""
    wc, root = isolated_tree
    _write(root / "docs" / "leak.md", f"{DENIED} approved this.\n")
    found = wc.check_no_personal_identifiers()
    assert len(found) == 1
    blob = f"{found[0].context}{found[0].message}"
    assert DENIED.lower() not in blob.lower()


def test_path_finding_never_echoes_the_account_segment(isolated_tree: Any) -> None:
    """Same discipline for the path half.

    The G8 CI job prints every finding, and its logs are public — so a
    message or context quoting the account name would re-disclose exactly
    what this rule exists to remove. ``file:line`` is enough to fix it.
    """
    wc, root = isolated_tree
    _write(root / "docs" / "p.md", "See C:\\Users\\jdoe\\Wylde for the tree.\n")
    found = wc.check_no_personal_identifiers()
    assert len(found) == 1
    blob = f"{found[0].context}{found[0].message}"
    assert "jdoe" not in blob
    assert found[0].file == "docs/p.md" and found[0].line == 1


def test_fail_reports_each_occurrence(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "docs" / "many.md",
        f"{DENIED} decided.\n"
        "Unrelated line.\n"
        f"Path `C:\\Users\\{DENIED}\\x` and {DENIED_SURNAME} too.\n",
    )
    found = wc.check_no_personal_identifiers()
    # line 1: name; line 3: path + name + surname
    assert len(found) >= 4
    assert {f.line for f in found} == {1, 3}
