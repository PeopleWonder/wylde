"""Wylde architectural checker.

Encodes Wylde-specific contracts as forty-eight active rules.  Each rule
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

The rules:

1. ``no_internal_http``   — Python/Svelte/JS files calling out to
                            Wylde-internal ports (8005, 7687, 8013,
                            5678, 8014, 8020, 11434) via HTTP libs.
                            Exemptions: ``Wylde/Gateway/**`` (Gateway IS
                            the legitimate HTTP boundary), Ollama client
                            (external), Memgraph Bolt driver (database
                            wire protocol).
2. ``manifest_paths``     — services that both have a daemon-managed
                            ``_start_X`` AND call ``write_manifest()``
                            in their own ``run.py`` (double-write).
3. ``tool_id_regex``      — ``tools/<group>/<id>/manifest.json`` must
                            have ``id`` / ``name`` matching the snake-
                            or-dotted regex.
4. ``action_registry``    — ``register_action()`` callsites: bare sanity
                            check that the registered names are
                            stringy + unique within a pipe module.
5. ``import_paths``       — bare ``Core.*`` is canonical; ``Wylde.Core.*``
                            in active code is flagged.  Tests are
                            exempt because they have try-fallback for
                            both forms.
6. ``dead_service_refs``  — known-dead service names appearing in
                            active code.
7. ``inferencebar_purity`` — RETIRED (slice-11 cutover): keyed on
                            ``InferenceBar.svelte``; the Svelte tree is gone.
8. ``gateway_scope``      — every Gateway route should fall into one of
                            the documented categories.
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
12. ``tool_docstring_required`` — every Python tool file under
                            ``Core/harness/tooling/tools/**/*.py`` must
                            have a non-empty top-level module docstring.
13. ``logging_setup_only`` — only ``Core/shared/logging_setup.configure_logging()``
                            should configure logging in active code.
14. ``no_external_subprocess`` — ``subprocess.Popen`` / ``.run`` /
                            ``.call`` / ``os.spawn*`` are restricted to
                            the Lifecycle daemon plus narrow exemptions.
15. ``spawn_paths_exist`` — every ``python -m <module>`` / script-path
                            spawn-command in
                            ``Core/Lifecycle/daemon_state.py`` resolves
                            to a real importable module or existing file.
16. ``run_py_entry_point`` — every top-level service folder uses
                            exactly ``run.py`` as its entry point.
17. ``pipe_name_convention`` — every Windows named-pipe ``wylde-<name>``
                            literal in active code matches the regex
                            ``^wylde-[a-z][a-z0-9-]*$``.
18. ``run_py_startup_sequence`` — every ``<Service>/run.py`` should call
                            ``configure_logging``, write a manifest,
                            start a heartbeat, then enter a serve loop.
19. ``shutdown_handler_marks_stopped`` — every ``<Service>/run.py``
                            should register a SIGTERM/SIGINT handler
                            whose body (or an ``atexit`` callback)
                            updates the manifest with a stopped state.
20. ``file_size_limit``   — flat 700-LOC cap on active Python files.
                            Files past the cap are split along their
                            natural seams.
21. ``test_init_present`` — every ``tests/`` folder under an active
                            root contains an ``__init__.py`` so pytest
                            rootdir discovery loads the right conftest.
22. ``memory_layer_boundaries`` — literal ``memory/<layer>/`` storage
                            paths only appear in code inside
                            ``Core/harness/memory/``; other callers
                            route through ``memory.*`` pipe actions.
23. ``action_docstring_required`` — every registered pipe-action
                            handler function carries a non-empty
                            docstring (≥15 chars).
24. ``no_bare_except``    — bare ``except:`` and silent-swallow
                            ``except Exception:`` blocks are flagged
                            in active code (tests exempt).
25. ``service_owns_its_state`` — a service only reads/writes paths
                            inside its own data directory; cross-
                            service state access goes via pipe action,
                            not the filesystem.
26. ``import_paths_rust`` — Rust crates may only depend on each other
                            via ``wylde-shared``; deep ``super::super::``
                            chains are flagged as a sign the module
                            graph is wrong.
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
32. ``manifest_sandbox_required`` — tests under
                            ``Core/Lifecycle/tests/`` and
                            ``Core/harness/tests/`` that touch the
                            manifest layer must sandbox ``_MANIFEST_DIR``
                            (either via ``monkeypatch.setattr`` in the
                            test itself or via an autouse fixture in a
                            sibling ``conftest.py``).  An unsandboxed
                            test can trip the reaper into killing real
                            wylde services — caught + fixed 2026-05-25
                            during Phase 11.B.
33. ``no_cross_panel_imports`` — a ``wylde-panel-*`` crate's
                            ``Cargo.toml`` may only depend on the
                            shared-infra crates (``wylde-theme`` /
                            ``wylde-gui-pipe`` / ``wylde-gpui-input`` /
                            ``wylde-panel-registry``).  Direct panel-to-
                            panel imports would build a coupling graph
                            that breaks the "one panel per crate"
                            boundary the gpui workspace is built around.
34. ``no_legacy_gui_imports_in_panels`` — no ``tauri::*`` use paths
                            and no Svelte references anywhere under
                            ``Core/GUI/Frontend/Panels/**``.  Panel
                            crates are gpui-native; the legacy
                            Tauri+Svelte tree stays in its own
                            standalone crate until cutover.
35. ``webview_only_in_extension_handlers`` — ``wry::*`` imports are
                            reserved for the ``wylde-webview`` crate at
                            ``Core/GUI/Frontend/Extension_handlers/WebView/``.
                            WebView exists to host iframe-extension
                            panels; first-party panels must be native
                            gpui.
36. ``first_party_manifest_must_be_gpui_view`` — two symmetric
                            kind-must-match-origin checks: every
                            ``manifest.json`` under
                            ``Core/GUI/Frontend/Panels/**`` declares
                            ``source.kind == "gpui_view"`` for every
                            entry in its ``panels`` array, AND every
                            ``Extensions/<X>/`` manifest's
                            ``ui_panels`` entry declares
                            ``source.kind == "iframe"``.  Extensions
                            can't ship a native gpui View; first-party
                            panels can't ship an iframe.
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
                            ``wylde-ollama``, ``wylde-trainer``,
                            ``wylde-voice``) must name a verb actually
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
41. ``rest_routes_exist_in_service`` — every literal-shape
                            ``wylde_gui_pipe::call(SVC, "METHOD",
                            "/api/...", ...)`` from a panel must
                            match a route in the destination service's
                            axum router (today: ``wylde-gateway``).
                            Path parameters (``:id``) match panel-side
                            wildcards (``{id}``).  Calls whose path or
                            method aren't literals are skipped — the
                            rule trades narrow scope for low false-
                            positive rate.  Action-envelope calls
                            (``POST /__action__``) are covered by rule
                            38 instead.
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
46. ``every_service_has_manifest`` — bidirectional at the launcher's
                            top-level discovery domain: a folder with a
                            ``run.py`` entry point must carry a
                            ``manifest.json``; and a runtime/archive dir
                            (``data``/``logs``/``docs``) must NOT carry a
                            service manifest (``Core`` is exempt — infra
                            rollup).
47. ``service_manifest_schema`` — every top-level service
                            ``manifest.json`` declares the required keys
                            (``name`` non-empty str, ``entry_point`` present
                            (str|null — the canonical launch command /
                            binary), ``shutdown_order`` int) with correct
                            types; ``depends_on`` / ``health_check`` / ``tier``
                            are type-checked when present.
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
49. ``no_python_gateway_imports`` — no active ``.py`` file may import
                            the deleted top-level ``Gateway`` package
                            (``from [Wylde.]Gateway … import`` or
                            ``import [Wylde.]Gateway``).  The Python
                            FastAPI Gateway was deleted on 2026-05-30 and
                            its client libraries moved to ``Core/shared/``
                            (``egress_client`` / ``gateway_auth`` /
                            ``extension_routes``); any surviving import is
                            a latent ``ImportError``.  The matchers require
                            real import syntax so docstring prose can't
                            false-fire; the ``wylde_check`` package + tests
                            are skipped (they carry the pattern as data).
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
    check_import_paths,
    check_manifest_paths,
    check_memory_layer_boundaries,
    check_no_internal_http,
    check_service_owns_its_state,
)
from .rules._tools import (  # noqa: E402
    check_tool_docstring_required,
    check_tool_id_regex,
)
from .rules._actions import (  # noqa: E402
    check_action_docstring_required,
    check_action_registry,
)
from .rules._gui import (  # noqa: E402
    check_gateway_scope,
    check_gui_no_backend_bypass,
)
from .rules._runtime import (  # noqa: E402
    check_logging_setup_only,
    check_no_external_subprocess,
    check_pipe_name_convention,
    check_run_py_entry_point,
    check_run_py_startup_sequence,
    check_shutdown_handler_marks_stopped,
    check_shutdown_reaps_manifest_orphans,
    check_spawn_paths_exist,
)
from .rules._quality import (  # noqa: E402
    check_file_size_limit,
    check_manifest_sandbox_required,
    check_no_bare_except,
    check_test_init_present,
)
from .rules._rust import (  # noqa: E402
    check_import_paths_rust,
    check_logging_setup_only_rust,
    check_no_external_process_spawn_rust,
    check_no_silent_error_swallow_rust,
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
from .rules._gpui_nav import (  # noqa: E402
    check_nav_targets_exist,
)
from .rules._gpui_polish import (  # noqa: E402
    check_manifest_factory_resolves,
    check_rest_routes_exist_in_service,
    check_stream_call_must_handle_cancel,
)
from .rules._lifecycle import (  # noqa: E402
    check_every_service_has_manifest,
    check_launcher_enumerates_services_from_manifests,
    check_service_manifest_schema,
    check_shutdown_enumerates_services_from_manifests,
)
from .rules._gateway_contract import (  # noqa: E402
    check_gateway_verbs_exist_in_harness_registry,
)
from .rules._no_gateway_import import (  # noqa: E402
    check_no_python_gateway_imports,
)
from .rules._no_bare_tokio import (  # noqa: E402
    check_no_bare_tokio_in_panel_src,
)
from .rules._no_panic_in_panel_render import check_no_panic_in_panel_render  # noqa: E402
from .rules._silent_skip_in_service_start import (  # noqa: E402
    check_silent_skip_in_service_start,
)
from ._single_file import (  # noqa: E402
    _check_dead_refs_lines,
    _check_import_paths_lines,
    _check_logging_setup_lines,
    _check_no_external_subprocess_lines,
    _check_no_http_lines,
    _check_pipe_name_convention_lines,
    _check_tool_docstring_lines,
    _check_tool_id_lines,
)


# ── Top-level dispatcher ──────────────────────────────────────────────


_RULES: Dict[str, Callable[[], List[Finding]]] = {
    "no_internal_http": check_no_internal_http,
    "manifest_paths": check_manifest_paths,
    "tool_id_regex": check_tool_id_regex,
    "action_registry": check_action_registry,
    "import_paths": check_import_paths,
    "dead_service_refs": check_dead_service_refs,
    # rule 7 (inferencebar_purity) retired at the slice-11 cutover — Svelte gone.
    "gateway_scope": check_gateway_scope,
    # rule 9 (gui_action_contract) retired — subsumed by panel_verbs_exist_in_harness_registry.
    "gui_no_backend_bypass": check_gui_no_backend_bypass,
    # rule 11 (gui_pipe_constants) retired — subsumed by the gpui contract rules.
    "tool_docstring_required": check_tool_docstring_required,
    "logging_setup_only": check_logging_setup_only,
    "no_external_subprocess": check_no_external_subprocess,
    "spawn_paths_exist": check_spawn_paths_exist,
    "run_py_entry_point": check_run_py_entry_point,
    "pipe_name_convention": check_pipe_name_convention,
    "run_py_startup_sequence": check_run_py_startup_sequence,
    "shutdown_handler_marks_stopped": check_shutdown_handler_marks_stopped,
    "shutdown_reaps_manifest_orphans": check_shutdown_reaps_manifest_orphans,
    "file_size_limit": check_file_size_limit,
    "test_init_present": check_test_init_present,
    "memory_layer_boundaries": check_memory_layer_boundaries,
    "action_docstring_required": check_action_docstring_required,
    "no_bare_except": check_no_bare_except,
    "service_owns_its_state": check_service_owns_its_state,
    "import_paths_rust": check_import_paths_rust,
    "no_silent_error_swallow_rust": check_no_silent_error_swallow_rust,
    "logging_setup_only_rust": check_logging_setup_only_rust,
    "no_external_process_spawn_rust": check_no_external_process_spawn_rust,
    # rule 30 (gui_error_reporting) retired at the slice-11 cutover — keyed on
    # Svelte console.error/toast; gpui panels surface errors as Result state.
    "manifest_sandbox_required": check_manifest_sandbox_required,
    "no_cross_panel_imports": check_no_cross_panel_imports,
    "no_legacy_gui_imports_in_panels": check_no_legacy_gui_imports_in_panels,
    "webview_only_in_extension_handlers": check_webview_only_in_extension_handlers,
    "first_party_manifest_must_be_gpui_view": check_first_party_manifest_must_be_gpui_view,
    "panel_crate_must_be_workspace_member": check_panel_crate_must_be_workspace_member,
    "panel_verbs_exist_in_harness_registry": check_panel_verbs_exist_in_harness_registry,
    "nav_targets_exist": check_nav_targets_exist,
    "required_services_includes_called_services": check_required_services_includes_called_services,
    "rest_routes_exist_in_service": check_rest_routes_exist_in_service,
    "manifest_factory_resolves": check_manifest_factory_resolves,
    "stream_call_must_handle_cancel": check_stream_call_must_handle_cancel,
    # Rules 44-47 — launcher / shutdown / service-manifest correctness
    # (slice-11 cutover).  Enforce the filesystem-as-registry contract.
    "launcher_enumerates_services_from_manifests": check_launcher_enumerates_services_from_manifests,
    "shutdown_enumerates_services_from_manifests": check_shutdown_enumerates_services_from_manifests,
    "every_service_has_manifest": check_every_service_has_manifest,
    "service_manifest_schema": check_service_manifest_schema,
    # Rule 48 — Gateway→harness dispatch contract (codebase-audit slice,
    # 2026-05-30).  The outbound companion to rule 38's inbound (panel→
    # harness) check.
    "gateway_verbs_exist_in_harness_registry": check_gateway_verbs_exist_in_harness_registry,
    # Rule 49 — no import of the deleted Python Gateway package
    # (egress-client relocation slice, 2026-05-30).  Bars `from Gateway`
    # / `import Gateway` from ever returning after the folder was deleted.
    "no_python_gateway_imports": check_no_python_gateway_imports,
    "no_bare_tokio_in_panel_src": check_no_bare_tokio_in_panel_src,
    # Rule 51 — panic primitives in a gpui panel render path (Dashboard
    # cold-start crash slice, 2026-05-31); panels share the event loop.
    "no_panic_in_panel_render": check_no_panic_in_panel_render,
    # Rule 52 — silent early-returns in lifecycle start_<service> functions
    # (silent-skip slice, 2026-05-31); a skipped spawn must log its reason.
    "silent_skip_in_service_start": check_silent_skip_in_service_start,
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
assert len(_RULES) == 48, f"_RULES dispatcher size drifted: {len(_RULES)} (expected 48)"


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
    DO need cross-file state (manifest_paths, action_registry,
    gateway_scope, gui_*, spawn_paths_exist, run_py_*) are skipped here
    — the full ``run_all()`` catches those.

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
    findings.extend(_check_no_http_lines(rel_path, content))
    findings.extend(_check_import_paths_lines(rel_path, content))
    findings.extend(_check_dead_refs_lines(rel_path, content))
    findings.extend(_check_tool_id_lines(rel_path, content))
    findings.extend(_check_tool_docstring_lines(rel_path, content))
    findings.extend(_check_logging_setup_lines(rel_path, content))
    findings.extend(_check_no_external_subprocess_lines(rel_path, content))
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
    "check_no_internal_http",
    "check_manifest_paths",
    "check_tool_id_regex",
    "check_action_registry",
    "check_import_paths",
    "check_dead_service_refs",
    "check_gateway_scope",
    "check_gui_no_backend_bypass",
    "check_tool_docstring_required",
    "check_logging_setup_only",
    "check_no_external_subprocess",
    "check_spawn_paths_exist",
    "check_run_py_entry_point",
    "check_pipe_name_convention",
    "check_run_py_startup_sequence",
    "check_shutdown_handler_marks_stopped",
    "check_shutdown_reaps_manifest_orphans",
    "check_file_size_limit",
    "check_test_init_present",
    "check_memory_layer_boundaries",
    "check_action_docstring_required",
    "check_no_bare_except",
    "check_service_owns_its_state",
    "check_import_paths_rust",
    "check_no_silent_error_swallow_rust",
    "check_logging_setup_only_rust",
    "check_no_external_process_spawn_rust",
    "check_manifest_sandbox_required",
    "check_no_cross_panel_imports",
    "check_no_legacy_gui_imports_in_panels",
    "check_webview_only_in_extension_handlers",
    "check_first_party_manifest_must_be_gpui_view",
    "check_panel_crate_must_be_workspace_member",
    "check_panel_verbs_exist_in_harness_registry",
    "check_nav_targets_exist",
    "check_required_services_includes_called_services",
    "check_rest_routes_exist_in_service",
    "check_manifest_factory_resolves",
    "check_stream_call_must_handle_cancel",
    "check_launcher_enumerates_services_from_manifests",
    "check_shutdown_enumerates_services_from_manifests",
    "check_every_service_has_manifest",
    "check_service_manifest_schema",
    "check_gateway_verbs_exist_in_harness_registry",
    "check_no_python_gateway_imports",
    "check_no_bare_tokio_in_panel_src",
    "check_no_panic_in_panel_render",
    "check_silent_skip_in_service_start",
]
