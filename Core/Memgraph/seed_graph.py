"""seed_graph.py — parse the Wylde service topology out of the docs.

Parses the human-curated docs (docs/ENDPOINTS.md +
docs/protocols/STANDARD_PROTOCOLS_OVERVIEW.md) and upserts the matching
Service / Endpoint / Protocol / Registry / External nodes plus their
structural relationships (HAS_ENDPOINT, REGISTERS_WITH) into the graph DB
at GRAPH_URL (default bolt://localhost:7687, targeting Neo4j Community
Edition).

The script is idempotent, every write uses MERGE — so it can be re-run any
time the docs change without producing duplicates.

History: the semantic-topology edge writers (``write_relate`` /
``write_unrelate`` and the GOVERNS / DEPENDS_ON / USES / IMPLEMENTS /
PART_OF / REPLACES edges they wrote) were the rollback path after the
2026-05-26 direct-Bolt cutover, and the ``/seed`` HTTP route + ``seed_graph``
CLI that drove them. All three were deleted in the 2026-05-30 Memgraph
cleanup slice (the harness now owns every graph write over Bolt). What
remains here is the pure topology parser plus the node / structural-edge
seeders, kept as a library for any future re-wiring.

Structural sub-component edges (HAS_ENDPOINT, REGISTERS_WITH) are not in the
relationship-protocol predicate vocabulary because the vocabulary covers
semantic topology, not container/child structure.
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple

logger = logging.getLogger(__name__)


# ── Paths ────────────────────────────────────────────────────────────────────

# core/wylde-rag/seed_graph.py → repo root is two levels up.
REPO_ROOT = Path(__file__).resolve().parents[2]
DOCS_DIR = REPO_ROOT / "docs"
ENDPOINTS_MD = DOCS_DIR / "ENDPOINTS.md"
PROTOCOLS_OVERVIEW_MD = DOCS_DIR / "protocols" / "STANDARD_PROTOCOLS_OVERVIEW.md"
PROTOCOLS_DIR = DOCS_DIR / "protocols"


# ── Section-name allow-list for the parser ───────────────────────────────────

# Headings inside ENDPOINTS.md that are not service definitions.
NON_SERVICE_HEADINGS = {
    "Conventions",
    "Port summary",
    "Consul / mDNS registration body, the contract",
}


# ── Data shapes ──────────────────────────────────────────────────────────────


@dataclass
class Endpoint:
    method: str
    path: str
    description: str

    @property
    def stable_id(self) -> str:
        # method may be "GET, POST"; normalise so reads are deterministic.
        m = ",".join(sorted(p.strip() for p in self.method.split(",")))
        return f"{m}|{self.path}"


@dataclass
class Service:
    name: str
    pipe: Optional[str] = None
    port: Optional[int] = None
    category: Optional[str] = None
    framework: Optional[str] = None
    consul_tags: List[str] = field(default_factory=list)
    endpoints: List[Endpoint] = field(default_factory=list)


@dataclass
class Protocol:
    name: str  # canonical token used in edges, e.g. SERVICE_SHUTDOWN_PROTOCOL
    title: (
        str  # human-readable title from the overview, e.g. "Service Shutdown Protocol"
    )
    doc: str  # repo-relative path to the in-depth protocol doc


@dataclass
class ProxyRoute:
    prefix: str
    upstream: str
    transport: str  # "Named pipe" | "HTTP" | ...


# ── Parsing ──────────────────────────────────────────────────────────────────

_SECTION_RE = re.compile(r"^## (.+)$", re.MULTILINE)
_PIPE_RE = re.compile(r"\*\*Pipe:\*\*\s*`([^`]+)`")
_PORT_RE = re.compile(r"\*\*HTTP port:\*\*\s*(\d+)")
_FRAMEWORK_RE = re.compile(r"\*\*Framework:\*\*\s*([^\n]+)")
_CONSUL_TAGS_RE = re.compile(r"tags\s*`?\[([^\]]+)\]`?", re.IGNORECASE)
_TABLE_ROW_RE = re.compile(r"^\|([^|]+)\|([^|]+)\|([^|\n]+)\|\s*$", re.MULTILINE)

_HTTP_METHODS = {"GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"}


def _is_method_cell(cell: str) -> bool:
    """True iff `cell` looks like an HTTP-method list (GET, POST, PATCH, ...)."""
    parts = [p.strip().upper() for p in cell.split(",") if p.strip()]
    return bool(parts) and all(p in _HTTP_METHODS for p in parts)


def _split_endpoints_sections(text: str) -> Dict[str, str]:
    """Return a {heading: body} map for every `## ...` section."""
    sections: Dict[str, str] = {}
    parts = _SECTION_RE.split(text)
    # parts[0] is the preamble; the rest alternates heading / body.
    for i in range(1, len(parts), 2):
        name = parts[i].strip()
        body = parts[i + 1] if i + 1 < len(parts) else ""
        sections[name] = body
    return sections


def _parse_endpoint_table(body: str) -> List[Endpoint]:
    """Parse the Method/Path/Description table inside a service section.

    The table body always starts after a header row of the form
    `| Method | Path | Description |`; we locate it and consume rows until
    a non-row line is hit.
    """
    out: List[Endpoint] = []
    seen_header = False
    for m in _TABLE_ROW_RE.finditer(body):
        method, path, desc = (c.strip() for c in m.groups())
        if not seen_header:
            if method.lower() == "method":
                seen_header = True
            continue
        # Delimiter row "|---|---|---|" — skip.
        if set(method) <= set("-:") and set(path) <= set("-:"):
            continue
        # Stop as soon as we leave the endpoint table (e.g. fletch-web's
        # proxy table follows immediately and would otherwise be misread
        # as endpoints).
        if not _is_method_cell(method):
            break
        out.append(Endpoint(method=method, path=path.strip("`"), description=desc))
    return out


def _parse_proxy_table(body: str) -> List[ProxyRoute]:
    """Parse the fletch-web reverse-proxy table.

    The table header is `| Prefix | Upstream service | Transport |`. We locate
    the section by searching for that exact header inside the fletch-web body.
    """
    out: List[ProxyRoute] = []
    seen_header = False
    for m in _TABLE_ROW_RE.finditer(body):
        prefix, upstream, transport = (c.strip() for c in m.groups())
        if not seen_header:
            if prefix.lower() == "prefix" and upstream.lower().startswith("upstream"):
                seen_header = True
            continue
        if set(prefix) <= set("-:") and set(upstream) <= set("-:"):
            continue
        prefix = prefix.strip("`")
        out.append(ProxyRoute(prefix=prefix, upstream=upstream, transport=transport))
    return out


def _parse_consul_tags(body: str) -> List[str]:
    m = _CONSUL_TAGS_RE.search(body)
    if not m:
        return []
    raw = m.group(1)
    return [t.strip().strip('"').strip("'") for t in raw.split(",") if t.strip()]


def _category_from_tags(tags: List[str]) -> Optional[str]:
    """The tag that isn't 'wylde' and isn't 'ipc=pipe' is the category."""
    for t in tags:
        if t == "wylde" or t.startswith("ipc="):
            continue
        return t
    return None


def parse_endpoints_md(text: str) -> Tuple[List[Service], List[ProxyRoute]]:
    """Parse ENDPOINTS.md into (services, proxy_routes).

    fletch-web's proxy table is returned separately because it represents a
    cross-service relationship (fletch-web USES upstream X) rather than a
    list of fletch-web's own endpoints.
    """
    sections = _split_endpoints_sections(text)
    services: List[Service] = []
    proxy_routes: List[ProxyRoute] = []

    for heading, body in sections.items():
        if heading in NON_SERVICE_HEADINGS:
            continue
        svc = Service(name=heading.strip())
        if pm := _PIPE_RE.search(body):
            svc.pipe = pm.group(1)
        if pm := _PORT_RE.search(body):
            svc.port = int(pm.group(1))
        if pm := _FRAMEWORK_RE.search(body):
            svc.framework = pm.group(1).strip()
        svc.consul_tags = _parse_consul_tags(body)
        svc.category = _category_from_tags(svc.consul_tags)
        svc.endpoints = _parse_endpoint_table(body)
        services.append(svc)

        if svc.name == "fletch-web":
            proxy_routes = _parse_proxy_table(body)

    return services, proxy_routes


_PROTO_LINE_RE = re.compile(r"^\d+\.\s+\*\*\[([^\]]+)\]\(([^)]+)\)\*\*", re.MULTILINE)


def parse_protocols_overview(text: str) -> List[Protocol]:
    """Extract Protocol entries from STANDARD_PROTOCOLS_OVERVIEW.md.

    Each numbered entry looks like:
      `1. **[Service Shutdown Protocol](SERVICE_SHUTDOWN_PROTOCOL.md)**, ...`
    The link target gives us the doc path; the link text gives us the title.
    The canonical edge token is the file stem (e.g. SERVICE_SHUTDOWN_PROTOCOL).
    """
    out: List[Protocol] = []
    for m in _PROTO_LINE_RE.finditer(text):
        title = m.group(1).strip()
        href = m.group(2).strip()
        # Strip any leading "./" and resolve to repo-relative path.
        href = href.lstrip("./")
        # Slug = file stem in upper-snake — matches the metadata label
        # convention each in-depth doc uses for itself.
        slug = Path(href).stem.upper()
        doc_path = f"docs/protocols/{href}" if "/" not in href else f"docs/{href}"
        out.append(Protocol(name=slug, title=title, doc=doc_path))
    return out


# ── Driver helper ────────────────────────────────────────────────────────────


def _open_driver(
    url: str, user: str = "", password: str = "", timeout_s: float = 5.0
) -> Any:
    """Open a Bolt driver. Returns the driver or raises a clear error."""
    from neo4j import GraphDatabase  # local import, keeps script importable

    auth = (user, password) if user else None
    drv = GraphDatabase.driver(url, auth=auth, connection_timeout=timeout_s)
    drv.verify_connectivity()
    return drv


# ── Cypher writers ───────────────────────────────────────────────────────────

# All topology nodes carry the umbrella :Node label so a generic
# /graph/relate(subject, object) call can MERGE on (n:Node {name}) regardless
# of whether the node was originally seeded as a Service / Protocol / etc.
# Specific labels (Service, Protocol, Endpoint, Registry, External) are added
# on top via SET so typed queries still work.

_SERVICE_CYPHER = """
UNWIND $batch AS row
MERGE (s:Node {name: row.name})
SET s:Service,
    s.pipe      = row.pipe,
    s.port      = row.port,
    s.category  = row.category,
    s.framework = row.framework,
    s.tags      = row.tags
"""

_ENDPOINT_CYPHER = """
UNWIND $batch AS row
MERGE (e:Endpoint {id: row.id})
SET e.service     = row.service,
    e.method      = row.method,
    e.path        = row.path,
    e.description = row.description
WITH e, row
MATCH (s:Service {name: row.service})
MERGE (s)-[:HAS_ENDPOINT]->(e)
"""

_PROTOCOL_CYPHER = """
UNWIND $batch AS row
MERGE (p:Node {name: row.name})
SET p:Protocol,
    p.title = row.title,
    p.doc   = row.doc
"""

_REGISTRY_CYPHER = """
MERGE (r:Node {name: $name})
SET r:Registry
WITH r
UNWIND $services AS sname
MATCH (s:Service {name: sname})
MERGE (s)-[:REGISTERS_WITH]->(r)
"""

_EXTERNAL_CYPHER = """
UNWIND $names AS n
MERGE (x:Node {name: n})
SET x:External
"""


def _write_services(session: Any, services: List[Service]) -> int:
    payload = [
        {
            "name": s.name,
            "pipe": s.pipe,
            "port": s.port,
            "category": s.category,
            "framework": s.framework,
            "tags": s.consul_tags,
        }
        for s in services
    ]
    session.run(_SERVICE_CYPHER, batch=payload).consume()
    return len(payload)


def _write_endpoints(session: Any, services: List[Service]) -> int:
    payload: List[Dict[str, Any]] = []
    for s in services:
        for ep in s.endpoints:
            payload.append(
                {
                    "id": f"{s.name}|{ep.stable_id}",
                    "service": s.name,
                    "method": ep.method,
                    "path": ep.path,
                    "description": ep.description,
                }
            )
    if not payload:
        return 0
    session.run(_ENDPOINT_CYPHER, batch=payload).consume()
    return len(payload)


def _write_protocols(session: Any, protocols: List[Protocol]) -> int:
    payload = [{"name": p.name, "title": p.title, "doc": p.doc} for p in protocols]
    session.run(_PROTOCOL_CYPHER, batch=payload).consume()
    return len(payload)


def _write_registry(session: Any, services: List[Service]) -> int:
    sname = [s.name for s in services]
    session.run(_REGISTRY_CYPHER, name="consul", services=sname).consume()
    return len(sname)


def _write_externals(session: Any, names: Iterable[str]) -> int:
    names_list = sorted(set(names))
    if not names_list:
        return 0
    session.run(_EXTERNAL_CYPHER, names=names_list).consume()
    return len(names_list)


# ── Top-level orchestration ──────────────────────────────────────────────────


def seed_all(
    url: str = "bolt://localhost:7687",
    user: str = "",
    password: str = "",
    docs_dir: Optional[Path] = None,
) -> Dict[str, int]:
    """Parse the docs and write the node topology. Returns counts per kind.

    Writes Service / Endpoint / Protocol / External / Registry nodes plus the
    structural HAS_ENDPOINT and REGISTERS_WITH edges. The semantic-topology
    edges (GOVERNS / DEPENDS_ON / USES / IMPLEMENTS / PART_OF) that this used
    to write via the now-deleted ``write_relate`` helper were removed with the
    rollback-only Python graph path in the 2026-05-30 cleanup slice — the
    harness owns those writes over Bolt now.
    """
    docs = Path(docs_dir) if docs_dir else DOCS_DIR
    endpoints_md = docs / "ENDPOINTS.md"
    overview_md = docs / "protocols" / "STANDARD_PROTOCOLS_OVERVIEW.md"

    if not endpoints_md.exists():
        raise FileNotFoundError(endpoints_md)
    if not overview_md.exists():
        raise FileNotFoundError(overview_md)

    services, proxy_routes = parse_endpoints_md(
        endpoints_md.read_text(encoding="utf-8")
    )
    protocols = parse_protocols_overview(overview_md.read_text(encoding="utf-8"))

    service_names = {s.name for s in services}

    # External nodes are anything referenced by a relationship that isn't a
    # known service or protocol.
    # Map proxy-table external labels onto canonical external node names so
    # "n8n editor (external)" doesn't collide with the existing "n8n" node.
    EXTERNAL_ALIASES = {"n8n editor": "n8n", "ollama": "ollama"}

    def _canonical_external(label: str) -> str:
        base = label.split("(")[0].strip().lower()
        return EXTERNAL_ALIASES.get(base, base)

    referenced: List[str] = ["ollama", "neo4j", "lancedb", "nginx", "n8n"]
    for pr in proxy_routes:
        if "(external)" in pr.upstream.lower():
            referenced.append(_canonical_external(pr.upstream))
    externals = [n for n in referenced if n not in service_names]

    drv = _open_driver(url, user, password)
    counts: Dict[str, int] = {}
    try:
        with drv.session() as sess:
            counts["services"] = _write_services(sess, services)
            counts["endpoints"] = _write_endpoints(sess, services)
            counts["protocols"] = _write_protocols(sess, protocols)
            counts["externals"] = _write_externals(sess, externals)
            counts["consul_register"] = _write_registry(sess, services)
    finally:
        drv.close()

    return counts
