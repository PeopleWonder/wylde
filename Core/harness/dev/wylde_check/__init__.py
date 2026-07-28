"""Wylde architectural checker.

Encodes Wylde-specific contracts as thirty active rules.  Each rule
walks the active tree (skipping `_legacy/`, `__pycache__/`, build
output, etc.) and emits structured findings.  Pure-Python, no
subprocesses, no network — runs purely off the filesystem.

Numbering note (slice-11 cutover, 2026-05-29): rules 7, 9, 11 and 30
were RETIRED when the Svelte (`Core/GUI/src/`) and Tauri
(`Core/GUI/src-tauri/`) trees were deleted — they keyed on
Svelte/Tauri-shaped source, or were subsumed by the gpui contract rules
(38/41).  Rules 44-47 were added in the same slice.  The original
numbers are kept for the surviving rules so cross-references in the doc
and git history stay stable; the dispatcher holds 47 active rules
(39 surviving + 4 new at slice-11 + rules 48-51 added across the
2026-05-30/31 audit, egress, bare-tokio and cold-start-crash slices).

Dead-rule retirement (2026-07-20): 22 further rules were retired, taking
the dispatcher from 52 to 30.  Fifteen were structurally dead — their
target tree (the Python service folders, ``Core/Lifecycle/daemon_state``,
``Gateway/routes``, ``Extensions/``, the tool/action manifests) was
deleted in the Rust cutover, so they walked nothing and could only ever
report a pass.  Seven were Python-only rules with no production Python
left to walk.  As with the slice-11 retirements the original numbers are
kept in the catalog below, marked RETIRED in place.  In the same pass
rule 20 (``file_size_limit``) was REPOINTED from Python to Rust, and
rules 34 / 36 were NARROWED to drop their Svelte / ``Extensions/`` halves.

The rules:

1. ``no_internal_http`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
2. ``manifest_paths`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
3. ``tool_id_regex`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
4. ``action_registry`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
5. ``import_paths`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
6. ``dead_service_refs``  — known-dead service names appearing in
                            active code.
7. ``inferencebar_purity`` — RETIRED (slice-11 cutover): keyed on
                            ``InferenceBar.svelte``; the Svelte tree is gone.
8. ``gateway_scope`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
9. ``gui_action_contract`` — RETIRED (slice-11 cutover): keyed on Svelte
                            ``pipeAction(SVC_X, …)`` callsites; subsumed by
                            rule 38 (``panel_verbs_exist_in_harness_registry``).
10. ``gui_no_backend_bypass`` — the GUI must not read or write
                            backend-owned storage paths (or service
                            ``manifest.json`` files) directly.  Repointed
                            at the slice-11 cutover from the deleted Svelte
                            ``src/`` + Tauri ``src-tauri/src/`` trees to the
                            gpui panel + shell Rust source
                            (``Core/GUI/Frontend`` + ``Core/GUI/Shell``).
11. ``gui_pipe_constants`` — RETIRED (slice-11 cutover): keyed on
                            ``src/lib/api.js`` ``SVC_*`` JS constants;
                            subsumed by the gpui contract rules (38/41).
12. ``tool_docstring_required`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
13. ``logging_setup_only`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
14. ``no_external_subprocess`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
15. ``spawn_paths_exist`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
16. ``run_py_entry_point`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
17. ``pipe_name_convention`` — every Windows named-pipe ``wylde-<name>``
                            literal in active code matches the regex
                            ``^wylde-[a-z][a-z0-9-]*$``.
18. ``run_py_startup_sequence`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
19. ``shutdown_handler_marks_stopped`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
20. ``file_size_limit``   — flat 700-LOC cap on active Rust files
                            (``rust/crates/*/src/**`` + ``Core/GUI/**``,
                            excluding ``target/``).  REPOINTED from
                            Python on 2026-07-20; the 91 files that were
                            already over-cap at that moment are recorded
                            as queued debt in
                            ``rules/_quality._FILE_SIZE_QUEUED_SPLITS``
                            so the cap engages on every new or newly
                            grown file.  Files past the cap are split
                            along their natural seams.
21. ``test_init_present`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
22. ``memory_layer_boundaries`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
23. ``action_docstring_required`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
24. ``no_bare_except`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
25. ``service_owns_its_state`` — a service only reads/writes paths
                            inside its own data directory; cross-
                            service state access goes via pipe action,
                            not the filesystem.
26. ``import_paths_rust`` — Rust crates may only depend on each other
                            via ``wylde-shared``; deep ``super::super::``
                            chains are flagged as a sign the module
                            graph is wrong.  TX S4 exemptions: the
                            Core-plugin SDK ``wylde_plugin_api`` is
                            importable everywhere (shared surface like
                            wylde-shared); ``wylde_plugin_*`` crates
                            are importable from ``wylde-harness`` only
                            (the plugin host).  The plugin crates
                            themselves live at ``Core/Plugins/`` and
                            are outside this rule's walk.
27. ``no_silent_error_swallow_rust`` — ``let _ = <result>;`` and
                            trailing ``.ok();`` patterns that drop a
                            Result without logging are flagged.  An
                            inline ``// wylde-check: discard-result-ok``
                            marker suppresses deliberate discards.
28. ``logging_setup_only_rust`` — only
                            ``wylde_shared::logging::configure_logging``
                            may build / initialise the tracing
                            subscriber; every other crate calls
                            ``configure_logging`` and inherits the
                            canonical format.
29. ``no_external_process_spawn_rust`` —
                            ``std::process::Command::new`` and
                            ``tokio::process::Command::new`` are
                            restricted to the ``wylde-lifecycle`` crate.
30. ``gui_error_reporting`` — RETIRED (slice-11 cutover): keyed on Svelte
                            ``console.error`` / ``toast.error`` sinks; the
                            gpui panels surface errors as ``Result`` state,
                            a different shape.  A gpui-era error-reporting
                            rule is a possible post-alpha addition.
31. ``shutdown_reaps_manifest_orphans`` — the canonical
                            ``stop_all_daemon_managed`` in
                            ``Core/Lifecycle/daemon_state/__init__.py``
                            must invoke a manifest-walking orphan reaper
                            (call name matches ``reap*orphan*``).  Without
                            it, services orphaned by a prior daemon crash
                            survive every shutdown — their PID is alive
                            but the daemon's in-memory Popen slots are
                            None, and the periodic sweep only acts on
                            dead PIDs.
32. ``manifest_sandbox_required`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
33. ``no_cross_panel_imports`` — a ``wylde-panel-*`` crate's
                            ``Cargo.toml`` may only depend on the
                            shared-infra crates (``wylde-theme`` /
                            ``wylde-gui-pipe`` / ``wylde-gpui-input`` /
                            ``wylde-panel-registry``).  Direct panel-to-
                            panel imports would build a coupling graph
                            that breaks the "one panel per crate"
                            boundary the gpui workspace is built around.
34. ``no_legacy_gui_imports_in_panels`` — no ``tauri::*`` use paths
                            anywhere under
                            ``Core/GUI/Frontend/Panels/**``.  Panel
                            crates are gpui-native.  NARROWED
                            2026-07-20: the Svelte matcher was retired
                            (tree deleted at the slice-11 cutover; its
                            only surviving finding was a false positive
                            on a file-icon table row).
35. ``webview_only_in_extension_handlers`` — ``wry::*`` imports are
                            reserved for the ``wylde-webview`` crate at
                            ``Core/GUI/Frontend/Extension_handlers/WebView/``.
                            WebView exists to host iframe-extension
                            panels; first-party panels must be native
                            gpui.
36. ``first_party_manifest_must_be_gpui_view`` — every
                            ``manifest.json`` under
                            ``Core/GUI/Frontend/Panels/**`` declares
                            ``source.kind == "gpui_view"`` for every
                            entry in its ``panels`` array.  NARROWED
                            2026-07-20: the symmetric
                            ``Extensions/<X>/`` ``ui_panels`` half was
                            retired — ``Extensions/`` no longer exists,
                            so that walk found nothing and could only
                            report a pass.
37. ``panel_crate_must_be_workspace_member`` — every
                            ``Core/GUI/Frontend/Panels/*/Cargo.toml``
                            on disk must appear in the ``members = [...]``
                            array of ``Core/GUI/Cargo.toml``, and vice
                            versa.  Either-direction failures either
                            skip the crate at build time or make
                            ``cargo metadata`` refuse the workspace.
38. ``panel_verbs_exist_in_harness_registry`` — every panel-side
                            ``pipe::call`` / ``stream_call`` whose
                            service arg resolves to a Rust crate with
                            a discoverable action registry
                            (``wylde-harness``, ``wylde-extension-bridge``,
                            ``wylde-ollama``, ``wylde-voice``) must name
                            a verb actually
                            declared in that service's registry.
                            Catches typo'd or as-yet-unimplemented
                            verbs at edit time instead of as a runtime
                            ``no_action`` error.
39. ``nav_targets_exist`` — every literal-string ``request_nav("X")``
                            call (and every ``request_nav(IDENT)``
                            site where ``IDENT`` resolves via a
                            file-local ``const IDENT: &str = "..."``)
                            must resolve to a panel actually declared
                            by some ``manifest.json`` under
                            ``Core/GUI/Frontend/Panels/**``.  Variable-
                            argument call sites whose value isn't a
                            const string are intentionally skipped.
40. ``required_services_includes_called_services`` — under-
                            declaration (ERROR) and over-declaration
                            (WARNING) both flagged.  An under-declared
                            manifest renders the panel half-broken
                            when the called service is down; an over-
                            declared one grays the panel out
                            unnecessarily when a service it doesn't
                            actually call is down.
41. ``rest_routes_exist_in_service`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
42. ``manifest_factory_resolves`` — every first-party panel
                            ``manifest.json``'s ``source.factory``
                            string (``<crate>::<...>::<fn>``) must
                            resolve to a workspace-member crate and a
                            ``pub fn <fn>`` that exists in that crate's
                            source.  Catches deleted/renamed factory
                            entry points at edit time so the panel-
                            registry aggregator doesn't blow up at
                            build with an opaque link error.
43. ``stream_call_must_handle_cancel`` — every
                            ``wylde_gui_pipe::stream_call(...)`` site
                            under ``Core/GUI/Frontend/Panels/**`` must
                            retain the returned ``PipeStream`` (via
                            ``let stream = ...``, ``self.stream =
                            Some(...)``, ``?`` propagation, ``return``,
                            or trailing-expression position).  Naked
                            ``let _ = stream_call(...)`` or
                            ``stream_call(...);`` drops the cancel
                            handle immediately and the stream never
                            delivers a frame.  Inline marker
                            ``// wylde-check: stream-discard-ok``
                            opts a single site out.
44. ``launcher_enumerates_services_from_manifests`` — the launcher
                            (``Core/Lifecycle/launcher.py``) must build its
                            service set from the filesystem registry
                            (``services.yaml`` + per-service
                            ``manifest.json``): it must reference a
                            manifest/registry loader AND must not assign a
                            module-level UPPERCASE ``SERVICES`` list literal.
                            The Rust lifecycle crate is held to the
                            no-hardcoded-``const SERVICES`` half — its
                            tier=core ``start_<name>`` sequence is bespoke
                            bring-up by design, not a data-driven roster.
45. ``shutdown_enumerates_services_from_manifests`` —
                            ``Core/Lifecycle/shutdown.py::shutdown_all`` must
                            drain in a manifest-driven order (reverse-launch
                            default + ``shutdown_order`` override), not a
                            hardcoded list; the gpui ``shutdown.rs`` must
                            delegate to that drain via
                            ``lifecycle.shutdown_all`` (its image-name
                            hard-kill fallback is a recognised last resort,
                            not the enumeration).
46. ``every_service_has_manifest`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
47. ``service_manifest_schema`` — RETIRED (2026-07-20 dead-rule retirement): target tree
                            deleted in the Rust cutover.
48. ``gateway_verbs_exist_in_harness_registry`` — the outbound
                            companion to rule 38.  Every harness-pipe
                            verb the Gateway crate dispatches
                            (``harness_dispatch("verb", ...)`` or
                            ``pipe_action("wylde-harness", "verb",
                            ...)`` under
                            ``rust/crates/wylde-gateway/src/**``) must
                            appear in the harness registry — the same
                            union of Rust ``ALL_PIPE_ACTIONS`` and
                            Python ``_ACTIONS`` rule 38 uses.  An
                            unregistered verb is a latent runtime
                            ``no_action`` on that REST route; the rule
                            catches it at edit time.  Dynamic-verb
                            dispatches are skipped; a deliberate
                            optional-verb probe opts out with an inline
                            ``// wylde-check: optional-verb`` marker.
49. ``no_python_gateway_imports`` — RETIRED (2026-07-20): Python-only rule, no production
                            Python remains.
50. ``no_bare_tokio_in_panel_src`` — bare tokio primitives
                            (spawn / timer / runtime ctor) in a gpui
                            panel ``src`` panic at startup (no reactor;
                            chat_panel.rs:544); details in
                            docs/wylde_check_rules.md.
51. ``no_panic_in_panel_render`` — panic primitives
                            (``.unwrap()`` / ``.expect(`` / ``unreachable!``
                            / ``todo!`` / ``panic!(``) in a gpui panel
                            ``src`` take down the whole shell (panels share
                            the event loop; Dashboard/src/lib.rs cold-start
                            crash); details in docs/wylde_check_rules.md.
52. ``silent_skip_in_service_start`` — every ``start_[a-z_]+`` function
                            in
                            ``rust/crates/wylde-lifecycle/src/state/services.rs``
                            must log a reason inside every early
                            ``return Ok(...)`` branch (a ``tracing::`` call in
                            the enclosing block).  A silent skip leaves the
                            daemon dark about WHY a service didn't spawn — the
                            stale-manifest outage that left five services
                            down on 2026-05-31.  The successful-spawn tail
                            (``Ok(())`` expression after ``record_spawn``) is
                            never flagged.  Opt out with
                            ``// wylde-check: silent-skip-allowed`` (rare);
                            details in docs/wylde_check_rules.md.
53. ``no_hardcoded_prompts_rust`` — ``"You are ...`` LLM system-prompt
                            string literals in Rust source must live in
                            the prompts catalog
                            (``prompts/catalog.json`` +
                            ``store::effective_prompt``), not as
                            hardcoded constants (prompt-engineering B11,
                            2026-06-11).  Grandfather allowlist
                            (``rules/_prompts.py``) covers the pre-B9
                            sites and empties when B9 migrates them.
                            Test fixtures opt out with
                            ``// wylde-check: prompt-literal-ok``.
55. ``no_personal_identifiers`` — this repo is public.  Flags (A) real
                            home-directory paths (``C:\\Users\\<x>``,
                            ``/home/<x>``, ``/Users/<x>``) whose segment
                            is not a recognised placeholder, and (B) the
                            maintainer's name, matched as **salted
                            SHA-256 digests** so the rule does not itself
                            carry the name it removes.  Added 2026-07-19
                            after the 2026-05-31 hand scrub — which
                            recorded "0 remaining" — drifted back to ~175
                            name occurrences and 11 personal paths in
                            seven weeks, with nothing failing in between.
                            Opt out with
                            ``wylde-check: personal-identifier-ok``.
56. ``graph_test_serialized_on_db_lock`` — every Rust integration-test
                            binary (``rust/crates/**/tests/*.rs``) with two or
                            more *live-graph* tests (a ``#[test]`` /
                            ``#[tokio::test]`` that is also ``#[ignore]``d with
                            a reason naming ``bolt://`` / Neo4j / Memgraph)
                            must (a) acquire a per-test ``DB_LOCK`` in every
                            such test body — directly
                            (``DB_LOCK.lock().await``) or via a same-file
                            ``db_guard()`` helper — and (b) be run in the
                            live-graph leg of ``.github/workflows/ci.yml`` (a
                            ``--test <stem> … --ignored`` invocation).  The
                            self-collision class (#83) recurred three times
                            (#216/#227) because the lock was a convention a
                            reviewer had to remember and CI ran these
                            ``#[ignore]``d tests only in a dedicated job; a
                            binary added later without the lock — or one that
                            holds the lock but isn't in the leg — now turns the
                            build red.  Every live-graph binary in the tree
                            reaches the graph over Bolt, which the leg stands up.
                            Single-test live-graph binaries can't self-collide
                            and are out of scope.  Findings carry a pointer to
                            the #83 class's diagnosis home,
                            docs/trackers/self-collision-class.md, when that
                            doc is present — a SELF-EXPIRING tracker, so the
                            pointer is presence-gated via
                            ``rules._tracker_ref.tracker_pointer`` and simply
                            goes quiet once the doc is auto-deleted (#253).
                            Details in docs/wylde_check_rules.md.
58. ``chat_surfaces_are_e2e_covered`` — every GUI chat entry point must be
                            driven by the all-surfaces chat-turn e2e
                            (``Core/GUI/Frontend/Panels/Chat/tests/
                            chat_turn_e2e.rs``, #236).  Two checks the Rust
                            compiler cannot make: (a) every ``ChatScope``
                            variant appears in the test's ``COVERED`` list —
                            an arm can be added to the exhaustive ``spec()``
                            match without ever being driven; and (b) every
                            *send-capable chat composer* in the GUI tree (a
                            ``SubmitMode::EnterSubmits`` input in a file that
                            also reaches the turn path) is declared in
                            ``COVERED_COMPOSER_FILES`` — a new panel growing
                            its own chat bar adds no ``ChatScope`` variant, so
                            the match is blind to it.  Chat is the product's
                            primary path; a new place to type into it must be
                            proven end-to-end before it ships, not covered by
                            a percentage that quietly drops.  Details in
                            docs/wylde_check_rules.md.

All rules are advisory.  The checker returns an envelope; nothing here
mutates state.

This module is a thin re-export shim over the split package: rules live
in :mod:`wylde_check.rules.*`, walk helpers in :mod:`wylde_check._walkers`,
constants in :mod:`wylde_check._config`, per-file (lint-hook) helpers in
:mod:`wylde_check._single_file`.  ``WYLDE_ROOT`` is defined here so the
test suite's ``monkeypatch.setattr(wc, "WYLDE_ROOT", tmp_path)`` flows
through every rule call (each submodule reads it via the package module).
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional


WYLDE_ROOT: Path = Path(__file__).resolve().parents[4]


@dataclass
class Finding:
    rule: str
    severity: str  # "error" | "warning" | "info"
    file: str  # relative to WYLDE_ROOT, forward slashes
    line: int  # 1-based; 0 means file-level
    message: str
    context: str = ""  # excerpted source line

    def as_dict(self) -> Dict[str, Any]:
        return {
            "rule": self.rule,
            "severity": self.severity,
            "file": self.file,
            "line": self.line,
            "message": self.message,
            "context": self.context,
        }


# Re-export walk helpers so callers that imported them from the old
# single-file module continue to work.
from ._walkers import (  # noqa: E402, F401
    _is_excluded,
    _is_test_path,
    _read_text,
    _to_rel,
    _walk,
)

# Re-export every rule function.  Imports happen after Finding/WYLDE_ROOT
# are defined above so the submodules can resolve them on load.
from .rules._arch import (  # noqa: E402
    check_dead_service_refs,
    check_service_owns_its_state,
)
from .rules._gui import (  # noqa: E402
    check_gui_no_backend_bypass,
)
from .rules._runtime import (  # noqa: E402
    check_pipe_name_convention,
    check_shutdown_reaps_manifest_orphans,
)
from .rules._quality import (  # noqa: E402
    check_file_size_limit,
)
from .rules._rust import (  # noqa: E402
    check_import_paths_rust,
    check_logging_setup_only_rust,
    check_no_external_process_spawn_rust,
    check_no_silent_error_swallow_rust,
    check_no_unbounded_log_sink_rust,
)
from .rules._gpui import (  # noqa: E402
    check_first_party_manifest_must_be_gpui_view,
    check_no_cross_panel_imports,
    check_no_legacy_gui_imports_in_panels,
    check_webview_only_in_extension_handlers,
)
from .rules._gpui_workspace import (  # noqa: E402
    check_panel_crate_must_be_workspace_member,
)
from .rules._gpui_contract import (  # noqa: E402
    check_panel_verbs_exist_in_harness_registry,
    check_required_services_includes_called_services,
)
from .rules._gpui_availability import (  # noqa: E402
    check_service_backed_surface_declares_availability,
)
from .rules._gpui_nav import (  # noqa: E402
    check_nav_targets_exist,
)
from .rules._gpui_polish import (  # noqa: E402
    check_manifest_factory_resolves,
    check_stream_call_must_handle_cancel,
)
from .rules._lifecycle import (  # noqa: E402
    check_launcher_enumerates_services_from_manifests,
    check_shutdown_enumerates_services_from_manifests,
)
from .rules._gateway_contract import (  # noqa: E402
    check_gateway_verbs_exist_in_harness_registry,
)
from .rules._no_bare_tokio import (  # noqa: E402
    check_no_bare_tokio_in_panel_src,
)
from .rules._no_panic_in_panel_render import check_no_panic_in_panel_render  # noqa: E402
from .rules._prompts import (  # noqa: E402
    check_no_hardcoded_prompts_rust,
)
from .rules._silent_skip_in_service_start import (  # noqa: E402
    check_silent_skip_in_service_start,
)
from .rules._personal_identifiers import (  # noqa: E402
    check_no_personal_identifiers,
)
from .rules._graph_test_isolation import (  # noqa: E402
    check_graph_test_serialized_on_db_lock,
)
from .rules._chat_surface_coverage import (  # noqa: E402
    check_chat_surfaces_are_e2e_covered,
)
from .rules._global_bus_test_isolation import (  # noqa: E402
    check_global_bus_test_isolation,
)
from .rules._control_functionality import (  # noqa: E402
    check_gui_controls_are_wired_and_walkable,
    check_every_control_building_crate_is_walked,
)
from .rules._selfcheck import check_rule_targets_exist  # noqa: E402
from .rules._dependency_spread import (  # noqa: E402
    check_dependency_spread_ratchet,
)
from ._single_file import (  # noqa: E402
    _check_dead_refs_lines,
    _check_pipe_name_convention_lines,
)


# ── Top-level dispatcher ──────────────────────────────────────────────


_RULES: Dict[str, Callable[[], List[Finding]]] = {
    "dead_service_refs": check_dead_service_refs,
    # rule 7 (inferencebar_purity) retired at the slice-11 cutover — Svelte gone.
    # rule 9 (gui_action_contract) retired — subsumed by panel_verbs_exist_in_harness_registry.
    "gui_no_backend_bypass": check_gui_no_backend_bypass,
    # rule 11 (gui_pipe_constants) retired — subsumed by the gpui contract rules.
    "pipe_name_convention": check_pipe_name_convention,
    "shutdown_reaps_manifest_orphans": check_shutdown_reaps_manifest_orphans,
    "file_size_limit": check_file_size_limit,
    "service_owns_its_state": check_service_owns_its_state,
    "import_paths_rust": check_import_paths_rust,
    "no_silent_error_swallow_rust": check_no_silent_error_swallow_rust,
    "logging_setup_only_rust": check_logging_setup_only_rust,
    "no_external_process_spawn_rust": check_no_external_process_spawn_rust,
    # rule 30 (gui_error_reporting) retired at the slice-11 cutover — keyed on
    # Svelte console.error/toast; gpui panels surface errors as Result state.
    "no_cross_panel_imports": check_no_cross_panel_imports,
    "no_legacy_gui_imports_in_panels": check_no_legacy_gui_imports_in_panels,
    "webview_only_in_extension_handlers": check_webview_only_in_extension_handlers,
    "first_party_manifest_must_be_gpui_view": check_first_party_manifest_must_be_gpui_view,
    "panel_crate_must_be_workspace_member": check_panel_crate_must_be_workspace_member,
    "panel_verbs_exist_in_harness_registry": check_panel_verbs_exist_in_harness_registry,
    "nav_targets_exist": check_nav_targets_exist,
    "required_services_includes_called_services": check_required_services_includes_called_services,
    "service_backed_surface_declares_availability": (
        check_service_backed_surface_declares_availability
    ),
    "manifest_factory_resolves": check_manifest_factory_resolves,
    "stream_call_must_handle_cancel": check_stream_call_must_handle_cancel,
    # Rules 44-45 — launcher / shutdown correctness (slice-11 cutover).
    # Enforce the filesystem-as-registry contract.  (Rules 46/47 retired
    # 2026-07-20 — the top-level Python service folders they discovered
    # were deleted in the Rust cutover.)
    "launcher_enumerates_services_from_manifests": check_launcher_enumerates_services_from_manifests,
    "shutdown_enumerates_services_from_manifests": check_shutdown_enumerates_services_from_manifests,
    # Rule 48 — Gateway→harness dispatch contract (codebase-audit slice,
    # 2026-05-30).  The outbound companion to rule 38's inbound (panel→
    # harness) check.
    "gateway_verbs_exist_in_harness_registry": check_gateway_verbs_exist_in_harness_registry,
    "no_bare_tokio_in_panel_src": check_no_bare_tokio_in_panel_src,
    # Rule 51 — panic primitives in a gpui panel render path (Dashboard
    # cold-start crash slice, 2026-05-31); panels share the event loop.
    "no_panic_in_panel_render": check_no_panic_in_panel_render,
    # Rule 52 — silent early-returns in lifecycle start_<service> functions
    # (silent-skip slice, 2026-05-31); a skipped spawn must log its reason.
    "silent_skip_in_service_start": check_silent_skip_in_service_start,
    # Rule 53 — hardcoded LLM system-prompt literals in Rust source
    # (prompt-engineering B11 slice, 2026-06-11); prompts belong in the
    # catalog so the shipped override surface can tune them. Grandfather
    # allowlist empties at B9.
    "no_hardcoded_prompts_rust": check_no_hardcoded_prompts_rust,
    # Rule 54 — every persistent file log must inherit the shared
    # rotation policy (0.2 Stability audit finding C, #98).  Flags a raw
    # append-only `OpenOptions` outside the canonical rotation factory —
    # the ad-hoc uncapped sink that let `ipc.jsonl` grow to ~179 MB.
    "no_unbounded_log_sink_rust": check_no_unbounded_log_sink_rust,
    # Rule 55 — personal identifiers in a public repo (scrub-drift slice,
    # 2026-07-19). The 2026-05-31 scrub drove the maintainer's name and
    # home paths to zero by hand; seven weeks later both had regrown,
    # because nothing failed in between. Name tokens are matched as
    # salted digests so this rule is not itself the leak.
    "no_personal_identifiers": check_no_personal_identifiers,
    # Rule 56 — multi-test bolt:// binaries must serialize each test on a
    # DB_LOCK and be run in the live-graph CI leg (0.2 Stability, #226). The
    # #83 self-collision class recurred three times because the lock was an
    # unenforced convention; this makes it structural.
    "graph_test_serialized_on_db_lock": check_graph_test_serialized_on_db_lock,
    # Rule 57 — every GUI chat entry point is driven by the all-surfaces
    # chat-turn e2e (#236). The exhaustive ChatScope match in that test is
    # the compile-time half; this catches the two cases it cannot see —
    # an arm added but never driven, and a brand-new chat bar elsewhere.
    "chat_surfaces_are_e2e_covered": check_chat_surfaces_are_e2e_covered,
    # Rule 59 — every interactive GUI control is wired and walkable (#247).
    # The static half of the control-functionality gate: a dead handler
    # body, and an interactive site that bypasses `controls::control()` and
    # so never enters the per-frame registry the control walk enumerates.
    # Error, with a per-file grandfather ratchet over the 140 pre-existing
    # sites: this job fails on any finding, so WARN would red develop too.
    "gui_controls_are_wired_and_walkable": check_gui_controls_are_wired_and_walkable,
    # Rule 61 — rule 59's companion and the other half of #247. Rule 59 proves
    # every control *site* routes through `control()`; this proves the *walk
    # exists and sees every control-building file*: a GUI crate whose shipped
    # src builds a control must have a control_walk declaring every such file in
    # `.sources()`. Together they mean a control can neither bypass the registry
    # nor sit in a file no walk's coverage assertion inspects. Added with the
    # deletion of rule 59's grandfather ratchet (the migration is complete).
    "every_control_building_crate_is_walked": check_every_control_building_crate_is_walked,
    # Rule 60 — a unit test touching a process-global broadcast bus must own
    # its channel or serialize on a test-module guard (#246). The other half
    # of rule 56's #83 self-collision class: same hazard, but in a `src/`
    # unit-test module rather than a `tests/` binary, and with no
    # minimum-count carve-out — #246 had exactly ONE bus-touching test, and
    # its colliders never mentioned the bus at all.
    "global_bus_test_isolation": check_global_bus_test_isolation,
    "rule_targets_exist": check_rule_targets_exist,
    # Rule 59 — dependency-spread ratchet (#290 dependency isolation). The
    # forward-looking half of #290: freezes each external dep's crate-spread
    # at today's baseline so unwrapped shotgun-risk (rand → 2 crates, axum →
    # 3) can't silently re-accumulate. Contained deps (rand, cpal) pinned to
    # their adapter's owning crate; reqwest (12) is the named watch target.
    "dependency_spread_ratchet": check_dependency_spread_ratchet,
}

# Asserting the count at import time so a future rule add/drop trips the
# import rather than going silently uncounted.  Slice-11 cutover churn:
# 43 active − 4 retired (7 inferencebar_purity, 9 gui_action_contract,
# 11 gui_pipe_constants, 30 gui_error_reporting; all Svelte/Tauri-shaped
# or subsumed by the gpui contract rules) + 4 new (44-47) = 43 active.
# Codebase-audit slice (2026-05-30): +1 (rule 48,
# gateway_verbs_exist_in_harness_registry) = 44 active.
# Egress-client relocation slice (2026-05-30): +1 (rule 49) = 45 active.
# Bare-tokio panel slice (2026-05-30): +1 (rule 50) = 46 active.
# Dashboard cold-start crash slice (2026-05-31): +1 (rule 51) = 47 active.
# Silent-skip-in-service-start slice (2026-05-31): +1 (rule 52) = 48 active.
# Prompt-engineering B11 slice (2026-06-11): +1 (rule 53,
# no_hardcoded_prompts_rust) = 49 active.
# 0.2 Stability audit finding C (#98, 2026-07-18): +1 (rule 54,
# no_unbounded_log_sink_rust) = 50 active.
# Enforcement audit (#116, 2026-07-19): +1 (rule 51, rule_targets_exist)
# = 51 active.  Meta-rule: asserts every other rule's target path still
# exists, so a refactor cannot silently disarm a gate again.
# Scrub-drift slice (2026-07-19): +1 (rule 55, no_personal_identifiers)
# = 52 active.  The public-repo personal-info guarantee, which had been
# a hand-audited number and drifted back to non-zero once already.
# Dead-rule retirement (2026-07-20): -22 = 30 active.  Fifteen rules were
# structurally dead (target tree deleted in the Rust cutover — they
# walked nothing and could only report a pass) and seven were Python-only
# rules with no production Python left.  52 - 22 = 30.
# 0.2 Stability enforcement (#226, 2026-07-22): +1 (rule 56,
# graph_test_serialized_on_db_lock) = 31 active.  Makes the shared-Neo4j
# per-test DB_LOCK + live-graph CI coverage a structural gate, so the #83
# self-collision class (which recurred three times) cannot recur silently.
# 0.2 Stability enforcement (#239, 2026-07-22): +1 (rule 57,
# service_backed_surface_declares_availability) = 32 active. Rule 40 gates a
# panel's dependence on services, but the unit that can be dead is the *item*,
# not the panel — Tools declared its bridge correctly and still rendered a card
# per extension pointing at a service nothing checked. This makes the per-item
# state a structural gate on both sides of the wire.
# All-surfaces chat-turn e2e (#236, 2026-07-22): +1 (rule 58,
# chat_surfaces_are_e2e_covered) = 33 active.  Chat is the primary path
# and has more than one entry point; this keeps a newly-added surface
# from shipping with no end-to-end proof that typing in it does anything.
# (Numbered 58, not 57: #239 landed its own rule 57 on develop first.)
# GUI control-functionality enforcement (#247, 2026-07-23): +1 (rule 59,
# gui_controls_are_wired_and_walkable) = 34 active.  Panel-walk proves a
# panel LOADS; nothing proved a control in it DOES anything.  Ships at
# Ships at error with a grandfather ratchet rather than at WARNING: the CI
# gate fails on any finding, warning included, so "warn for now" would red
# develop just as hard.  (Numbered 59, not 58: #236 landed 58 first.)
# Global-bus test isolation (#246, 2026-07-23): +1 (rule 60,
# global_bus_test_isolation) = 35 active.  The `src/`-unit-test half of the
# #83 self-collision class rule 56 covers for `tests/` binaries: a watcher
# test asserting on the first event off a process-global broadcast bus was
# really asserting that no sibling test published during its window, and
# failed ~17% of the time at --test-threads=8 on unrelated PRs.  No
# minimum-count carve-out, unlike rule 56 — #246 had exactly one
# bus-touching test and its colliders never named the bus.
# #247 endgame (2026-07-26): +1 (rule 61, every_control_building_crate_is_walked)
# = 36 active. Rule 59's companion: it makes a control_walk mandatory for every
# control-building GUI crate (declaring all its control sources), landed together
# with the deletion of rule 59's now-drained grandfather ratchet. Closes #247 —
# every panel is walked and the property is structurally enforced going forward.
# Dependency isolation (#290, 2026-07-28): +1 (rule 62, dependency_spread_ratchet)
# = 37 active. The forward-looking half of #290: freezes each external dep's
# crate-spread at today's baseline so unwrapped shotgun-risk (rand → 2 crates,
# axum → 3, before they were contained) cannot silently re-accumulate. reqwest
# (12 crates) is the named watch target.
assert len(_RULES) == 37, f"_RULES dispatcher size drifted: {len(_RULES)} (expected 37)"


def run_all(only: Optional[List[str]] = None) -> Dict[str, Any]:
    """Run every rule (or the subset named in ``only``).

    Returns the standard envelope ``{ok, data: {findings, summary}}``.
    Never raises — a broken rule emits an error-level finding pointing
    at the checker itself.
    """
    selected = list(_RULES.keys()) if only is None else [r for r in only if r in _RULES]
    findings: List[Finding] = []
    by_rule: Dict[str, int] = {r: 0 for r in selected}
    for rule_name in selected:
        fn = _RULES[rule_name]
        try:
            rule_findings = fn()
        except Exception as exc:  # noqa: BLE001
            rule_findings = [
                Finding(
                    rule=rule_name,
                    severity="error",
                    file="Core/harness/dev/wylde_check/__init__.py",
                    line=0,
                    message=f"rule {rule_name!r} raised {type(exc).__name__}: {exc}",
                )
            ]
        by_rule[rule_name] = len(rule_findings)
        findings.extend(rule_findings)

    errors = sum(1 for f in findings if f.severity == "error")
    warnings = sum(1 for f in findings if f.severity == "warning")
    infos = sum(1 for f in findings if f.severity == "info")

    return {
        "ok": True,
        "data": {
            "rules_checked": len(selected),
            "findings": [f.as_dict() for f in findings],
            "summary": {
                "by_rule": by_rule,
                "by_severity": {
                    "error": errors,
                    "warning": warnings,
                    "info": infos,
                },
                "total": len(findings),
            },
        },
    }


# ── Single-file checker (for pre-write hooks) ─────────────────────────


def check_one_file(rel_path: str, content: str) -> Dict[str, Any]:
    """Run the rules applicable to a single (path, content) pair.

    Used by pre-write hooks — the architectural rules that don't need
    the full tree all reduce cleanly to a per-file check.  Rules that
    DO need cross-file state (gui_*, the gpui contract rules, the
    lifecycle rules) are skipped here — the full ``run_all()`` catches
    those.

    Returns the canonical envelope shape.
    """
    if not isinstance(rel_path, str) or not rel_path:
        return {
            "ok": False,
            "data": {
                "findings": [],
                "summary": {
                    "total": 0,
                    "by_severity": {"error": 0, "warning": 0, "info": 0},
                    "by_rule": {},
                },
            },
            "error": {"code": "bad_request", "message": "rel_path required"},
        }
    if content is None:
        content = ""
    # Normalise to forward slashes for consistent exemption matching.
    rel_path = rel_path.replace("\\", "/")

    findings: List[Finding] = []
    findings.extend(_check_dead_refs_lines(rel_path, content))
    findings.extend(_check_pipe_name_convention_lines(rel_path, content))

    by_rule: Dict[str, int] = {}
    for f in findings:
        by_rule[f.rule] = by_rule.get(f.rule, 0) + 1
    by_sev = {"error": 0, "warning": 0, "info": 0}
    for f in findings:
        if f.severity in by_sev:
            by_sev[f.severity] += 1

    return {
        "ok": True,
        "data": {
            "findings": [f.as_dict() for f in findings],
            "summary": {
                "total": len(findings),
                "by_severity": by_sev,
                "by_rule": by_rule,
            },
        },
    }


__all__ = [
    "Finding",
    "WYLDE_ROOT",
    "run_all",
    "check_one_file",
    "check_dead_service_refs",
    "check_gui_no_backend_bypass",
    "check_pipe_name_convention",
    "check_shutdown_reaps_manifest_orphans",
    "check_file_size_limit",
    "check_service_owns_its_state",
    "check_import_paths_rust",
    "check_no_silent_error_swallow_rust",
    "check_logging_setup_only_rust",
    "check_no_external_process_spawn_rust",
    "check_no_unbounded_log_sink_rust",
    "check_no_cross_panel_imports",
    "check_no_legacy_gui_imports_in_panels",
    "check_webview_only_in_extension_handlers",
    "check_first_party_manifest_must_be_gpui_view",
    "check_panel_crate_must_be_workspace_member",
    "check_panel_verbs_exist_in_harness_registry",
    "check_nav_targets_exist",
    "check_required_services_includes_called_services",
    "check_service_backed_surface_declares_availability",
    "check_manifest_factory_resolves",
    "check_stream_call_must_handle_cancel",
    "check_launcher_enumerates_services_from_manifests",
    "check_shutdown_enumerates_services_from_manifests",
    "check_gateway_verbs_exist_in_harness_registry",
    "check_no_bare_tokio_in_panel_src",
    "check_no_panic_in_panel_render",
    "check_rule_targets_exist",
    "check_dependency_spread_ratchet",
    "check_silent_skip_in_service_start",
    "check_no_hardcoded_prompts_rust",
    "check_graph_test_serialized_on_db_lock",
    "check_chat_surfaces_are_e2e_covered",
    "check_global_bus_test_isolation",
]
