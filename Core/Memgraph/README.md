# Wylde Graph

Graph service: a thin supervisor around **vendored Neo4j Community** (Bolt/Cypher), hosting the **code graph** — entities and relations extracted from workspace code and chunk ingest. Despite the historical "Memgraph" name, the engine is Neo4j and the contents are code-structure data; there is no memory-to-memory schema today (memory records reach the graph only as entity edges from workspace saves). See `outputs/wylde-memory-fixes-plan.md` M9 for the deferred memory-edges design.

- Transport: `\\.\pipe\wylde-memgraph` (Neo4j Bolt port 7687)
- Run: `python run.py`
- Install: `pip install -r requirements.txt`
