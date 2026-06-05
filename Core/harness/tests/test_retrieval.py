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


# workspaces: removed in config-file-backed redesign (2026-06-05) —
# the fixture previously activated a workspace via the deleted
# `workspaces` module and indexed files into it so hybrid_search /
# retrieve had a corpus to rank. Rust now owns workspace file RAG, so
# hybrid_search yields no Python-side hits; the hit-asserting tests
# (test_hybrid_returns_ranked_hits, test_rerank_is_no_op_without_
# sentence_transformers, test_retrieve_pipeline_labels_citations,
# test_format_for_prompt_emits_citations) were removed with it.
#
# The query-expansion (HyDE) helper is workspace-independent and still
# covered below via a lightweight fixture that only sets up the
# retrieval module + a fake embedder.
@pytest.fixture
def retrieval_module(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))
    try:
        _common = importlib.import_module("Core.harness.memory._common")
        embeddings = importlib.import_module("Core.harness.memory.embeddings")
        retrieval = importlib.import_module("Core.harness.memory.retrieval")
    except ImportError:  # pragma: no cover
        _common = importlib.import_module("Wylde.Core.harness.memory._common")
        embeddings = importlib.import_module("Wylde.Core.harness.memory.embeddings")
        retrieval = importlib.import_module("Wylde.Core.harness.memory.retrieval")
    importlib.reload(_common)
    importlib.reload(embeddings)
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

    return retrieval


def test_hyde_falls_back_to_raw_query_without_chat_fn(
    retrieval_module: Any,
) -> None:
    retrieval = retrieval_module
    assert (
        retrieval.hyde_query("how do I shut down?", chat_fn=None)
        == "how do I shut down?"
    )


def test_hyde_combines_with_query_when_chat_fn_provided(
    retrieval_module: Any,
) -> Any:
    retrieval = retrieval_module

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
