"""File indexing — full + delta walks, chunking, LanceDB IO.

The indexer reads the workspace folder, splits text-ish files into
overlapping chunks, embeds them, and writes rows into the per-
workspace ``files.lance`` table. Binary / oversized / VCS / venv
files are skipped on a best-effort basis so the indexer never blocks
on a multi-MB PDF or video.

Two entry points:

* :func:`_index_full` drops the existing table and re-embeds
  everything. Triggered by the GUI "Reindex" button and by first-
  time activation of a new workspace path.
* :func:`_index_delta` only re-embeds files whose mtime is newer
  than the cached entry. Triggered automatically by
  :func:`Core.harness.memory.workspaces.activate` for already-known
  workspaces.

The :func:`status` helper exposes the per-workspace indexing snapshot
that the GUI polls while a reindex is in flight.
"""

from __future__ import annotations

import hashlib
import logging
import os
import shutil
from pathlib import Path
from typing import Any, Dict, Iterator, List

from .._common import EMBED_DIM, ensure_dir
from ._store import (
    INDEXES_DIR,
    Workspace,
    _set_indexing,
    _update_workspace_metadata,
    get_workspace,
)

logger = logging.getLogger("wylde.harness.memory.workspaces")

# Soft-cap text-file size at 1 MB. Bigger files get logged-and-skipped
# rather than crashing the embedder with a multi-MB chunk.
_MAX_INDEXABLE_BYTES = 1 * 1024 * 1024
# Chunk boundary for long files. Embedders cap at ~512–1024 tokens; 4 KB
# of text is comfortably under that for english-ish content.
_CHUNK_SIZE_CHARS = 4000
_CHUNK_OVERLAP_CHARS = 200

# Files we never try to read — covers bytecode caches, VCS metadata,
# and the obvious binary-blob extensions the binary-sniff would catch
# anyway. Matched by suffix on the file's own path.
_SKIP_SUFFIXES = frozenset(
    {
        ".pyc",
        ".pyo",
        ".class",
        ".o",
        ".obj",
        ".dll",
        ".so",
        ".dylib",
        ".exe",
        ".bin",
        ".pdb",
        ".jpg",
        ".jpeg",
        ".png",
        ".gif",
        ".bmp",
        ".webp",
        ".tiff",
        ".ico",
        ".mp3",
        ".mp4",
        ".m4a",
        ".mov",
        ".avi",
        ".mkv",
        ".webm",
        ".zip",
        ".tar",
        ".gz",
        ".7z",
        ".rar",
        ".pdf",
        ".doc",
        ".docx",
        ".xls",
        ".xlsx",
        ".ppt",
        ".pptx",
    }
)
_SKIP_DIR_NAMES = frozenset(
    {
        "__pycache__",
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        "venv",
        ".venv",
        "env",
        ".env",
        "dist",
        "build",
        "target",
        ".pytest_cache",
        ".mypy_cache",
        ".tox",
        ".idea",
        ".vscode",
    }
)


# ── Index dir + table ──────────────────────────────────────────────────


def workspace_index_dir(workspace_id: str) -> Path:
    return ensure_dir(INDEXES_DIR / workspace_id)


def _files_table(workspace_id: str) -> Any:
    """Open / create the per-workspace files table. Lazy-imports lancedb."""
    import lancedb
    import pyarrow as pa

    workspace_dir = workspace_index_dir(workspace_id)
    db = lancedb.connect(str(workspace_dir))
    # lancedb 0.30 list_tables() returns a ListTablesResponse object
    # (.tables list + .page_token), not a plain list of strings.
    if "files" in db.list_tables().tables:
        return db.open_table("files")
    schema = pa.schema(
        [
            pa.field("id", pa.string()),
            pa.field("path", pa.string()),
            pa.field("chunk_idx", pa.int32()),
            pa.field("content", pa.string()),
            pa.field("mtime", pa.float64()),
            pa.field("vector", pa.list_(pa.float32(), EMBED_DIM)),
        ]
    )
    return db.create_table("files", schema=schema)


def status(workspace_id: str) -> Dict[str, Any]:
    """Indexing snapshot: file count, mtime of last index, indexing flag."""
    w = get_workspace(workspace_id)
    if w is None:
        return {"id": workspace_id, "exists": False}
    return {
        "id": w.id,
        "path": w.path,
        "exists": True,
        "file_count": w.file_count,
        "last_indexed_at": w.last_indexed_at,
        "last_activated_at": w.last_activated_at,
        "indexing": w.indexing,
    }


# ── Public reindex entrypoints ─────────────────────────────────────────


def reindex_workspace(workspace_id: str) -> Workspace:
    """Force a full reindex regardless of mtime. The "Reindex" button."""
    w = get_workspace(workspace_id)
    if w is None:
        raise ValueError(f"unknown workspace {workspace_id!r}")
    _index_full(w)
    return w


def refresh_workspace(workspace_id: str) -> Workspace:
    """Delta refresh — only re-embed files changed since last index."""
    w = get_workspace(workspace_id)
    if w is None:
        raise ValueError(f"unknown workspace {workspace_id!r}")
    _index_delta(w)
    return w


# ── Internal: full vs delta passes ────────────────────────────────────


def _index_full(workspace: Workspace) -> None:
    """Drop existing rows and re-index every file in the folder."""
    _set_indexing(workspace.id, True)
    try:
        # Delete the table by removing the dir; the next _files_table
        # call recreates it. Cheaper than per-row delete on big trees.
        idx_dir = workspace_index_dir(workspace.id)
        files_dir = idx_dir / "files.lance"
        if files_dir.exists():
            try:
                shutil.rmtree(files_dir)
            except Exception:  # noqa: BLE001
                logger.warning(
                    "workspaces: could not drop files.lance for %s", workspace.id
                )

        rows = list(_walk_and_chunk(workspace.path))
        _embed_and_write(workspace.id, rows)
        _update_workspace_metadata(workspace.id, file_count=_count_unique_paths(rows))
        logger.info(
            "workspaces: full index of %s — %d chunks", workspace.path, len(rows)
        )
    finally:
        _set_indexing(workspace.id, False)


def _index_delta(workspace: Workspace) -> None:
    """Re-embed files whose mtime is newer than the cached entry."""
    _set_indexing(workspace.id, True)
    try:
        # Existing chunk index keyed by path -> {(chunk_idx, mtime)}.
        cached = _existing_path_mtimes(workspace.id)

        new_rows: List[Dict[str, Any]] = []
        live_paths: set = set()
        for row in _walk_and_chunk(workspace.path):
            live_paths.add(row["path"])
            cached_mtime = cached.get(row["path"])
            if cached_mtime is not None and cached_mtime >= row["mtime"] - 0.001:
                continue  # unchanged, skip
            new_rows.append(row)

        # Drop chunks for files that have disappeared from the folder.
        gone = [p for p in cached if p not in live_paths]
        if gone:
            _delete_paths(workspace.id, gone)
            logger.info(
                "workspaces: removed %d gone files from %s", len(gone), workspace.id
            )

        # Re-embed the changed chunks. We delete-then-add per path so old
        # chunk_idx rows for a shrunk file don't linger.
        if new_rows:
            changed_paths = sorted({r["path"] for r in new_rows})
            _delete_paths(workspace.id, changed_paths)
            _embed_and_write(workspace.id, new_rows)

        # File count: number of distinct paths that have at least one row.
        _update_workspace_metadata(
            workspace.id,
            file_count=len(live_paths),
        )
        logger.info(
            "workspaces: delta index of %s — %d chunks updated, %d files removed",
            workspace.path,
            len(new_rows),
            len(gone),
        )
    finally:
        _set_indexing(workspace.id, False)


def _existing_path_mtimes(workspace_id: str) -> Dict[str, float]:
    """Best-effort: return ``{path: max(mtime)}`` for already-indexed files."""
    try:
        tbl = _files_table(workspace_id)
        rows = tbl.search().limit(100_000).to_list()
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "workspaces: existing-path scan failed for %s: %s", workspace_id, exc
        )
        return {}
    out: Dict[str, float] = {}
    for r in rows:
        path = r.get("path") or ""
        mtime = float(r.get("mtime") or 0.0)
        if not path:
            continue
        if mtime > out.get(path, -1.0):
            out[path] = mtime
    return out


def _delete_paths(workspace_id: str, paths: List[str]) -> None:
    if not paths:
        return
    try:
        tbl = _files_table(workspace_id)
        # LanceDB filter syntax — single-quoted strings.
        for p in paths:
            safe = p.replace("'", "''")
            try:
                tbl.delete(f"path = '{safe}'")
            except Exception:  # noqa: BLE001
                logger.debug("workspaces: delete %r noop", p)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: bulk delete failed for %s: %s", workspace_id, exc)


def _embed_and_write(workspace_id: str, rows: List[Dict[str, Any]]) -> None:
    if not rows:
        return
    from ..embeddings import embed

    try:
        vectors = embed([r["content"] for r in rows])
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: embed failed for %s: %s", workspace_id, exc)
        return
    if len(vectors) != len(rows):
        logger.warning(
            "workspaces: embed returned %d vectors for %d rows — skipping",
            len(vectors),
            len(rows),
        )
        return

    enriched = []
    for row, vec in zip(rows, vectors):
        rid = hashlib.sha256(
            f"{row['path']}::{row['chunk_idx']}::{row['mtime']}".encode("utf-8")
        ).hexdigest()[:16]
        enriched.append(
            {
                "id": rid,
                "path": row["path"],
                "chunk_idx": int(row["chunk_idx"]),
                "content": row["content"],
                "mtime": float(row["mtime"]),
                "vector": [float(x) for x in vec],
            }
        )
    try:
        tbl = _files_table(workspace_id)
        tbl.add(enriched)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: write failed for %s: %s", workspace_id, exc)


def _count_unique_paths(rows: List[Dict[str, Any]]) -> int:
    return len({r["path"] for r in rows})


# ── Walk + chunk ──────────────────────────────────────────────────────


def _walk_and_chunk(workspace_path: str) -> Iterator[Dict[str, Any]]:
    """Yield ``{path, chunk_idx, content, mtime}`` for every indexable
    chunk under the workspace folder. Skips binary / oversized / hidden
    / VCS / venv directories on a best-effort basis."""
    root = Path(workspace_path)
    for dirpath, dirnames, filenames in os.walk(root):
        # Prune skip-dirs in place so os.walk doesn't descend.
        dirnames[:] = [
            d for d in dirnames if d not in _SKIP_DIR_NAMES and not d.startswith(".")
        ]
        for fname in filenames:
            if fname.startswith("."):
                continue
            fpath = Path(dirpath) / fname
            if fpath.suffix.lower() in _SKIP_SUFFIXES:
                continue
            try:
                stat = fpath.stat()
            except OSError:
                continue
            if stat.st_size == 0 or stat.st_size > _MAX_INDEXABLE_BYTES:
                if stat.st_size > _MAX_INDEXABLE_BYTES:
                    logger.debug(
                        "workspaces: skip oversized %s (%d bytes)", fpath, stat.st_size
                    )
                continue
            try:
                raw = fpath.read_bytes()
            except OSError as exc:
                logger.debug("workspaces: skip unreadable %s: %s", fpath, exc)
                continue
            if b"\x00" in raw[:1024]:
                # Binary sniff — NUL byte in the first 1 KB is a strong
                # signal of a non-text file the embedder shouldn't see.
                continue
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                try:
                    text = raw.decode("utf-8", errors="replace")
                except Exception:  # noqa: BLE001
                    continue
            if not text.strip():
                continue
            relpath = str(fpath.resolve())
            for chunk_idx, chunk in enumerate(_chunk_text(text)):
                yield {
                    "path": relpath,
                    "chunk_idx": chunk_idx,
                    "content": chunk,
                    "mtime": stat.st_mtime,
                }


def _chunk_text(text: str) -> List[str]:
    """Naive overlapping chunker. Good enough for small files; long
    files get a few overlapping windows so the embedder sees context
    spanning section boundaries."""
    if len(text) <= _CHUNK_SIZE_CHARS:
        return [text]
    chunks: List[str] = []
    start = 0
    step = _CHUNK_SIZE_CHARS - _CHUNK_OVERLAP_CHARS
    while start < len(text):
        chunks.append(text[start : start + _CHUNK_SIZE_CHARS])
        start += step
    return chunks
