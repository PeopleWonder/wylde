"""Internal helpers shared by the harness memory layer.

The leading underscore signals this module is for use within
``Wylde/Core/harness/memory/`` only. External callers should not import from
here directly.

Right now the helpers cover three things:

* Path resolution (``WYLDE_ROOT``, ``DATA_DIR``, ``CONVERSATIONS_DIR``).
* The Memgraph service identity (pipe name + service name) used by
  :mod:`memgraph`.
* A tiny shared logger so every module under ``harness/memory/`` logs under
  the same dotted name (``wylde.harness.memory``).

If any module starts pulling helpers that don't belong to the others, split
them out — keep this file small and obvious.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

logger = logging.getLogger("wylde.harness.memory")


# ─── Paths ──────────────────────────────────────────────────────────────────

# Wylde/Core/harness/memory/_common.py → Wylde/
WYLDE_ROOT: Path = Path(__file__).resolve().parents[3]

# LanceDB and other on-disk stores. Falls back to Wylde/.wylde/data/ so a
# fresh checkout works without env. Env override stays available because
# users sometimes point storage at an external SSD.
DATA_DIR: Path = Path(
    os.getenv("WYLDE_DATA_DIR")
    or os.getenv("DATA_DIR")
    or (WYLDE_ROOT / ".wylde" / "data")
)

# One JSON file per conversation lives here.
CONVERSATIONS_DIR: Path = Path(
    os.getenv("CONVERSATIONS_DIR") or (DATA_DIR / "conversations")
)


def ensure_dir(p: Path) -> Path:
    """Create ``p`` (and parents) if missing. Returns the path back for chaining."""
    p.mkdir(parents=True, exist_ok=True)
    return p


# ─── Memgraph service identity ──────────────────────────────────────────────

# Memgraph migration shipped: the server registers as ``wylde-memgraph``
# (see ``Core/Memgraph/run.py:217-232``), and the Lifecycle daemon spawns
# it under that name in Phase 2c. The default below now matches the
# server. Override via ``WYLDE_MEMGRAPH_SERVICE`` if a non-standard
# deployment uses a different pipe name.
MEMGRAPH_SERVICE_NAME: str = os.getenv("WYLDE_MEMGRAPH_SERVICE", "wylde-memgraph")
MEMGRAPH_PIPE_NAME: str = rf"\\.\pipe\{MEMGRAPH_SERVICE_NAME}"


# ─── Embedding tunables ─────────────────────────────────────────────────────
#
# Legacy wylde-rag pulled these from a YAML config module. Phase 4b reads
# from env so the harness is portable — if Phase 4c centralises config, swap
# these constants for an import from there.

EMBED_MODEL: str = os.getenv("WYLDE_EMBED_MODEL", "nomic-embed-text")
EMBED_NATIVE_DIM: int = int(os.getenv("WYLDE_EMBED_NATIVE_DIM", "768"))
EMBED_DIM: int = int(os.getenv("WYLDE_EMBED_DIM", "768"))


# ─── Memory-store tunables ──────────────────────────────────────────────────

MEMORY_COLD_MAX_MB: float = float(os.getenv("WYLDE_MEMORY_COLD_MAX_MB", "256"))
MEMORY_CONSOLIDATION_THRESHOLD: int = int(
    os.getenv("WYLDE_MEMORY_CONSOLIDATION_THRESHOLD", "200")
)
MEMORY_CONSOLIDATION_SIMILARITY: float = float(
    os.getenv("WYLDE_MEMORY_CONSOLIDATION_SIMILARITY", "0.92")
)


__all__ = [
    "logger",
    "WYLDE_ROOT",
    "DATA_DIR",
    "CONVERSATIONS_DIR",
    "ensure_dir",
    "MEMGRAPH_SERVICE_NAME",
    "MEMGRAPH_PIPE_NAME",
    "EMBED_MODEL",
    "EMBED_NATIVE_DIM",
    "EMBED_DIM",
    "MEMORY_COLD_MAX_MB",
    "MEMORY_CONSOLIDATION_THRESHOLD",
    "MEMORY_CONSOLIDATION_SIMILARITY",
]
