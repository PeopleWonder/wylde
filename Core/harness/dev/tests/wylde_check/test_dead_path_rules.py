"""Regression suite for issue #116 — rules that could not fire.

Every test here is a **negative** test: it breaks a guarantee and
asserts the owning rule reports an ``error``.  Each one is annotated
with what it did *before* the #116 fix, because that is the whole
point — all of these passed green against a broken tree.

The bug class: a rule loads a registry or opens a target file, gets
nothing back because the path no longer exists, and ``return out``s an
empty findings list.  An empty findings list is indistinguishable from
a clean bill of health.  The rule reports success for having checked
nothing.

This happened to rules 44/45 (issue #101, deleted
``Core/Lifecycle/launcher.py``) and then again to rules 38/48 (this
issue, ``src/pipe.rs`` → ``src/pipe/mod.rs`` plus the deleted Python
half).  Rule 51 (``rule_targets_exist``) is the generalization that
should stop a third occurrence.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write

_PIPE = "rust/crates/wylde-harness/src/pipe/mod.rs"
_GATEWAY = "rust/crates/wylde-gateway/src/routes/workspaces.rs"
_PANEL = "Core/GUI/Frontend/Panels/Chat/src/ipc.rs"
_DAEMON = "rust/crates/wylde-lifecycle/src/daemon.rs"
_SWEEP = "rust/crates/wylde-lifecycle/src/state/orphan_sweep.rs"
_DAEMON_MANAGED = "rust/crates/wylde-lifecycle/src/daemon_managed.rs"


def _write_registry(root: Any, *verbs: str) -> None:
    """A harness pipe registry declaring exactly ``verbs``."""
    body = "".join(f'    "{v}",\n' for v in verbs)
    _write(root / _PIPE, f"pub const ALL_PIPE_ACTIONS: &[&str] = &[\n{body}];\n")


def _errors(findings: list) -> list:
    return [f for f in findings if f.severity == "error"]


# ── Rule 48: gateway_verbs_exist_in_harness_registry ──────────────────


def test_gateway_rule_clean_when_verb_registered(isolated_tree: Any) -> None:
    """Control: a registered verb produces no findings."""
    wc, root = isolated_tree
    _write_registry(root, "workspaces.list_mru")
    _write(root / _GATEWAY, 'harness_dispatch("workspaces.list_mru", Value::Null).await\n')
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_gateway_rule_fires_on_unregistered_verb(isolated_tree: Any) -> None:
    """The rule's actual job: a dispatched verb absent from the registry
    is a latent runtime ``no_action`` on a live REST route."""
    wc, root = isolated_tree
    _write_registry(root, "conversations.list")
    _write(root / _GATEWAY, 'harness_dispatch("workspaces.list_mru", Value::Null).await\n')
    errs = _errors(wc.check_gateway_verbs_exist_in_harness_registry())
    assert len(errs) == 1
    assert "workspaces.list_mru" in errs[0].message


def test_gateway_rule_fires_when_registry_file_missing(isolated_tree: Any) -> None:
    """THE #116 REGRESSION.

    The registry file does not exist, so no verb can possibly be
    validated.  Before the fix this returned ``[]`` — a pass — while 46
    real Gateway verbs across 8 route files went unchecked.  It must now
    be an ``error``: a rule that cannot load its input has not passed,
    it has failed to run.
    """
    wc, root = isolated_tree
    # Deliberately do NOT write the registry.
    _write(root / _GATEWAY, 'harness_dispatch("workspaces.list_mru", Value::Null).await\n')
    errs = _errors(wc.check_gateway_verbs_exist_in_harness_registry())
    assert errs, "unloadable registry must fail, not pass vacuously"
    assert "not found" in errs[0].message


def test_gateway_rule_fires_when_registry_declares_no_verbs(isolated_tree: Any) -> None:
    """A registry file that exists but declares nothing is equally
    useless — an empty array must not read as 'nothing to check'."""
    wc, root = isolated_tree
    _write(root / _PIPE, "// registry moved elsewhere\n")
    _write(root / _GATEWAY, 'harness_dispatch("workspaces.list_mru", Value::Null).await\n')
    errs = _errors(wc.check_gateway_verbs_exist_in_harness_registry())
    assert errs
    assert "declares no verbs" in errs[0].message


# ── Rule 38: panel_verbs_exist_in_harness_registry ────────────────────


def test_panel_rule_fires_when_registry_file_missing(isolated_tree: Any) -> None:
    """Same regression on the panel side.  Before the fix, every
    ``wylde-harness`` panel verb hit ``if not registry: continue`` and
    was skipped — covering Settings (13 refs), Dashboard (5), Memory (3),
    Chat and Models."""
    wc, root = isolated_tree
    _write(root / _PANEL, 'wylde_gui_pipe::stream_call("wylde-harness", "conversations.get", args)\n')
    errs = _errors(wc.check_panel_verbs_exist_in_harness_registry())
    assert errs, "unloadable registry must fail, not skip every verb"


def test_panel_rule_fires_on_unregistered_verb(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write_registry(root, "conversations.list")
    _write(root / _PANEL, 'wylde_gui_pipe::stream_call("wylde-harness", "conversations.get", args)\n')
    errs = _errors(wc.check_panel_verbs_exist_in_harness_registry())
    assert errs
    assert "conversations.get" in errs[0].message


# ── Rule 31: shutdown_reaps_manifest_orphans (repointed) ──────────────


def _write_healthy_lifecycle(root: Any) -> None:
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write(
        root / _DAEMON,
        "let boot_sweep = crate::state::boot_orphan_sweep();\n"
        "start_gateway().await;\n",
    )


def test_orphan_rule_clean_on_healthy_tree(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write_healthy_lifecycle(root)
    assert wc.check_shutdown_reaps_manifest_orphans() == []


def test_orphan_rule_fires_when_daemon_missing(isolated_tree: Any) -> None:
    """Inverted guard (#101): a deleted target is the failure.

    This is the state ``develop`` is in TODAY for the pre-#116 rule,
    which pointed at ``Core/Lifecycle/daemon_state/__init__.py`` in a
    tree where ``Core/Lifecycle/`` has zero files.
    """
    wc, root = isolated_tree
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    errs = _errors(wc.check_shutdown_reaps_manifest_orphans())
    assert errs
    assert _DAEMON in errs[0].file


def test_orphan_rule_fires_when_sweep_never_called(isolated_tree: Any) -> None:
    """The guarantee itself: delete the boot sweep call and a service
    orphaned by an ungraceful exit stays dark across every restart."""
    wc, root = isolated_tree
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write(root / _DAEMON, "start_gateway().await;\n")
    errs = _errors(wc.check_shutdown_reaps_manifest_orphans())
    assert errs
    assert "never calls" in errs[0].message


def test_orphan_rule_not_satisfied_by_a_comment(isolated_tree: Any) -> None:
    """A doc comment naming the sweep must not stand in for calling it."""
    wc, root = isolated_tree
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write(
        root / _DAEMON,
        "// Phase 2b-sweep — boot_orphan_sweep() runs before any start_.\n"
        "start_gateway().await;\n",
    )
    errs = _errors(wc.check_shutdown_reaps_manifest_orphans())
    assert errs, "a comment mentioning the sweep is not a call to it"


def test_orphan_rule_fires_when_sweep_runs_after_first_start(isolated_tree: Any) -> None:
    """Ordering matters: sweeping after the first launch is too late —
    the launch already saw the stale alive-marked manifest."""
    wc, root = isolated_tree
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write(
        root / _DAEMON,
        "start_gateway().await;\nlet r = crate::state::boot_orphan_sweep();\n",
    )
    errs = _errors(wc.check_shutdown_reaps_manifest_orphans())
    assert errs
    assert "after the first service launch" in errs[0].message


def test_orphan_rule_rejects_stop_sweep_as_a_reap(isolated_tree: Any) -> None:
    """``stop_orphan_sweep()`` halts the recurring sweep; it performs no
    reap.  It matches the name pattern but not the guarantee, so it must
    not satisfy the rule — otherwise the teardown call in
    ``state/mod.rs`` would count as boot-time coverage."""
    wc, root = isolated_tree
    _write(root / _SWEEP, "pub fn boot_orphan_sweep() -> BootSweepReport {}\n")
    _write(root / _DAEMON, "stop_orphan_sweep();\nstart_gateway().await;\n")
    errs = _errors(wc.check_shutdown_reaps_manifest_orphans())
    assert errs, "halting the sweep is not performing one"


# ── Rules 44/45 residue: comment stripping ────────────────────────────


def test_boot_rule_not_satisfied_by_doc_comment_alone(isolated_tree: Any) -> None:
    """THE #116 RESIDUE.

    ``_require_token`` was a bare substring test.  Every token it looks
    for is also *named* in a doc comment beside the real call — so
    deleting the real ``boot_sequence()`` call at ``daemon.rs:187`` and
    leaving the comment at ``:180`` kept rule 44 green.  Before the fix
    this test passed (no findings); it must now fire.
    """
    wc, root = isolated_tree
    _write(root / _DAEMON_MANAGED, "pub const DAEMON_MANAGED: &[DaemonService] = &[];\n")
    _write(
        root / "rust/crates/wylde-lifecycle/src/state/mod.rs",
        "for svc in crate::daemon_managed::shutdown_sequence() {}\n",
    )
    # The call is GONE; only the doc comment that mentions it remains.
    _write(
        root / _DAEMON,
        "//! The boot order comes from crate::daemon_managed::boot_sequence().\n"
        "// see boot_sequence() for the ordering rationale\n"
        "let services = hardcoded_roster();\n",
    )
    errs = _errors(wc.check_launcher_enumerates_services_from_manifests())
    assert errs, "a comment mentioning boot_sequence() must not satisfy rule 44"


# ── Rule 51: rule_targets_exist (the generalization) ──────────────────


def test_selfcheck_fires_for_each_missing_target(isolated_tree: Any) -> None:
    """An empty tree means every rule target is missing — the self-check
    must name each one and the rule it just disarmed."""
    wc, _root = isolated_tree
    errs = _errors(wc.check_rule_targets_exist())
    from Core.harness.dev.wylde_check.rules._selfcheck import RULE_TARGET_PATHS

    assert len(errs) == len(RULE_TARGET_PATHS)
    assert all("can no longer do its job" in e.message for e in errs)


def test_selfcheck_clean_when_all_targets_present(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    from Core.harness.dev.wylde_check.rules._selfcheck import RULE_TARGET_PATHS

    for rel in RULE_TARGET_PATHS:
        p = root / rel
        if p.suffix:
            _write(p, "// present\n")
        else:
            p.mkdir(parents=True, exist_ok=True)
    assert wc.check_rule_targets_exist() == []
