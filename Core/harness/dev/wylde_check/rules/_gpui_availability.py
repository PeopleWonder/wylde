"""Rule 57 — ``service_backed_surface_declares_availability``.

**The invariant: no surface may present a service-backed thing as working
when it isn't** ("no silent dead panel", #239).

The GUI gates a panel's dependence on services two ways, and both are
already structural:

* a panel that depends on services declares them in ``required_services``
  and the Shell's :func:`NavModel::slot_state` renders
  ``SlotState::ServiceUnavailable`` when one is down (rule 40 enforces the
  *declaration*); and
* a first-party ``iframe`` panel additionally gets the Shell's URL probe
  (``spawn_iframe_probe`` → ``IframeHealth`` → a synthesised
  ``ServiceUnavailable``).

**Neither covers a panel that renders a list of *per-item* endpoints.**
That is the hole #239 came through, and it is worth being exact about why
the existing gates missed it: the Tools panel declared
``wylde-extension-bridge`` correctly, so rule 40 was *satisfied* — the
bridge was up, the panel mounted, and the panel then drew one card per
extension panel, each pointing at a **different** service's URL that
nothing checked. `Extensions/wylde-images` pointed at a port whose service
had been extracted and rendered exactly like a working one.

So a panel-level gate is structurally incapable of covering a per-item
surface: the unit that can be dead is the item, not the panel.

This rule closes that, derived rather than enumerated, so a surface added
later is covered without anyone remembering:

* **Clause A** — every wire row that carries an endpoint (a ``pub url``
  field) must also carry an availability field.  The endpoint is the tell:
  a row that models something remote can be dead, so it has to say whether
  it is.  ERROR.
* **Clause B** — the panel that owns such a row must actually *consult*
  that field outside the wire module.  A field nothing reads is the same
  silent dead panel with extra steps.  ERROR.
* **Clause C** — a panel that opts out of the Shell's ``required_services``
  gate (rule 40) has taken responsibility for rendering unavailability
  itself, so it must demonstrably do so.  The opt-out is otherwise a free
  pass out of every gate in this file.  ERROR.

Corpus (both sides of the wire, so this is repo-wide and not a GUI-only
check): ``Core/GUI/Frontend/Panels/*/src/ipc.rs`` — the panel↔service wire
shapes — plus the producer of the extension-panel rows,
``rust/crates/wylde-extension-bridge/src/host.rs``.  Both are registered in
``RULE_TARGET_SPECS`` so emptying either goes red instead of quietly
disarming this rule.

Why a source rule and not a Rust test: the property has to hold for a panel
that does not exist yet.  A test can only assert about code that is already
written, and `Core/GUI` CI runs ``build`` + ``panel-walk`` only, so a test
in the registry crate would not execute at all.  This runs in the
``wylde_check (full rule set)`` job over a plain checkout.
"""

from __future__ import annotations

import json
import re
import sys as _sys
from pathlib import Path
from typing import List, Set, Tuple

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]

GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"

#: The producer side of the extension-panel wire rows.  Listed explicitly
#: rather than walked because exactly one file in ``rust/`` models a
#: GUI-rendered remote surface; a blanket walk of every ``pub url`` in the
#: backend would fire on config structs and teach people to ignore it.
BRIDGE_WIRE_FILE: str = "rust/crates/wylde-extension-bridge/src/host.rs"

#: Field name a row uses to report whether its endpoint is actually there.
AVAILABILITY_FIELD: str = "availability"

#: The rule 40 opt-out.  A panel carrying it has told the tree it renders
#: degraded/per-item state instead of the Shell stub — clause C makes it
#: prove that rather than simply exempting itself.
RULE_40_OPT_OUT: str = "required_services_includes_called_services"

#: Tokens that count as "this source consults availability / renders a
#: status".  Deliberately a small, concrete set: these are the names the
#: mechanism actually uses, not a generic vocabulary.
_STATUS_TOKENS: Tuple[str, ...] = (
    AVAILABILITY_FIELD,
    "is_live",
    "Unavailable",
    "unreachable",
    "not_running",
    "ServiceUnavailable",
)

_STRUCT_RE = re.compile(r"pub struct (\w+)\s*\{(.*?)\n\}", re.DOTALL)
_PUB_FIELD_RE = re.compile(r"pub (\w+)\s*:")


def _struct_fields(text: str) -> List[Tuple[str, Set[str], int]]:
    """``(struct_name, pub_field_names, 1-based line)`` for each ``pub struct``."""
    out: List[Tuple[str, Set[str], int]] = []
    for m in _STRUCT_RE.finditer(text):
        name = m.group(1)
        fields = set(_PUB_FIELD_RE.findall(m.group(2)))
        line = text.count("\n", 0, m.start()) + 1
        out.append((name, fields, line))
    return out


def _panel_dirs() -> List[Path]:
    base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if not base.exists():
        return []
    return [c for c in sorted(base.iterdir()) if c.is_dir()]


def _wire_files() -> List[Path]:
    """Every file that models a GUI-rendered remote surface."""
    out: List[Path] = []
    for panel in _panel_dirs():
        ipc = panel / "src" / "ipc.rs"
        if ipc.exists() and not _is_excluded(ipc):
            out.append(ipc)
    bridge = _pkg.WYLDE_ROOT / BRIDGE_WIRE_FILE
    if bridge.exists() and not _is_excluded(bridge):
        out.append(bridge)
    return out


def _consults_status(paths: List[Path]) -> bool:
    """True if any of ``paths`` mentions a status/availability token."""
    for p in paths:
        text = _read_text(p) or ""
        if any(tok in text for tok in _STATUS_TOKENS):
            return True
    return False


def _render_sources(panel_dir: Path) -> List[Path]:
    """A panel's own sources *excluding* its wire module — the render path.

    Clause B is about the field being *used*, so the file that merely
    declares it must not be what satisfies the check.
    """
    src = panel_dir / "src"
    if not src.exists():
        return []
    return [
        p
        for p in sorted(src.rglob("*.rs"))
        if p.name != "ipc.rs" and not _is_excluded(p)
    ]


def check_service_backed_surface_declares_availability() -> List[Finding]:
    """Every service-backed surface must be able to say it is unavailable.

    See the module docstring for the three clauses and why a panel-level
    gate cannot cover a per-item surface.
    """
    out: List[Finding] = []

    # ── Clauses A + B: endpoint-carrying wire rows ───────────────────
    for wire in _wire_files():
        text = _read_text(wire)
        if text is None:
            continue
        rel = _to_rel(wire)
        # A panel's render path is its sibling sources; the bridge is the
        # producer and has no render path, so clause B does not apply to it.
        panel_dir = wire.parent.parent if wire.name == "ipc.rs" else None

        for name, fields, line in _struct_fields(text):
            if "url" not in fields:
                continue

            # Clause A — the row must carry availability.
            if AVAILABILITY_FIELD not in fields:
                out.append(
                    Finding(
                        rule="service_backed_surface_declares_availability",
                        severity="error",
                        file=rel,
                        line=line,
                        message=(
                            f"`{name}` carries a `url` (it models a remote "
                            f"surface that can be dead) but no "
                            f"`{AVAILABILITY_FIELD}` field, so nothing can "
                            f"tell a live endpoint from a dead one and the "
                            f"GUI renders both identically. Add "
                            f"`{AVAILABILITY_FIELD}` (and a reason field) and "
                            f"populate it from the producer's live probe — "
                            f"see wylde_extension_bridge::availability (#239)."
                        ),
                    )
                )
                continue

            # Clause B — someone outside the wire module must read it.
            if panel_dir is None:
                continue
            renders = _render_sources(panel_dir)
            if renders and not _consults_status(renders):
                out.append(
                    Finding(
                        rule="service_backed_surface_declares_availability",
                        severity="error",
                        file=rel,
                        line=line,
                        message=(
                            f"`{name}` declares `{AVAILABILITY_FIELD}` but "
                            f"nothing in {panel_dir.name}'s render path reads "
                            f"it, so every row still paints as though it "
                            f"works. Render the state (status chip / hide) "
                            f"per item, not per panel (#239)."
                        ),
                    )
                )

    # ── Clause C: the rule 40 opt-out is not a free pass ─────────────
    for panel in _panel_dirs():
        manifest = panel / "manifest.json"
        if not manifest.exists() or _is_excluded(manifest):
            continue
        raw = _read_text(manifest)
        if raw is None:
            continue
        try:
            doc = json.loads(raw)
        except json.JSONDecodeError:
            # Malformed manifests are rule 41's business, not this rule's.
            continue
        if RULE_40_OPT_OUT not in (doc.get("wylde_check_opt_outs") or []):
            continue
        sources = _render_sources(panel) + [panel / "src" / "ipc.rs"]
        sources = [p for p in sources if p.exists()]
        if sources and not _consults_status(sources):
            out.append(
                Finding(
                    rule="service_backed_surface_declares_availability",
                    severity="error",
                    file=_to_rel(manifest),
                    line=0,
                    message=(
                        f"`{panel.name}` opts out of `{RULE_40_OPT_OUT}`, "
                        f"which means it has taken responsibility for showing "
                        f"unavailability itself instead of relying on the "
                        f"Shell's ServiceUnavailable stub — but nothing in "
                        f"its sources renders a status. Either drop the "
                        f"opt-out and declare `required_services`, or render "
                        f"per-item status (#239)."
                    ),
                )
            )

    return out
