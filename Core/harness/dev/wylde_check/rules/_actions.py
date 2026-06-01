"""Action-registry rules: per-pipe action uniqueness, GUI pipeAction
contract, registered-action docstring required."""

from __future__ import annotations

import ast
import json
import re
import sys as _sys
from typing import Dict, List, Optional, Set, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


_REGISTER_ACTION_RE = re.compile(r'register_action\(\s*"([^"]+)"')


# ── Contract-file lookup (post W1.1) ─────────────────────────────────


def _load_action_contract(service_name: str) -> Set[str]:
    """Read ``data/contracts/actions/<service>.json`` and return the
    action names listed there.

    The contract file is the cross-language source of truth: every
    pipe-hosting service writes it on startup (see
    ``Core/shared/ipc/_server._write_action_contract``). Rules read the
    contract instead of grepping Python source so Rust services (when
    they land) participate in the same checks.

    Returns an empty set when the contract file is missing — callers
    are expected to fall back to source-grep for one release so the
    transition can land without forcing every service to have started
    once first.
    """
    contract_path = (
        _pkg.WYLDE_ROOT / "data" / "contracts" / "actions" / f"{service_name}.json"
    )
    if not contract_path.exists():
        return set()
    try:
        data = json.loads(contract_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return set()
    actions = data.get("actions") or []
    if not isinstance(actions, list):
        return set()
    return {a for a in actions if isinstance(a, str)}


def _load_action_contract_list(service_name: str) -> List[str]:
    """Same as :func:`_load_action_contract` but returns the ordered list
    so duplicate-detection can spot duplicates in the contract itself."""
    contract_path = (
        _pkg.WYLDE_ROOT / "data" / "contracts" / "actions" / f"{service_name}.json"
    )
    if not contract_path.exists():
        return []
    try:
        data = json.loads(contract_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    actions = data.get("actions") or []
    if not isinstance(actions, list):
        return []
    return [a for a in actions if isinstance(a, str)]


# Maps the pipe source modules each service exposes to the service name
# that owns the matching contract artifact. Used by the contract-first
# path to know which contract to load, and by the source-grep fallback
# to know which files to inspect.
_SERVICE_TO_PIPE_MODULES: Dict[str, Tuple[str, ...]] = {
    "wylde-lifecycle": ("Core/Lifecycle/control.py",),
    "wylde-harness": ("Core/harness/pipe/__init__.py",),
    "wylde-device-gate": ("device_gate/pipe.py",),
    "wylde-voice": ("Voice/pipe.py",),
    "wylde-gateway": ("Gateway/pipe.py",),
    "wylde-extensions": ("Extensions/extension_bridge/dispatcher.py",),
}


# ── Rule 4: action registry consistency ───────────────────────────────


def check_action_registry() -> List[Finding]:
    """Sanity check: action names registered by a pipe module should be
    unique within that pipe.  Cross-pipe dupes are allowed (e.g.
    every pipe can have its own ``health`` action).

    Contract-first: when ``data/contracts/actions/<service>.json``
    exists, the rule inspects the contract's ``actions`` list (which is
    what every wylde_check downstream consumer reads). Duplicates in
    the contract are a finding even though the dict-keyed writer makes
    them improbable — surfacing one means a hand-edited contract or a
    race in the writer.

    Source-grep fallback runs for any service whose contract file is
    missing (transitional window during the W1.x port). Pipe modules
    listed in ``_SERVICE_TO_PIPE_MODULES`` are scanned for duplicate
    ``register_action("name", ...)`` literals.
    """
    out: List[Finding] = []
    for service, pipe_modules in _SERVICE_TO_PIPE_MODULES.items():
        contract_actions = _load_action_contract_list(service)
        contract_path = (
            _pkg.WYLDE_ROOT / "data" / "contracts" / "actions" / f"{service}.json"
        )
        if contract_path.exists():
            # Duplicate scan on the contract list. The writer dict-keys
            # the registry so this is unlikely, but a hand-edited
            # contract or a stale tmpfile that survived an os.replace
            # crash could still surface dupes — we must catch them.
            seen: Dict[str, int] = {}
            for idx, name in enumerate(contract_actions):
                if name in seen:
                    out.append(
                        Finding(
                            rule="action_registry",
                            severity="error",
                            file=str(
                                contract_path.relative_to(_pkg.WYLDE_ROOT)
                            ).replace("\\", "/"),
                            line=0,
                            message=(
                                f"action {name!r} appears more than once in "
                                f"the contract for service {service!r} "
                                f"(positions {seen[name]} and {idx})"
                            ),
                        )
                    )
                else:
                    seen[name] = idx
            continue
        # Fallback path: contract absent, scan the pipe-module sources.
        # Kept for one release so the rule still fires while services
        # are first writing their contract files.
        for rel in pipe_modules:
            path = _pkg.WYLDE_ROOT / rel
            if not path.exists():
                continue
            text = _read_text(path)
            if not text:
                continue
            seen_lines: Dict[str, int] = {}
            for lineno, line in enumerate(text.splitlines(), start=1):
                m = _REGISTER_ACTION_RE.search(line)
                if not m:
                    continue
                name = m.group(1)
                if name in seen_lines:
                    out.append(
                        Finding(
                            rule="action_registry",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                f"action {name!r} registered more than once "
                                f"(first at line {seen_lines[name]})"
                            ),
                            context=line.strip()[:200],
                        )
                    )
                else:
                    seen_lines[name] = lineno
    return out


# ── Rule 9: GUI action contract ───────────────────────────────────────


# ── Rule 23: every registered pipe action handler has a docstring ────


# Pipe modules that own canonical action.name → handler mappings.
# Rule 23 walks these for handler *bodies* (docstring presence).
_ACTION_PIPE_MODULES: Tuple[str, ...] = (
    "Core/Lifecycle/control.py",
    "Core/harness/pipe/__init__.py",
    "device_gate/pipe.py",
    "Voice/pipe.py",
    "Gateway/pipe.py",
    "Extensions/extension_bridge/dispatcher.py",
)


_ACTION_DOCSTRING_MIN = 15


def _collect_action_handler_names(text: str) -> List[Tuple[str, str]]:
    """Return ``[(action_name, handler_symbol), ...]`` extracted from a
    pipe-module source.

    Covers both wiring styles:
      * ``register_action("name", handler_symbol)`` — handler symbol is
        the bare identifier in the second positional argument.
      * ``_ACTIONS = {"name": handler_symbol, ...}`` — handler symbol
        is the value in the dict literal.

    Multi-line forms (handler on its own line) are handled by parsing
    the AST and matching by start-line proximity.
    """
    pairs: List[Tuple[str, str]] = []
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return pairs
    for node in ast.walk(tree):
        # Style 1: register_action("name", handler) call.
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "register_action"
            and len(node.args) >= 2
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
            and isinstance(node.args[1], ast.Name)
        ):
            pairs.append((node.args[0].value, node.args[1].id))
        elif (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "register_action"
            and len(node.args) >= 2
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
            and isinstance(node.args[1], ast.Name)
        ):
            pairs.append((node.args[0].value, node.args[1].id))
        # Style 2: _ACTIONS = {"name": handler, ...} dict literal.
        elif isinstance(node, ast.Assign) and isinstance(node.value, ast.Dict):
            # Only consider assignments whose target name is *ACTIONS*.
            is_actions = False
            for target in node.targets:
                if isinstance(target, ast.Name) and "ACTIONS" in target.id:
                    is_actions = True
                    break
            if not is_actions:
                continue
            for key_node, val_node in zip(node.value.keys, node.value.values):
                if (
                    isinstance(key_node, ast.Constant)
                    and isinstance(key_node.value, str)
                    and isinstance(val_node, ast.Name)
                ):
                    pairs.append((key_node.value, val_node.id))
    return pairs


def _find_function_def(
    tree: ast.Module, name: str
) -> Optional[ast.FunctionDef | ast.AsyncFunctionDef]:
    for node in tree.body:
        if (
            isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name == name
        ):
            return node
    return None


def check_action_docstring_required() -> List[Finding]:
    """Every registered pipe-action handler function must have a non-
    empty top-level docstring (≥15 chars).  Handlers are the contract
    surface the LLM sees through ``tools.list`` / pipe introspection;
    missing docstrings ship a black-box action.

    Handlers are discovered via the same wiring patterns rule 9 reads
    (``register_action(...)`` callsites and ``_ACTIONS = {...}`` dicts)
    and resolved to ``def`` blocks inside the same module.  Handlers
    that resolve to an imported symbol (not defined in the pipe module)
    are skipped — those are checked when the linter visits their
    defining file.
    """
    out: List[Finding] = []
    for rel in _ACTION_PIPE_MODULES:
        path = _pkg.WYLDE_ROOT / rel
        if not path.exists():
            continue
        text = _read_text(path)
        if not text:
            continue
        try:
            tree = ast.parse(text)
        except SyntaxError:
            continue
        seen: set = set()
        for action_name, handler_sym in _collect_action_handler_names(text):
            if (action_name, handler_sym) in seen:
                continue
            seen.add((action_name, handler_sym))
            fn = _find_function_def(tree, handler_sym)
            if fn is None:
                # Handler defined elsewhere (e.g. imported from a
                # sibling module); the defining file's own visit will
                # cover the docstring.
                continue
            ds = ast.get_docstring(fn) or ""
            if len(ds.strip()) >= _ACTION_DOCSTRING_MIN:
                continue
            out.append(
                Finding(
                    rule="action_docstring_required",
                    severity="error",
                    file=rel,
                    line=getattr(fn, "lineno", 0),
                    message=(
                        f"Action handler {handler_sym!r} for action "
                        f"{action_name!r} is missing a docstring "
                        f"(or it is shorter than {_ACTION_DOCSTRING_MIN} "
                        f"characters).  Document the payload it accepts "
                        f"and the envelope it returns."
                    ),
                )
            )
    return out
