"""Tests for rule 57 — ``service_backed_surface_declares_availability`` (#239).

Mirrors prod-side ``wylde_check/rules/_gpui_availability.py``.

These exist to prove the gate can **fail**. A structural rule that only
ever reports a pass is worse than no rule, because it reads as coverage
(``docs/known-issues.md`` KI-6). So every clause is asserted in both
directions — the violating tree goes red, and the compliant tree goes
green — and the "a panel this rule has never heard of" case is asserted
explicitly, because that is the whole claim: coverage is derived from the
tree, not from a list someone has to remember to extend.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, List

from .conftest import _write

PANELS = "Core/GUI/Frontend/Panels"
BRIDGE = "rust/crates/wylde-extension-bridge/src/host.rs"


def _seed_panel(
    root: Path,
    name: str,
    *,
    wire: str,
    render: str = "pub fn render() {}\n",
    opt_outs: List[str] | None = None,
    required_services: List[str] | None = None,
) -> None:
    """A first-party panel with a wire module and a render module."""
    _write(root / PANELS / name / "src" / "ipc.rs", wire)
    _write(root / PANELS / name / "src" / f"{name.lower()}_panel.rs", render)
    manifest: dict[str, Any] = {
        "schema_version": 2,
        "service": "core",
        "panels": [
            {
                "id": name.lower(),
                "title": name,
                "required_services": required_services or [],
                "source": {
                    "kind": "gpui_view",
                    "factory": f"wylde_panel_{name.lower()}::Panel::view",
                },
            }
        ],
    }
    if opt_outs is not None:
        manifest["wylde_check_opt_outs"] = opt_outs
    _write(
        root / PANELS / name / "manifest.json",
        json.dumps(manifest, indent=2),
    )


ROW_WITHOUT_AVAILABILITY = """\
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
}
"""

ROW_WITH_AVAILABILITY = """\
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
    pub availability: PanelAvailability,
}
"""

RENDER_THAT_READS_IT = """\
pub fn render(e: &crate::ipc::Endpoint) -> String {
    if e.availability.is_live() { e.url.clone() } else { "UNAVAILABLE".into() }
}
"""

RENDER_THAT_IGNORES_IT = """\
pub fn render(e: &crate::ipc::Endpoint) -> String {
    format!("{} {}", e.name, e.url)
}
"""


def _run(wc: Any) -> List[Any]:
    return wc.check_service_backed_surface_declares_availability()


# ── Clause A: an endpoint-carrying row must carry availability ───────


def test_row_with_url_and_no_availability_is_an_error(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_panel(root, "Widgets", wire=ROW_WITHOUT_AVAILABILITY)
    findings = _run(wc)
    assert len(findings) == 1, findings
    assert findings[0].severity == "error"
    assert "Endpoint" in findings[0].message
    assert "availability" in findings[0].message


def test_row_with_availability_and_a_render_that_reads_it_is_clean(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Widgets",
        wire=ROW_WITH_AVAILABILITY,
        render=RENDER_THAT_READS_IT,
    )
    assert _run(wc) == []


def test_a_row_with_no_url_is_not_this_rules_business(isolated_tree: Any) -> None:
    # The `url` field is the tell for "models something remote that can be
    # dead". A plain data row must not be dragged in, or the rule becomes
    # noise people learn to suppress.
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Widgets",
        wire="pub struct Row {\n    pub name: String,\n    pub count: u32,\n}\n",
    )
    assert _run(wc) == []


# ── Clause B: the field must actually be read ────────────────────────


def test_availability_declared_but_never_rendered_is_an_error(
    isolated_tree: Any,
) -> None:
    # The exact half-fix this clause exists to block: carry the state on the
    # wire, then paint every row as though it works anyway.
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Widgets",
        wire=ROW_WITH_AVAILABILITY,
        render=RENDER_THAT_IGNORES_IT,
    )
    findings = _run(wc)
    assert len(findings) == 1, findings
    assert "nothing in Widgets's render path reads it" in findings[0].message


def test_declaring_the_field_in_ipc_alone_does_not_satisfy_clause_b(
    isolated_tree: Any,
) -> None:
    # ipc.rs necessarily mentions `availability` — it declares it. If the
    # wire module counted as "consulting" it, clause B could never fail.
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Widgets",
        wire=ROW_WITH_AVAILABILITY + "\n// availability is_live unreachable\n",
        render=RENDER_THAT_IGNORES_IT,
    )
    assert len(_run(wc)) == 1


# ── Clause C: the rule-40 opt-out is not a free pass ─────────────────


def test_opt_out_without_any_status_rendering_is_an_error(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Blank",
        wire="pub struct Row {\n    pub name: String,\n}\n",
        render="pub fn render() -> String { String::new() }\n",
        opt_outs=["required_services_includes_called_services"],
    )
    findings = _run(wc)
    assert len(findings) == 1, findings
    assert "opts out" in findings[0].message
    assert findings[0].file.endswith("manifest.json")


def test_opt_out_is_fine_when_the_panel_does_render_status(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Tiles",
        wire="pub struct Row {\n    pub name: String,\n}\n",
        render="pub fn render() -> String { \"unreachable\".into() }\n",
        opt_outs=["required_services_includes_called_services"],
    )
    assert _run(wc) == []


def test_a_panel_without_the_opt_out_is_left_to_rule_40(
    isolated_tree: Any,
) -> None:
    wc, root = isolated_tree
    _seed_panel(
        root,
        "Plain",
        wire="pub struct Row {\n    pub name: String,\n}\n",
        render="pub fn render() {}\n",
        required_services=["wylde-harness"],
    )
    assert _run(wc) == []


# ── The universality claim ───────────────────────────────────────────


def test_a_panel_the_rule_has_never_heard_of_is_covered(isolated_tree: Any) -> None:
    """The point of the rule: coverage is derived from the tree.

    No panel is named anywhere in `_gpui_availability.py`. A surface added
    later is walked because it exists, not because someone remembered to
    add it to a list — so coverage cannot regress by omission.
    """
    wc, root = isolated_tree
    for name in ("Alpha", "Beta", "Gamma"):
        _seed_panel(
            root,
            name,
            wire=ROW_WITH_AVAILABILITY,
            render=RENDER_THAT_READS_IT,
        )
    assert _run(wc) == []

    # The newest one regresses; only it goes red.
    _seed_panel(root, "Delta", wire=ROW_WITHOUT_AVAILABILITY)
    findings = _run(wc)
    assert len(findings) == 1, findings
    assert "Delta" in findings[0].file


def test_the_producer_side_of_the_wire_is_policed_too(isolated_tree: Any) -> None:
    """Repo-wide, not GUI-only: the bridge mints these rows, so a `url`
    added there without an availability verdict is the same defect one
    layer up."""
    wc, root = isolated_tree
    _write(root / BRIDGE, ROW_WITHOUT_AVAILABILITY)
    findings = _run(wc)
    assert len(findings) == 1, findings
    assert findings[0].file.endswith("host.rs")

    # Clause B does not apply to the producer — it has no render path, so
    # a compliant row there is clean without one.
    _write(root / BRIDGE, ROW_WITH_AVAILABILITY)
    assert _run(wc) == []


def test_rule_is_registered_and_reachable_through_run_all(isolated_tree: Any) -> None:
    # A rule that isn't in the dispatcher enforces nothing, however good it
    # is. Pin the wiring, not just the logic.
    wc, _root = isolated_tree
    res = wc.run_all(only=["service_backed_surface_declares_availability"])
    assert res["ok"] is True
    assert res["data"]["rules_checked"] == 1
