"""Runtime / lifecycle rules: logging-setup centralization,
subprocess restrictions, spawn-path resolution, run.py conventions,
pipe-name convention, startup sequence, shutdown handler."""

from __future__ import annotations

import json
import re
import sys as _sys
from typing import Dict, List, Optional, Tuple

from .. import Finding
from .._config import (
    DEPRECATED_ENTRY_PATTERNS,
    LOGGING_SETUP_PATTERNS,
    PIPE_NAME_GOOD_RE,
    PIPE_NAME_REF_RE,
    PIPE_NAME_TYPO_RE,
    SERVICE_FOLDERS,
    SUBPROCESS_ALLOWED_PREFIXES,
    SUBPROCESS_PATTERNS,
)
from .._walkers import _is_test_path, _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# Pattern an entry_point manifest field must satisfy.  Two language
# prefixes are valid today: ``python:<dotted-module>`` for the existing
# Python services and ``rust:<crate-bin>`` for the Rust services
# landing in the next phases.  The colon separator + non-empty suffix
# is non-negotiable.
_ENTRY_POINT_RE = re.compile(r"^(python|rust):.+$")


# ── Rule 13: logging setup is centralized ────────────────────────────


def check_logging_setup_only() -> List[Finding]:
    """Only ``Core/shared/logging_setup.configure_logging()`` should
    configure root logging in active code.  Other ``logging.basicConfig``
    / ``getLogger().addHandler`` calls regress Phase 12's centralization."""
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        # The source of truth itself is allowed to use the primitives.
        if rel == "Core/shared/logging_setup.py":
            continue
        # Tests sometimes need to configure logging for assertions.
        if _is_test_path(rel):
            continue
        # The checker itself mentions the matched patterns in its docstring.
        if rel.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#"):
                continue
            # Skip docstring lines — the regex would otherwise match
            # mentions inside service-side help text.  We can't AST-parse
            # every file just for this rule, but rejecting lines whose
            # stripped form starts with three quotes catches the common
            # docstring delimiter and avoids most documentation false
            # positives.  Direct code calls don't start that way.
            if (
                stripped.startswith('"""')
                or stripped.startswith("'''")
                or stripped.startswith('r"""')
                or stripped.startswith("r'''")
            ):
                continue
            for pat in LOGGING_SETUP_PATTERNS:
                if pat.search(line):
                    out.append(
                        Finding(
                            rule="logging_setup_only",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Direct logging.basicConfig / "
                                "getLogger().addHandler call detected.  "
                                "Replace with `from Core.shared.logging_setup "
                                "import configure_logging; configure_logging(...)`."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break
    return out


# ── Rule 14: subprocess spawning is daemon-only ──────────────────────


def _is_subprocess_allowed(rel: str) -> bool:
    for prefix in SUBPROCESS_ALLOWED_PREFIXES:
        if rel == prefix or rel.startswith(prefix):
            return True
    return False


def check_no_external_subprocess() -> List[Finding]:
    """``subprocess.Popen`` / ``.run`` / ``os.spawn*`` are restricted to
    the Lifecycle daemon plus the documented external-program wrappers
    (tool runtimes, Memgraph, VPN tunnel, audio device manager)."""
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        if _is_test_path(rel):
            continue
        if _is_subprocess_allowed(rel):
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#"):
                continue
            for pat in SUBPROCESS_PATTERNS:
                if pat.search(line):
                    out.append(
                        Finding(
                            rule="no_external_subprocess",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Subprocess spawning is restricted to the "
                                "Lifecycle daemon and a narrow allowlist "
                                "(tool runtimes, Memgraph wrapper, VPN "
                                "tunnel shell-outs).  To invoke another "
                                "Wylde service, use its pipe action instead."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break
    return out


# ── Rule 15: spawn-command paths in daemon_state.py exist ────────────


def _module_resolves(dotted: str) -> bool:
    """Best-effort check that ``a.b.c`` resolves to a real Python file
    under WYLDE_ROOT.  Returns True if either ``a/b/c.py`` exists or
    ``a/b/c/__init__.py`` exists."""
    parts = dotted.split(".")
    file_candidate = _pkg.WYLDE_ROOT.joinpath(*parts).with_suffix(".py")
    if file_candidate.exists():
        return True
    pkg_candidate = _pkg.WYLDE_ROOT.joinpath(*parts) / "__init__.py"
    if pkg_candidate.exists():
        return True
    return False


def check_spawn_paths_exist() -> List[Finding]:
    """Every ``python -m <module>`` or ``[..., "script.py"]`` argument
    constructed inside ``Core/Lifecycle/daemon_state.py`` must resolve
    to an importable module or an existing script."""
    daemon_state = _pkg.WYLDE_ROOT / "Core" / "Lifecycle" / "daemon_state.py"
    if not daemon_state.exists():
        return []
    text = _read_text(daemon_state)
    if not text:
        return []
    rel = _to_rel(daemon_state)
    out: List[Finding] = []
    # Find every `cmd = [...]` construction whose entries include either
    # the `-m` flag (next entry is a dotted module path) or a string that
    # ends in ".py" (treated as a script path).
    for lineno, line in enumerate(text.splitlines(), start=1):
        # `-m <module>` form — pick the dotted name from the next quoted
        # argument on the same line.
        m = re.search(r'"-m"\s*,\s*"([A-Za-z0-9_.]+)"', line)
        if m:
            module = m.group(1)
            if not _module_resolves(module):
                out.append(
                    Finding(
                        rule="spawn_paths_exist",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Spawn command references module {module!r} "
                            f"via `-m`, but no such module exists under the "
                            f"Wylde tree."
                        ),
                        context=line.strip()[:200],
                    )
                )
        # `script.py` form — path-shaped string literal.
        for sm in re.finditer(r'"([A-Za-z0-9_./-]+\.py)"', line):
            script = sm.group(1)
            candidate = (_pkg.WYLDE_ROOT / script).resolve()
            if not candidate.exists():
                out.append(
                    Finding(
                        rule="spawn_paths_exist",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Spawn command references script path "
                            f"{script!r}, but the file does not exist."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 16: run.py entry-point naming convention ────────────────────


def _service_name_for_folder(rel_folder: str) -> Optional[str]:
    """Best-effort mapping from a service folder to the manifest
    ``service`` field that folder's run.py writes.

    Used by the manifest-first path in :func:`check_run_py_entry_point`
    to look up ``data/manifests/<service>.json``. Returns ``None`` for
    folders that don't host a top-level service (Trainer/Caption is a
    sub-module, Extensions/* are library-style).
    """
    mapping = {
        "Core/resource_monitor": "vram-broker",
        "Core/Memgraph": "wylde-memgraph",
        "device_gate": "wylde-device-gate",
        "Gateway": "wylde-gateway",
        "Voice": "wylde-voice",
        "VPN": "wylde-vpn",
    }
    return mapping.get(rel_folder)


def _load_manifest(service_name: str) -> Optional[Dict[str, object]]:
    """Read ``data/manifests/<service>.json``. Returns ``None`` when the
    file is missing or unreadable so callers can apply their fallback."""
    path = _pkg.WYLDE_ROOT / "data" / "manifests" / f"{service_name}.json"
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def check_run_py_entry_point() -> List[Finding]:
    """Every active service declares its entry point in its manifest.

    Manifest-first (post W1.6): each service folder maps to a
    ``data/manifests/<service>.json`` file, and the ``entry_point``
    field there must match ``^(python|rust):.+$``. That makes the rule
    language-agnostic — a Rust service stamps ``rust:wylde-foo-bin``
    on startup and passes without any rule changes here.

    Fallback (one-release transitional): when the manifest doesn't
    carry an ``entry_point`` field yet, the rule falls back to the
    on-disk ``run.py`` / deprecated-pattern check it used previously,
    so services that haven't started since the field rolled out don't
    false-fire.
    """
    out: List[Finding] = []
    for rel_folder in SERVICE_FOLDERS:
        folder = _pkg.WYLDE_ROOT / rel_folder
        if not folder.exists() or not folder.is_dir():
            continue
        service_name = _service_name_for_folder(rel_folder)
        manifest = _load_manifest(service_name) if service_name else None
        entry_point = (
            manifest.get("entry_point") if isinstance(manifest, dict) else None
        )

        if isinstance(entry_point, str):
            # Manifest-first path. We have an authoritative entry_point
            # declaration; verify it parses cleanly.
            if not _ENTRY_POINT_RE.match(entry_point):
                manifest_rel = (
                    f"data/manifests/{service_name}.json"
                    if service_name
                    else rel_folder
                )
                out.append(
                    Finding(
                        rule="run_py_entry_point",
                        severity="error",
                        file=manifest_rel,
                        line=0,
                        message=(
                            f"Service {service_name!r} declares entry_point "
                            f"{entry_point!r} which does not match "
                            f"`^(python|rust):.+$`.  Use the language-prefixed "
                            f"form (e.g. `python:Voice.run` or "
                            f"`rust:wylde-voice-bin`)."
                        ),
                    )
                )
            continue

        # Fallback path: manifest absent OR entry_point not yet present.
        # Inspect filesystem for deprecated entry-point names so the rule
        # still catches `<svc>_run.py`, `start_<x>.py`, `launcher*.py`,
        # `main_*.py` variants while the field rolls out.
        for entry in folder.iterdir():
            if not entry.is_file():
                continue
            name = entry.name
            if name == "run.py":
                continue
            for pat in DEPRECATED_ENTRY_PATTERNS:
                if pat.match(name):
                    out.append(
                        Finding(
                            rule="run_py_entry_point",
                            severity="error",
                            file=_to_rel(entry),
                            line=0,
                            message=(
                                f"Service folder {rel_folder!r} uses the "
                                f"deprecated entry-point name {name!r}.  "
                                f"Rename to ``run.py`` to match the "
                                f"convention every other service uses."
                            ),
                        )
                    )
                    break
    return out


# ── Rule 17: named-pipe naming convention ────────────────────────────


def check_pipe_name_convention() -> List[Finding]:
    """Every ``wylde-<name>`` named-pipe literal in active code must be
    lowercase, dash-separated, and start with ``wylde-``."""
    out: List[Finding] = []
    seen: set = set()
    for path in _walk((".py", ".js", ".svelte", ".rs", ".md", ".json")):
        rel = _to_rel(path)
        # The checker itself uses ``wylde-X`` / ``wylde-foo`` as
        # placeholders in docstrings and rule messages — skip it.
        if rel.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel:
            continue
        # The wylde_check test package uses bad-form pipe names as
        # synthetic data; skip it wholesale.
        if "/dev/tests/wylde_check/" in rel:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            # Pass 1: canonical dash form with bad casing / trailing noise.
            for m in PIPE_NAME_REF_RE.finditer(line):
                name = m.group(0)
                if PIPE_NAME_GOOD_RE.match(name):
                    continue
                if (name, rel, lineno) in seen:
                    continue
                seen.add((name, rel, lineno))
                out.append(
                    Finding(
                        rule="pipe_name_convention",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Pipe name {name!r} does not match the "
                            f"convention `^wylde-[a-z][a-z0-9-]*$`.  Use "
                            f"lowercase, dash-separated form."
                        ),
                        context=line.strip()[:200],
                    )
                )
            # Pass 2: typo'd underscore form, only inside quoted strings.
            for m in PIPE_NAME_TYPO_RE.finditer(line):
                name = m.group(1)
                if (name, rel, lineno) in seen:
                    continue
                seen.add((name, rel, lineno))
                out.append(
                    Finding(
                        rule="pipe_name_convention",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Pipe name {name!r} uses underscores; the "
                            f"convention is dash-separated "
                            f"(``wylde-{name[len('wylde_') :].replace('_', '-')}``)."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 18: run.py startup sequence ─────────────────────────────────


_STARTUP_PATTERNS: Dict[str, Tuple[re.Pattern[str], ...]] = {
    "configure_logging": (
        re.compile(r"\bconfigure_logging\s*\("),
        re.compile(r"\blogging\.basicConfig\s*\("),
    ),
    "write_manifest": (
        re.compile(r"\bwrite_(?:service_)?manifest\s*\("),
        re.compile(r"\b_write_daemon_manifest\s*\("),
        re.compile(r"\bmanifest\.write_manifest\s*\("),
    ),
    "start_heartbeat": (
        re.compile(r"\bstart_heartbeat\s*\("),
        re.compile(r"\b_start_daemon_heartbeat\s*\("),
        re.compile(r"\bmanifest\.start_heartbeat\s*\("),
    ),
    "serve_loop": (
        re.compile(r"\bserve_forever\s*\("),
        re.compile(r"\bserve\s*\("),
        re.compile(r"\buvicorn\.run\s*\("),
        re.compile(r"\bmainloop\s*\("),
        re.compile(r"\bsvc_main\s*\("),
        re.compile(r"\.run\s*\("),
        re.compile(r"\.start\s*\("),
        re.compile(r"\bstop_event\.wait\s*\("),
        re.compile(r"\b_shutdown_event\.wait\s*\("),
    ),
}


def _find_pattern_line(text: str, patterns: Tuple[re.Pattern[str], ...]) -> int:
    """Return the 1-based line number of the first match across all
    patterns; 0 when nothing matched."""
    for lineno, line in enumerate(text.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        for pat in patterns:
            if pat.search(line):
                return lineno
    return 0


_REQUIRED_STARTUP_PHASES: Tuple[str, ...] = (
    "configure_logging",
    "write_manifest",
    "start_heartbeat",
    "serve_loop",
)


def check_run_py_startup_sequence() -> List[Finding]:
    """Each service must traverse the four-phase startup convention:
    configure_logging → write_manifest → start_heartbeat → serve_loop.

    Manifest-first (post W1.7): when the service's manifest carries a
    ``startup_sequence`` list, the rule validates the recorded order
    rather than AST-walking ``run.py``.  Services self-attest by
    calling :func:`Core.shared.manifest.attest_phase`,
    :func:`write_manifest`, :func:`start_heartbeat`, and
    :func:`mark_serve_loop_entered` from the right places — those
    helpers are wired in :mod:`Core.shared.logging_setup`,
    :mod:`Core.shared.manifest`, and :mod:`Core.shared.ipc._server`,
    plus their Rust mirrors in ``wylde-shared``.

    Fallback (one-release transitional): when the manifest hasn't
    been written yet or doesn't carry ``startup_sequence``, the rule
    falls back to the regex source-walk it used previously so services
    that haven't started since the field rolled out don't false-fire.
    """
    out: List[Finding] = []
    for rel_folder in SERVICE_FOLDERS:
        run_path = _pkg.WYLDE_ROOT / rel_folder / "run.py"
        service_name = _service_name_for_folder(rel_folder)
        manifest = _load_manifest(service_name) if service_name else None
        sequence = (
            manifest.get("startup_sequence") if isinstance(manifest, dict) else None
        )

        if isinstance(sequence, list) and sequence:
            manifest_rel = (
                f"data/manifests/{service_name}.json" if service_name else rel_folder
            )
            seq_str: List[str] = [s for s in sequence if isinstance(s, str)]
            missing = [p for p in _REQUIRED_STARTUP_PHASES if p not in seq_str]
            for phase in missing:
                out.append(
                    Finding(
                        rule="run_py_startup_sequence",
                        severity="warning",
                        file=manifest_rel,
                        line=0,
                        message=(
                            f"Service {service_name!r} startup_sequence is "
                            f"missing the {phase!r} phase.  Each service "
                            f"should self-attest configure_logging → "
                            f"write_manifest → start_heartbeat → serve_loop."
                        ),
                    )
                )
            if not missing:
                indexes = [seq_str.index(p) for p in _REQUIRED_STARTUP_PHASES]
                if indexes != sorted(indexes):
                    out.append(
                        Finding(
                            rule="run_py_startup_sequence",
                            severity="warning",
                            file=manifest_rel,
                            line=0,
                            message=(
                                f"Service {service_name!r} startup_sequence "
                                f"is {seq_str!r}; canonical order is "
                                f"configure_logging → write_manifest → "
                                f"start_heartbeat → serve_loop."
                            ),
                        )
                    )
            continue

        # Fallback path: manifest absent OR startup_sequence missing.
        if not run_path.exists():
            continue
        rel = _to_rel(run_path)
        text = _read_text(run_path)
        if not text:
            continue
        positions: Dict[str, int] = {}
        for step, patterns in _STARTUP_PATTERNS.items():
            positions[step] = _find_pattern_line(text, patterns)
        for step in _REQUIRED_STARTUP_PHASES:
            if positions[step] == 0:
                out.append(
                    Finding(
                        rule="run_py_startup_sequence",
                        severity="warning",
                        file=rel,
                        line=0,
                        message=(
                            f"run.py is missing the {step!r} step of the "
                            f"startup convention.  This is a WARNING — "
                            f"daemon-managed services may legitimately skip "
                            f"manifest writes; review and either add the "
                            f"call or leave a comment explaining why it is "
                            f"intentionally absent."
                        ),
                    )
                )
        if all(positions[s] for s in _REQUIRED_STARTUP_PHASES):
            cfg = positions["configure_logging"]
            wm = positions["write_manifest"]
            hb = positions["start_heartbeat"]
            sv = positions["serve_loop"]
            if cfg > min(wm, hb, sv):
                out.append(
                    Finding(
                        rule="run_py_startup_sequence",
                        severity="warning",
                        file=rel,
                        line=cfg,
                        message=(
                            "configure_logging should be the first startup "
                            "step so manifest / heartbeat / serve logs are "
                            "captured."
                        ),
                    )
                )
            if sv < max(cfg, wm, hb):
                out.append(
                    Finding(
                        rule="run_py_startup_sequence",
                        severity="warning",
                        file=rel,
                        line=sv,
                        message=(
                            "serve loop should be the last startup step; "
                            "running it before manifest / heartbeat means "
                            "the service can serve requests before the "
                            "dashboard knows it is up."
                        ),
                    )
                )
    return out


# ── Rule 19: shutdown handler updates manifest ──────────────────────


# Matches both literal forms (``signal.signal(signal.SIGTERM, handler)``)
# AND dynamic forms (``signal.signal(sig, handler)`` where ``sig`` is a
# loop variable holding SIGTERM/SIGINT/SIGBREAK).  Several services use
# the dynamic form to support cross-platform signal sets.
_SIGNAL_REG_RE = re.compile(r"\bsignal\.signal\s*\(")
_MARK_STOPPED_RE = re.compile(
    r"\b(?:mark_stopped|_delete_manifest|delete_manifest|unregister_(?:core_)?manifest|stop_heartbeat)\s*\("
)
_ATEXIT_REG_RE = re.compile(r"\batexit\.register\s*\(")


def check_shutdown_handler_marks_stopped() -> List[Finding]:
    """Each service should wire a graceful shutdown that flips the
    manifest to ``stopped`` so the dashboard sees a clean exit.

    Manifest-first (post W1.8): when the manifest carries
    ``shutdown_attested: true``, the rule treats the service as
    contract-compliant — :func:`Core.shared.manifest.mark_stopped`
    flips that field for the Python side, and
    ``wylde_shared::manifest::ManifestWriter::mark_stopped`` does the
    same for Rust services.  A live ``alive`` manifest naturally has
    ``shutdown_attested = false`` until the process exits, which is
    fine: the rule only flags services whose manifest exists and is in
    a stopped state without the attestation flag, OR services whose
    ``run.py`` doesn't wire any shutdown path at all.

    Fallback (transitional): when the manifest is absent (e.g. service
    never ran) the rule walks ``run.py`` for SIGTERM/SIGINT/atexit
    registration plus a manifest-cleanup callsite, matching the
    pre-W1.8 behaviour.
    """
    out: List[Finding] = []
    for rel_folder in SERVICE_FOLDERS:
        run_path = _pkg.WYLDE_ROOT / rel_folder / "run.py"
        service_name = _service_name_for_folder(rel_folder)
        manifest = _load_manifest(service_name) if service_name else None

        if isinstance(manifest, dict) and "shutdown_attested" in manifest:
            attested = bool(manifest.get("shutdown_attested"))
            status = manifest.get("status")
            state = status.get("state") if isinstance(status, dict) else None
            if state == "stopped" and not attested:
                manifest_rel = f"data/manifests/{service_name}.json"
                out.append(
                    Finding(
                        rule="shutdown_handler_marks_stopped",
                        severity="warning",
                        file=manifest_rel,
                        line=0,
                        message=(
                            f"Service {service_name!r} manifest is in the "
                            f"'stopped' state but shutdown_attested is "
                            f"false.  Use ``manifest.mark_stopped`` (or the "
                            f"Rust mirror) so the dashboard knows the exit "
                            f"was graceful."
                        ),
                    )
                )
            continue

        # Fallback path: no manifest yet, OR shutdown_attested missing.
        if not run_path.exists():
            continue
        rel = _to_rel(run_path)
        text = _read_text(run_path)
        if not text:
            continue
        has_signal = bool(_SIGNAL_REG_RE.search(text))
        has_atexit = bool(_ATEXIT_REG_RE.search(text))
        has_mark_stopped = bool(_MARK_STOPPED_RE.search(text))
        if not has_signal and not has_atexit:
            out.append(
                Finding(
                    rule="shutdown_handler_marks_stopped",
                    severity="warning",
                    file=rel,
                    line=0,
                    message=(
                        "run.py does not register a SIGTERM/SIGINT handler "
                        "or an atexit callback.  Daemon-managed services "
                        "may rely on the daemon's teardown, but standalone "
                        "services should wire a graceful-shutdown path so "
                        "their manifest reflects the stopped state."
                    ),
                )
            )
            continue
        if not has_mark_stopped:
            out.append(
                Finding(
                    rule="shutdown_handler_marks_stopped",
                    severity="warning",
                    file=rel,
                    line=0,
                    message=(
                        "run.py registers a signal handler / atexit "
                        "callback but never calls a manifest-cleanup "
                        "function (mark_stopped / delete_manifest / "
                        "stop_heartbeat).  Daemon-managed services may "
                        "rely on the daemon doing this, but verify the "
                        "manifest state stays accurate after teardown."
                    ),
                )
            )
    return out


# ── Rule 31: daemon shutdown reaps manifest orphans ──────────────────


# Canonical Python lifecycle daemon entry point that owns the
# unified-teardown function. If this file ever moves the rule needs
# updating — the path is asserted explicitly so a structural rename
# can't make the rule silently skip its target.
_SHUTDOWN_TARGET_FILE = "Core/Lifecycle/daemon_state/__init__.py"
_SHUTDOWN_TARGET_FUNC = "stop_all_daemon_managed"

# A call inside ``stop_all_daemon_managed`` whose name matches this
# pattern satisfies the rule.  Any "reap*orphan*"-shaped identifier
# counts so the implementation can evolve (rename to
# ``reap_live_orphans`` etc.) without churn here.
_REAP_NAME_RE = re.compile(r"^_?reap[_a-z]*orphan[_a-z]*$")


def check_shutdown_reaps_manifest_orphans() -> List[Finding]:
    """The daemon's unified-teardown function must invoke a
    manifest-walking orphan reaper.

    Why: ``stop_all_daemon_managed`` walks the in-memory Popen handles
    (``_gateway_proc`` etc.) the daemon populated this session.  Services
    that survived a prior daemon crash live in ``data/manifests/*.json``
    with a pid still in the process table — the matching ``_<svc>_proc``
    is ``None`` on a fresh daemon, the periodic orphan sweep only acts
    when the pid is *dead*, and the orphan outlives every shutdown until
    something hard-kills it.  A manifest-walking reap is the only
    safety net that closes the loop.

    The rule checks that ``stop_all_daemon_managed`` in
    ``Core/Lifecycle/daemon_state/__init__.py`` contains a call to a
    function whose name matches ``reap*orphan*``.  Structural rather
    than name-specific so the reaper can be renamed without churn here,
    but pattern-bound so it can't be silently replaced with an
    unrelated function call.
    """
    out: List[Finding] = []
    target = _pkg.WYLDE_ROOT / _SHUTDOWN_TARGET_FILE
    if not target.exists():
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=_SHUTDOWN_TARGET_FILE,
                line=0,
                message=(
                    f"Expected the unified-teardown owner at "
                    f"{_SHUTDOWN_TARGET_FILE!r}; file not found.  If the "
                    f"daemon refactored, update the rule's target path."
                ),
            )
        )
        return out

    text = _read_text(target)
    if not text:
        return out

    import ast

    try:
        tree = ast.parse(text, filename=str(target))
    except SyntaxError as exc:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=_SHUTDOWN_TARGET_FILE,
                line=getattr(exc, "lineno", 0) or 0,
                message=f"Cannot parse {_SHUTDOWN_TARGET_FILE!r}: {exc}",
            )
        )
        return out

    target_func: Optional[ast.FunctionDef] = None
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.name == _SHUTDOWN_TARGET_FUNC:
                target_func = node  # type: ignore[assignment]
                break

    if target_func is None:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=_SHUTDOWN_TARGET_FILE,
                line=0,
                message=(
                    f"Expected function {_SHUTDOWN_TARGET_FUNC!r} in "
                    f"{_SHUTDOWN_TARGET_FILE!r} — the unified teardown the "
                    f"signal handler + service.shutdown_all action both "
                    f"go through.  If renamed, update the rule's target."
                ),
            )
        )
        return out

    reaper_call_lineno: Optional[int] = None
    for sub in ast.walk(target_func):
        if not isinstance(sub, ast.Call):
            continue
        func = sub.func
        # Match bare-name calls (``reap_manifest_orphans()``) AND
        # attribute calls (``self.reap_manifest_orphans()``,
        # ``mod._reap_orphans()``) — the rightmost identifier carries
        # the semantic name.
        if isinstance(func, ast.Name):
            name = func.id
        elif isinstance(func, ast.Attribute):
            name = func.attr
        else:
            continue
        if _REAP_NAME_RE.match(name):
            reaper_call_lineno = sub.lineno
            break

    if reaper_call_lineno is None:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=_SHUTDOWN_TARGET_FILE,
                line=target_func.lineno,
                message=(
                    f"{_SHUTDOWN_TARGET_FUNC!r} does not call a "
                    f"manifest-orphan reaper.  Without a reap step the "
                    f"shutdown only kills the daemon's in-memory Popen "
                    f"handles — services orphaned by a prior daemon "
                    f"crash survive every restart.  Call a function "
                    f"whose name matches ``reap*orphan*`` (e.g. "
                    f"``reap_manifest_orphans``) as the final step."
                ),
            )
        )
    return out


# Rules 20 (file_size_limit), 21 (test_init_present), and 24
# (no_bare_except) live in the sibling ``_quality.py`` submodule.
