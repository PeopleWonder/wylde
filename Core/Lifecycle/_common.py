"""Internal helpers shared by the Lifecycle scripts.

The leading underscore signals this module is for use within Core/Lifecycle/
only. External callers should not import from here directly.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any

import yaml

logger = logging.getLogger("wylde.lifecycle")


# ─── Paths ────────────────────────────────────────────────────────────────

# Wylde/Core/Lifecycle/_common.py → Wylde/
WYLDE_ROOT: Path = Path(__file__).resolve().parent.parent.parent

CORE_DIR: Path = WYLDE_ROOT / "Core"
NETWORK_DIR: Path = CORE_DIR / "Network"
SERVICES_YAML: Path = NETWORK_DIR / "services.yaml"
# Committed template for services.yaml. The live file is runtime state
# (gitignored, rewritten by discovery on every boot); this seed carries the
# canonical comment header + empty roster and is copied into place on first
# boot. See ensure_services_file().
SERVICES_SEED: Path = NETWORK_DIR / "services.yaml.seed"

CACHE_DIR: Path = WYLDE_ROOT / ".wylde"
DISCOVERY_CACHE: Path = CACHE_DIR / "discovery.cache"

# Folders inside Wylde/ that are NOT discoverable services. Anything starting
# with `_` or `.` is excluded by convention. Core itself is excluded because
# its sub-services (Network, Lifecycle, harness, etc.) are infrastructure
# rather than launchable processes.
#
# `data`, `logs`, and `docs` are runtime / archive directories that hold
# state — not services. `rust` (the backend Cargo workspace) and `tools`
# (loose PowerShell setup scripts) are build/dev folders, not launchable
# processes. Without explicit exclusion, discovery's automatic manifest
# generator would mint a service entry for each on first boot (observed
# during the Phase-9 audit, and again for rust/tools) and the launcher
# would try to spawn them. Adding to the exclusion set is the simplest
# fix; the alternative (require manifest.json before registering) would
# break the auto-discover story for genuinely-new service folders.
EXCLUDED_TOP_LEVEL: frozenset[str] = frozenset(
    {"Core", "data", "logs", "docs", "rust", "tools"}
)
EXCLUDED_PREFIXES: tuple[str, ...] = ("_", ".")


# ─── Port assignment ──────────────────────────────────────────────────────

PORT_RANGE_START: int = 8000
PORT_RANGE_END: int = 8999  # inclusive


# ─── Shutdown ordering ──────────────────────────────────────────────────────

# Default shutdown slot for a service whose manifest declares no explicit
# `shutdown_order`. Services are stopped in ASCENDING order (lowest first);
# anything at the default falls back to reverse-launch order among its peers
# (a stable sort preserves that). Sits comfortably above the hand-assigned
# user-facing slots (GUI=10, Gateway=20, …) so undeclared services drain
# after the ones with an explicit early slot.
DEFAULT_SHUTDOWN_ORDER: int = 100


# ─── services.yaml IO ─────────────────────────────────────────────────────


def ensure_services_file() -> None:
    """Seed services.yaml from its committed template on first boot.

    services.yaml is runtime state (gitignored, rewritten by discovery), so a
    fresh clone — or a machine where the live file was wiped — has no roster
    file at all. Copy the committed seed into place so downstream readers find
    a well-formed file with the canonical comment header intact. No-op once the
    live file exists; discovery owns it from there.
    """
    if SERVICES_YAML.exists():
        return
    if not SERVICES_SEED.exists():
        logger.warning(
            "services.yaml.seed missing at %s; discovery will create the "
            "roster from scratch (no comment header)",
            SERVICES_SEED,
        )
        return
    NETWORK_DIR.mkdir(parents=True, exist_ok=True)
    SERVICES_YAML.write_text(
        SERVICES_SEED.read_text(encoding="utf-8"), encoding="utf-8"
    )
    logger.info("seeded services.yaml from services.yaml.seed")


def load_services() -> list[dict[str, Any]]:
    """Read services.yaml. Returns [] if file is missing or malformed."""
    if not SERVICES_YAML.exists():
        return []
    try:
        data = yaml.safe_load(SERVICES_YAML.read_text(encoding="utf-8")) or {}
    except yaml.YAMLError as e:
        logger.error("services.yaml is malformed: %s", e)
        return []
    services = data.get("services") or []
    if not isinstance(services, list):
        logger.error("services.yaml: 'services' is not a list")
        return []
    return services


def save_services(services: list[dict[str, Any]]) -> None:
    """Atomically write services.yaml.

    Writes machine-read runtime state — no comment header. (The canonical
    header lives in services.yaml.seed; safe_dump would strip any comments
    here on every rewrite anyway, and no consumer reads them.)
    """
    NETWORK_DIR.mkdir(parents=True, exist_ok=True)
    payload = {"services": services}
    body = yaml.safe_dump(payload, sort_keys=False, default_flow_style=False)
    # Atomic write — temp file + replace, so a crash mid-write doesn't corrupt
    tmp = SERVICES_YAML.with_suffix(".yaml.tmp")
    tmp.write_text(body, encoding="utf-8")
    tmp.replace(SERVICES_YAML)


def find_service(services: list[dict[str, Any]], name: str) -> dict[str, Any] | None:
    """Return the service entry with `name`, or None."""
    for svc in services:
        if svc.get("name") == name:
            return svc
    return None


# ─── Port pool ────────────────────────────────────────────────────────────


def assign_port(services: list[dict[str, Any]]) -> int:
    """Pick the lowest unused port in the range. Sequential with slot reuse."""
    used = {s.get("port") for s in services if isinstance(s.get("port"), int)}
    for port in range(PORT_RANGE_START, PORT_RANGE_END + 1):
        if port not in used:
            return port
    raise RuntimeError(
        f"Port pool exhausted ({PORT_RANGE_START}–{PORT_RANGE_END}). "
        f"Increase the range or remove unused services."
    )


# ─── Folder enumeration ───────────────────────────────────────────────────


def list_service_folders() -> list[Path]:
    """Top-level Wylde/ subdirs that count as services.

    Excludes Core/, anything starting with `_` or `.`, and anything that's
    not a directory. Order is sorted alphabetically for deterministic output.
    """
    out: list[Path] = []
    for p in sorted(WYLDE_ROOT.iterdir(), key=lambda x: x.name):
        if not p.is_dir():
            continue
        if p.name in EXCLUDED_TOP_LEVEL:
            continue
        if p.name.startswith(EXCLUDED_PREFIXES):
            continue
        out.append(p)
    return out


# ─── Discovery cache ──────────────────────────────────────────────────────


def read_discovery_cache() -> dict[str, float]:
    """Load the folder-name → mtime map. Empty dict if missing or unreadable."""
    if not DISCOVERY_CACHE.exists():
        return {}
    try:
        data: dict[str, float] = json.loads(DISCOVERY_CACHE.read_text(encoding="utf-8"))
        return data
    except (json.JSONDecodeError, OSError) as e:
        logger.warning("discovery.cache unreadable, treating as empty: %s", e)
        return {}


def write_discovery_cache(state: dict[str, float]) -> None:
    """Atomically write the folder-name → mtime map."""
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    tmp = DISCOVERY_CACHE.with_suffix(".cache.tmp")
    tmp.write_text(json.dumps(state, indent=2), encoding="utf-8")
    tmp.replace(DISCOVERY_CACHE)
