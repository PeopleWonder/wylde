"""Wylde central config loader.

Pattern: each `*.yaml` file in this folder declares a flat map of env-var
names → values. At app startup, `load_all_to_env()` reads every YAML here
and exports the values into `os.environ`. Subsequent code (the harness,
launched services, etc.) reads its config from the env.

Env always wins: if `WYLDE_FOO` is already set in the environment when the
loader runs, the YAML value is NOT applied. This lets you override any
config file value with a one-shot env var without editing the YAML.

Add a new config module by dropping a YAML file here. No code changes.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from typing import Any

import yaml

logger = logging.getLogger("wylde.config")

CONFIG_DIR: Path = Path(__file__).resolve().parent


def load_all_to_env(*, override: bool = False) -> dict[str, str]:
    """Read every *.yaml in this folder and export values into os.environ.

    Args:
        override: if True, YAML values overwrite existing env vars.
                  Default False (env wins, per the documented pattern).

    Returns:
        Dict of {env_var_name: applied_value} for everything that was set.
    """
    applied: dict[str, str] = {}
    for path in sorted(CONFIG_DIR.glob("*.yaml")):
        applied.update(_load_one(path, override=override))
    if applied:
        logger.info(
            "config: loaded %d env vars from %d files",
            len(applied),
            len(list(CONFIG_DIR.glob("*.yaml"))),
        )
    return applied


def _load_one(path: Path, *, override: bool) -> dict[str, str]:
    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except yaml.YAMLError as e:
        logger.error("config: %s is malformed YAML: %s", path.name, e)
        return {}

    if not isinstance(data, dict):
        logger.error("config: %s top-level is not a mapping", path.name)
        return {}

    applied: dict[str, str] = {}
    for key, value in data.items():
        if not isinstance(key, str):
            logger.warning("config: skipping non-string key %r in %s", key, path.name)
            continue
        if not override and key in os.environ:
            continue  # env wins
        os.environ[key] = _stringify(value)
        applied[key] = os.environ[key]
    return applied


def _stringify(value: Any) -> str:
    """Convert a YAML value into the string form an env var expects."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return ""
    return str(value)


__all__ = ["load_all_to_env", "CONFIG_DIR"]
