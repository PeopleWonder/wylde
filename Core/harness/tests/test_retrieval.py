"""Smoke for the retrieval pipeline (HyDE + hybrid + rerank + citations).

Cross-encoder rerank is skipped in this env (sentence-transformers
isn't installed) — the test asserts the pipeline degrades gracefully
without it. HyDE is exercised via a synthetic chat_fn that returns a
canned hypothetical answer.
"""

from __future__ import annotations

from typing import Any

import importlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def workspace_with_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    pytest.importorskip("lancedb")
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))
    try:
        _common = importlib.import_module("Core.harness.memory._common")

        embeddings = importlib.import_module("Core.harness.memory.embeddings")

        workspaces = importlib.import_module("Core.harness.memory.workspaces")

        retrieval = importlib.import_module("Core.harness.memory.retrieval")

    except ImportError:  # pragma: no cover
        _common = importlib.import_module("Wylde.Core.harness.memory._common")

        embeddings = importlib.import_module("Wylde.Core.harness.memory.embeddings")

        workspaces = importlib.import_module("Wylde.Core.harness.memory.workspaces")

        retrieval = importlib.import_module("Wylde.Core.harness.memory.retrieval")
    importlib.reload(_common)
    importlib.reload(embeddings)
    importlib.reload(workspaces)
    importlib.reload(retrieval)

    dim = _common.EMBED_DIM

    def _fake_embed(texts: Any) -> Any:
        out = []
        for t in texts:
            t = str(t or "")
            v = [0.05] * dim
            v[0] = (sum(ord(c) for c in t.lower()) % 53) / 53.0
            v[1] = (len(t) % 31) / 31.0
            out.append(v)
        return out

    monkeypatch.setattr(embeddings, "embed", _fake_embed)
    monkeypatch.setattr(embeddings, "embed_one", lambda t: _fake_embed([t])[0])

    folder = tmp_path / "code"
    folder.mkdir()
    (folder / "lifecycle.py").write_text(
        "def shutdown_all(): pass  # gracefully drains every running service",
        encoding="utf-8",
    )
    (folder / "harness.md").write_text(
        "The harness pipe exposes chat.start_turn / chat.run_turn / chat.stream_turn",
        encoding="utf-8",
    )
    (folder / "memgraph.txt").write_text(
        "Memgraph runs Neo4j on bolt://127.0.0.1:7687 and serves the graph pipe",
        encoding="utf-8",
    )

    record = workspaces.activate(str(folder))
    return retrieval, record.id


def test_hyde_falls_back_to_raw_query_without_chat_fn(
    workspace_with_files: Any,
) -> None:
    retrieval, _ = workspace_with_files
    assert (
        retrieval.hyde_query("how do I shut down?", chat_fn=None)
        == "how do I shut down?"
    )


def test_hyde_combines_with_query_when_chat_fn_provided(
    workspace_with_files: Any,
) -> Any:
    retrieval, _ = workspace_with_files

    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    def fake_chat(*, messages: Any, tools: Any, model: Any, **_: Any) -> Any:
        return _Step(
            "Run the lifecycle daemon's shutdown_all action to drain services cleanly."
        )

    expanded = retrieval.hyde_query("how do I shut down?", chat_fn=fake_chat)
    assert "how do I shut down?" in expanded
    assert "shutdown_all" in expanded


def test_hybrid_returns_ranked_hits(workspace_with_files: Any) -> None:
    retrieval, ws_id = workspace_with_files
    hits = retrieval.hybrid_search(
        ws_id, "shutdown drains services", raw_query="shutdown drains services", limit=3
    )
    assert hits, "expected at least one hybrid hit"
    paths = [h.path for h in hits]
    # The lifecycle.py file is the most lexically-matching one.
    assert any("lifecycle" in p for p in paths)


def test_rerank_is_no_op_without_sentence_transformers(
    workspace_with_files: Any,
) -> None:
    retrieval, ws_id = workspace_with_files
    hits = retrieval.hybrid_search(ws_id, "shutdown", raw_query="shutdown", limit=3)
    # Without sentence-transformers installed, rerank should pass the
    # list through unchanged (no exception, same length).
    reranked = retrieval.rerank("shutdown", hits)
    assert len(reranked) == len(hits)


def test_retrieve_pipeline_labels_citations(workspace_with_files: Any) -> None:
    retrieval, ws_id = workspace_with_files
    hits = retrieval.retrieve(ws_id, "memgraph bolt port", limit=2)
    assert hits
    labels = [h.citation_label for h in hits]
    assert labels[0] == "[1]"
    if len(labels) > 1:
        assert labels[1] == "[2]"


def test_format_for_prompt_emits_citations(workspace_with_files: Any) -> None:
    retrieval, ws_id = workspace_with_files
    hits = retrieval.retrieve(ws_id, "harness pipe actions", limit=2)
    block = retrieval.format_for_prompt(hits)
    assert "[1]" in block
