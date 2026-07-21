#!/usr/bin/env python3
"""check-voice-presets-mirror.py — the gate that makes a drifted voice preset
list turn CI RED instead of shipping a picker the service will reject (#129).

Four voice value-domain lists are duplicated across the Core/GUI <-> rust/ cargo
workspace boundary. The GUI cannot simply `use` the canonical crate: importing
`wylde-voice` would drag the audio stack (cpal) into the headless panel-walk
gate, which segfaults (Core/GUI/.cargo/config.toml). So the lists are copied,
and until now the only thing holding them in sync was the word "Mirrors" in a
doc comment. A GUI picker offering a value the service validator no longer
accepts produces a rejected `voice.set_config` — the user picks a legal-looking
option and it silently fails.

This is a MECHANICAL cross-check, not an import. It:

  1. finds each `pub const <NAME>: &[&str]` in the GUI Settings ipc.rs whose
     doc comment names a canonical const via the `/// Mirrors ...
     config_persist::<CANON>` convention,
  2. resolves both lists to their string values (the canonical side builds its
     lists from named `&str` consts, e.g. `ALL_BACKENDS = &[BACKEND_AUTO, ...]`;
     the GUI side uses literals), and
  3. asserts they are EQUAL — same values, same order (the doc comments call
     these "cycle order", so order is load-bearing).

The pairing is derived from the `/// Mirrors` comment the code already carries
(criterion 2), so the fix does not reintroduce a second hand-kept mapping.

    tools/check-voice-presets-mirror.py            # check the repo; exit 1 on drift
    tools/check-voice-presets-mirror.py --selftest # prove the check catches drift

Pure Python stdlib (no PyYAML / third-party); mirrors the `python
tools/check_release_milestones.py` precedent for a Python CI gate.
"""
from __future__ import annotations

import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUI_IPC = os.path.join(
    REPO_ROOT, "Core", "GUI", "Frontend", "Panels", "Settings", "src", "ipc.rs"
)
CANON = os.path.join(
    REPO_ROOT, "rust", "crates", "wylde-voice", "src", "config_persist.rs"
)

# `pub const NAME: &str = "value";`  — the single-string consts a list may cite.
_STR_CONST = re.compile(
    r'pub\s+const\s+(?P<name>[A-Z0-9_]+)\s*:\s*&str\s*=\s*"(?P<val>(?:[^"\\]|\\.)*)"\s*;'
)
# `pub const NAME: &[&str] = &[ ... ];`  — the list consts we compare. The body
# is captured non-greedily up to the closing `];` (arrays here never nest).
_LIST_CONST = re.compile(
    r"pub\s+const\s+(?P<name>[A-Z0-9_]+)\s*:\s*&\[&str\]\s*=\s*&\[(?P<body>.*?)\]\s*;",
    re.DOTALL,
)
# One element of a list body: a "quoted literal" or a BARE_IDENT reference.
_ELEM = re.compile(r'"(?P<lit>(?:[^"\\]|\\.)*)"|(?P<ident>[A-Za-z_][A-Za-z0-9_]*)')


def _str_consts(text: str) -> dict[str, str]:
    return {m.group("name"): m.group("val") for m in _STR_CONST.finditer(text)}


def _resolve_list(name: str, body: str, str_consts: dict[str, str]) -> list[str]:
    """Resolve a list body to string values. Identifiers are looked up in
    `str_consts`; an unresolved identifier is a hard error (we cannot compare
    what we cannot resolve, and silently dropping it would hide drift)."""
    out: list[str] = []
    for m in _ELEM.finditer(body):
        if m.group("lit") is not None:
            out.append(m.group("lit"))
        else:
            ident = m.group("ident")
            if ident not in str_consts:
                raise KeyError(
                    f"{name}: element `{ident}` is not a resolvable &str const"
                )
            out.append(str_consts[ident])
    return out


def _list_consts(text: str) -> dict[str, list[str]]:
    sc = _str_consts(text)
    return {
        m.group("name"): _resolve_list(m.group("name"), m.group("body"), sc)
        for m in _LIST_CONST.finditer(text)
    }


def _mirror_pairs(gui_text: str) -> list[tuple[str, str, list[str]]]:
    """For each GUI `&[&str]` const preceded by a `/// Mirrors ...
    config_persist::<CANON>` doc block, yield (gui_name, canon_name, values).
    The doc block is the run of contiguous `///` lines immediately above the
    const; the `Mirrors` reference may wrap across several of them."""
    gui_sc = _str_consts(gui_text)
    lines = gui_text.splitlines()
    # Map a const's start line -> the const match, so we can look upward.
    pairs: list[tuple[str, str, list[str]]] = []
    for m in _LIST_CONST.finditer(gui_text):
        start_line = gui_text.count("\n", 0, m.start())
        # Walk up collecting the contiguous doc-comment block.
        doc: list[str] = []
        i = start_line - 1
        while i >= 0 and lines[i].lstrip().startswith("///"):
            doc.append(lines[i].lstrip()[3:].strip())
            i -= 1
        doc_text = " ".join(reversed(doc))
        ref = re.search(r"config_persist::([A-Z0-9_]+)", doc_text)
        if not ref:
            continue  # a list const that does not claim to mirror anything
        values = _resolve_list(m.group("name"), m.group("body"), gui_sc)
        pairs.append((m.group("name"), ref.group(1), values))
    return pairs


def check(gui_text: str, canon_text: str) -> list[str]:
    """Return a list of human-readable drift messages; empty means in sync."""
    problems: list[str] = []
    canon = _list_consts(canon_text)
    pairs = _mirror_pairs(gui_text)
    if not pairs:
        problems.append(
            "no `/// Mirrors config_persist::<CONST>` preset consts found in the "
            "GUI ipc.rs — the convention this check keys on has moved or broken"
        )
        return problems
    for gui_name, canon_name, gui_vals in pairs:
        if canon_name not in canon:
            problems.append(
                f"{gui_name} claims to mirror config_persist::{canon_name}, "
                f"but no such &[&str] const exists there"
            )
            continue
        canon_vals = canon[canon_name]
        if gui_vals != canon_vals:
            problems.append(
                f"DRIFT: GUI {gui_name} != config_persist::{canon_name}\n"
                f"    GUI      ({len(gui_vals)}): {gui_vals}\n"
                f"    canonical({len(canon_vals)}): {canon_vals}"
            )
    return problems


def _selftest() -> None:
    gui = (
        '/// Backends. Mirrors\n'
        '/// wylde_voice::config_persist::ALL_BACKENDS.\n'
        'pub const BACKEND_PRESETS: &[&str] = &["auto", "cpu", "npu"];\n'
        '/// VAD. Mirrors `wylde_voice::config_persist::ALL_VAD_SENSITIVITIES`.\n'
        'pub const VAD_PRESETS: &[&str] = &["low", "medium", "high"];\n'
    )
    canon = (
        'pub const BACKEND_AUTO: &str = "auto";\n'
        'pub const BACKEND_CPU: &str = "cpu";\n'
        'pub const BACKEND_NPU: &str = "npu";\n'
        'pub const ALL_BACKENDS: &[&str] = &[BACKEND_AUTO, BACKEND_CPU, BACKEND_NPU];\n'
        'pub const ALL_VAD_SENSITIVITIES: &[&str] = &["low", "medium", "high"];\n'
    )
    assert check(gui, canon) == [], "in-sync fixture must report no drift"

    # Falsification 1: append a value on the GUI side only -> must be caught.
    drifted = gui.replace('"npu"];', '"npu", "vulkan"];')
    assert any("BACKEND_PRESETS" in p for p in check(drifted, canon)), (
        "an appended GUI value must be reported as drift"
    )
    # Falsification 2: reorder on the GUI side only -> must be caught (order matters).
    reordered = gui.replace(
        '&["low", "medium", "high"]', '&["medium", "low", "high"]'
    )
    assert any("VAD_PRESETS" in p for p in check(reordered, canon)), (
        "a reordered GUI list must be reported as drift"
    )
    # A missing canonical const must be reported, not silently passed.
    assert check(
        '/// Mirrors config_persist::NOPE.\npub const X_PRESETS: &[&str] = &["a"];\n',
        "pub const UNRELATED: &[&str] = &[];\n",
    ), "a mirror pointing at a nonexistent canonical const must be reported"
    print("selftest OK: in-sync passes; appended/reordered/missing all caught")


def main(argv: list[str]) -> int:
    if len(argv) > 1 and argv[1] == "--selftest":
        _selftest()
        return 0
    try:
        gui_text = open(GUI_IPC, encoding="utf-8").read()
        canon_text = open(CANON, encoding="utf-8").read()
    except OSError as e:
        print(f"::error::cannot read a voice source file: {e}", file=sys.stderr)
        return 1
    problems = check(gui_text, canon_text)
    if problems:
        print(
            "::error::GUI voice preset list(s) have drifted from "
            "wylde-voice config_persist (see tools/check-voice-presets-mirror.py, "
            "#129):",
            file=sys.stderr,
        )
        for p in problems:
            print("  " + p.replace("\n", "\n  "), file=sys.stderr)
        print(
            f"\nFix so each list in {os.path.relpath(GUI_IPC, REPO_ROOT)} equals "
            f"(value and order) the config_persist const its `/// Mirrors` comment names.",
            file=sys.stderr,
        )
        return 1
    print(
        "OK: every GUI voice preset list mirrors its wylde-voice canonical "
        "const (value and order)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
