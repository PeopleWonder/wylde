"""Rule 51: every rule's configured target path must exist.

The failure this exists to prevent
----------------------------------

A rule that points at a deleted file does not go red — it goes *quiet*.
Its walker finds nothing, its loop runs zero times, and it reports a
clean pass while checking nothing at all.  The tree looks greener the
more of the rule engine rots.

That has now happened twice.  Rules 44/45 pointed at
``Core/Lifecycle/launcher.py`` and ``Core/Lifecycle/shutdown.py`` after
the Rust cutover deleted them, and passed green for months (issue #101).
Rule 48 pointed at ``rust/crates/wylde-harness/src/pipe.rs`` and
``Core/harness/pipe/__init__.py`` — the first renamed to ``pipe/mod.rs``,
the second deleted outright — leaving 46 Gateway verbs unchecked
(issue #116).

Both were found by hand, by someone auditing rules one at a time.  This
rule makes the next one turn up automatically: it asserts that every
path the rule engine is *configured* to inspect is actually present in
the tree.  Delete a file a rule depends on and CI tells you, on that PR,
instead of the rule quietly becoming decorative.

Why a rule rather than an import-time assertion
-----------------------------------------------

An import-time check would fire inside the unit suite, where
``WYLDE_ROOT`` is monkeypatched to a synthetic ``tmp_path`` that
deliberately contains almost none of these files.  Every test would
explode.  As a rule it runs against whatever root is bound at call time
and is skipped by tests that don't select it.
"""

from __future__ import annotations

import sys as _sys
from typing import Dict, List

from .. import Finding

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# Path → the rule(s) that would silently stop working if it vanished.
#
# Only paths whose absence *disarms* a rule belong here.  A path a rule
# merely walks for violations (e.g. a panel source root) does not: an
# empty walk there is a legitimate "no violations".  The test is
# "if this file disappeared, would the rule still report a pass?" — if
# yes, it belongs in this table.
RULE_TARGET_PATHS: Dict[str, str] = {
    # Rule 31 — manifest orphan reap (repointed #116)
    "rust/crates/wylde-lifecycle/src/daemon.rs": "shutdown_reaps_manifest_orphans",
    "rust/crates/wylde-lifecycle/src/state/orphan_sweep.rs": "shutdown_reaps_manifest_orphans",
    # Rules 38 + 48 — harness pipe action registry (repointed #116)
    "rust/crates/wylde-harness/src/pipe/mod.rs": (
        "panel_verbs_exist_in_harness_registry, "
        "gateway_verbs_exist_in_harness_registry"
    ),
    # Rules 44/45 — DAEMON_MANAGED single-source boot + shutdown (#101)
    "rust/crates/wylde-lifecycle/src/daemon_managed.rs": (
        "launcher_enumerates_services_from_manifests, shutdown_stops_all_services"
    ),
    # Rule 48 — Gateway route surface
    "rust/crates/wylde-gateway/src": "gateway_verbs_exist_in_harness_registry",
    # Rule 38 — panel tree the pipe-call walker covers
    "Core/GUI/Frontend/Panels": "panel_verbs_exist_in_harness_registry",
}


def check_rule_targets_exist() -> List[Finding]:
    """Every path in :data:`RULE_TARGET_PATHS` must exist.

    Fires an ``error`` per missing path, naming the rule(s) that just
    went dead.  This is the generalization of the #101 and #116 fixes:
    the next time a refactor deletes or moves a file a rule depends on,
    the PR that does it goes red, rather than the rule going quiet.

    If a path here is *intentionally* removed, the fix is to repoint or
    retire the owning rule and update this table in the same commit —
    which is precisely the step both prior incidents skipped.
    """
    out: List[Finding] = []
    for rel, owner in sorted(RULE_TARGET_PATHS.items()):
        if (_pkg.WYLDE_ROOT / rel).exists():
            continue
        out.append(
            Finding(
                rule="rule_targets_exist",
                severity="error",
                file=rel,
                line=0,
                message=(
                    f"Rule target {rel!r} does not exist, so {owner} can no "
                    f"longer do its job — and a rule that cannot inspect its "
                    f"target reports a pass, not a failure.  Repoint or "
                    f"retire that rule and update RULE_TARGET_PATHS in "
                    f"wylde_check/rules/_selfcheck.py in the same change."
                ),
            )
        )
    return out
