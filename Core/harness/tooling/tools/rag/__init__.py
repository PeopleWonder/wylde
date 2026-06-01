"""tools/rag/ — In-process RAG / memory tools.

These tools used to live in the ``wylde-rag`` Flask service and were called
over HTTP from the orchestrator. The service has been dissolved into
:mod:`Wylde.Core.harness.memory` (rag, memgraph, vector_store, ingest), and
these tools are the LLM-facing surface that exercises that layer.

The set surfaced here is the **explicit-call** subset only — tools an LLM
can choose to invoke. Memory pipeline ingress (rag_query / graphrag_query /
memory_save / memory_search) is auto-managed and lives elsewhere.

Pulled forward from ``_legacy/core/wylde-rag/tools/`` per Phase 6 plan:

* ``rag_ask``         — semantic Q&A with citations
* ``rag_index``       — incremental indexing of one or more paths
* ``rag_reindex``     — full re-index from scratch
* ``rag_feedback``    — record user feedback on a prior answer
* ``rag_misses``      — list recent retrieval misses
* ``rag_chunk_usage`` — chunk citation frequency
* ``rag_graph_stats`` — knowledge-graph node/edge counts
* ``rag_prune``       — conditional delete from the vector store

Deliberately NOT pulled forward: ``rag_status`` (health goes via error code,
not a tool), ``graph_query`` (already in tools/meta/), and the
``rag_query``/``graphrag_query``/``memory_save``/``memory_search`` quartet
(AUTO_MANAGED — the memory pipeline drives them, not the LLM).
"""
