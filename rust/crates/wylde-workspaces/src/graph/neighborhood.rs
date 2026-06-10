//! `workspaces.symbol_context` — the Slice G-data read verb (Phase 1).
//!
//! Given one focal symbol, return its **body** plus its k-hop neighbourhood:
//! who calls it, what it calls, the types it uses, and its file-siblings.
//! This is the structural-retrieval data the Phase 2 chat-turn driver
//! (Slice G) pulls per turn so the LLM reasons over real call/type edges
//! instead of vector-similarity guesses — "the data layer for the AI getting
//! smarter".
//!
//! ## What the graph can (and can't) give us
//!
//! The ingest graph (see [`super::cypher`] / [`super::query`] docs) persists
//! only an entity's **name** (its key) and, via `MENTIONED_IN`, the **file**
//! of a chunk it appears in. It stores **no per-entity line number** and no
//! kind. So this module derives what the graph lacks:
//!
//!   * **`file`** — borrowed from a chunk the entity is mentioned in
//!     (`min(c.path)`, deterministic, exactly like Slice B's node projection).
//!   * **`line` + `body`** — re-derived by reading the focal's file and
//!     locating its definition ([`locate_symbol_line`] + [`extract_body`]).
//!     Neighbours get `line = 0` (locating every neighbour's line would mean
//!     reading every neighbour's file — outside the per-hop budget; a Phase-2
//!     / Slice-F-data enrichment can fill it from the symbol index).
//!   * **`kind`** — a best-effort heuristic from the relationship that
//!     surfaced the symbol (CALLS participant → `Function`, INHERITS target →
//!     `Class`, IMPORTS target → `Module`), mirroring Slice B's edge-role
//!     heuristic. The focal's kind comes from its own outgoing type edges.
//!
//! ## The walk
//!
//! `hops` (default 1) controls only the **call-graph** traversal: callers are
//! a breadth-first expansion along incoming `CALLS`, callees along outgoing
//! `CALLS`, each newly-discovered symbol tagged with the hop level it was
//! first reached at (`hop_distance`). Each direction dedups against its own
//! seen-set, so a symbol is visited once per direction and the cost is
//! linear in the reachable sub-graph, not exponential in `hops`. `types_used`
//! and `siblings` are inherently 1-hop relationships of the focal and do not
//! expand with `hops`.
//!
//! ## Per-hop time budget (OI-1)
//!
//! The client's IPC timeout for this verb is `base 200ms + per_hop 300ms × N`
//! ([`wylde_workspaces_client::timeouts`]). The walk honours the *same*
//! formula server-side: it computes a deadline up front and stops expanding
//! once it's crossed, returning whatever depth it reached in
//! `hops_traversed` (which is also `< hops` when the call graph simply runs
//! dry). `took_ms` reports the measured wall time for OI-1 observability.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::graph::projection::{NodeKind, RelType};

/// Default hop depth when the caller omits it.
pub const DEFAULT_HOPS: u32 = 1;

/// Hard cap on hop depth. The per-hop time budget bounds the work already;
/// this is a belt-and-braces guard against a pathological request (the plan
/// budgets to 5 hops, §2.5). Requests above this are clamped, not rejected.
pub const MAX_HOPS: u32 = 6;

/// Server-side per-hop budget base — mirrors the client `symbol_context`
/// formula (`200ms + 300ms × N`) so the walk self-limits to the same window
/// the client times out at, surfacing partial depth instead of a hard abort.
pub const BUDGET_BASE: Duration = Duration::from_millis(200);
/// Server-side per-hop budget increment (see [`BUDGET_BASE`]).
pub const BUDGET_PER_HOP: Duration = Duration::from_millis(300);

/// The relationship by which a [`RelatedSymbol`] reached the focal symbol.
///
/// A superset of the graph's typed-edge vocabulary ([`RelType`]) plus
/// `SiblingOf`, which has **no** graph edge: siblings share a source file
/// with the focal (co-`MENTIONED_IN`), not a `CALLS`/`IMPORTS`/`INHERITS`
/// edge. Kept local to this module so the graph's [`RelType`] (consumed by
/// Slice B's projection and Slice F-data's index) stays the pure edge
/// vocabulary. Wire form is SCREAMING_SNAKE to match `type(r)` strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContextRel {
    Calls,
    Imports,
    Inherits,
    Configures,
    Exposes,
    SiblingOf,
}

impl ContextRel {
    /// Lift a graph [`RelType`] into the context vocabulary.
    pub fn from_rel(r: RelType) -> Self {
        match r {
            RelType::Calls => ContextRel::Calls,
            RelType::Imports => ContextRel::Imports,
            RelType::Inherits => ContextRel::Inherits,
            RelType::Configures => ContextRel::Configures,
            RelType::Exposes => ContextRel::Exposes,
        }
    }
}

/// The focal symbol the request is centred on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    /// Stable id — the entity name (the graph's node key; same as Slice B's
    /// `Node::id`).
    pub id: String,
    pub name: String,
    /// Best-effort kind from the focal's own type edges (see module docs).
    pub kind: NodeKind,
    /// A representative source file the symbol is defined/mentioned in.
    pub file: PathBuf,
    /// 1-based definition line, re-derived from the file (0 if not located /
    /// `include_body=false` / file unreadable).
    pub line: u32,
    /// The symbol's source body, or `None` when `include_body=false`, the
    /// file is unreadable, or the definition couldn't be located.
    pub body: Option<String>,
    /// Recent git blame over the body's lines (TBS Slice L) — per-commit,
    /// newest-first. `None` when `include_blame=false`, the body wasn't
    /// located, or the file isn't tracked in a git repository (fail-soft —
    /// blame is enrichment, never a failure). Computed by the live wrapper
    /// AFTER the walk, so the walk itself stays graph-pure and mock-testable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blame: Option<Vec<super::blame::BlameEntry>>,
}

/// A symbol related to the focal, with the relationship and hop distance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelatedSymbol {
    pub id: String,
    pub name: String,
    pub kind: NodeKind,
    pub file: PathBuf,
    /// 1-based line — always 0 for related symbols in v1 (see module docs).
    pub line: u32,
    /// How this symbol relates to the focal.
    pub rel_type: ContextRel,
    /// 1 for direct neighbours; 2+ for deeper call-graph reaches.
    pub hop_distance: u32,
}

/// The full structural context for one symbol — the verb's reply shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SymbolContext {
    /// The focal symbol (with body, when requested).
    pub symbol: Symbol,
    /// Entities that `CALLS` the focal (incoming), BFS to `hops`.
    pub callers: Vec<RelatedSymbol>,
    /// Entities the focal `CALLS` (outgoing), BFS to `hops`.
    pub callees: Vec<RelatedSymbol>,
    /// Entities the focal `INHERITS` or `IMPORTS` (1-hop).
    pub types_used: Vec<RelatedSymbol>,
    /// Entities sharing a source file with the focal (1-hop).
    pub siblings: Vec<RelatedSymbol>,
    /// Actual call-graph depth reached (may be `< hops` if the graph ran dry
    /// or the per-hop budget was hit).
    pub hops_traversed: u32,
    /// Measured wall time, for OI-1 perf observability.
    pub took_ms: u32,
}

// ── data-source abstraction (so the walk is unit-testable) ───────────────

/// One decoded `(name, file)` neighbour row. `file` is empty for an entity
/// not mentioned in the workspace (an external call/import target).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NeighborRow {
    pub name: String,
    pub file: String,
}

/// One decoded type-edge row: a target entity + the edge type that reached it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRow {
    pub name: String,
    pub file: String,
    pub rel: RelType,
}

/// The graph reads the neighbourhood walk needs. Implemented by the live
/// Bolt source ([`LiveSource`]) and, in tests, by an in-memory mock — so the
/// BFS hop-assignment + assembly are verified without a database.
///
/// Methods return `impl Future + Send` (RPITIT) to match the crate's other
/// pipe/graph traits ([`super::super::rag::indexer::graph_writer`]) and keep
/// the spawned action handler's future `Send`.
pub trait NeighborhoodSource {
    /// The focal's representative file, or `None` if it isn't mentioned in
    /// `workspace` at all (→ a `not_found` for the verb).
    fn focal_file(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, SourceError>> + Send;

    /// Direct callers of any name in `frontier` (incoming `CALLS`).
    fn callers_of(
        &self,
        workspace: &str,
        frontier: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send;

    /// Direct callees of any name in `frontier` (outgoing `CALLS`).
    fn callees_of(
        &self,
        workspace: &str,
        frontier: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send;

    /// Types the focal `INHERITS`/`IMPORTS` (1-hop).
    fn types_used_by(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<TypeRow>, SourceError>> + Send;

    /// Entities sharing a file with the focal (1-hop, co-`MENTIONED_IN`).
    fn siblings_of(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send;
}

/// A data-source failure carrying the underlying wire `code`/`message` so the
/// verb can preserve `bolt_*` codes for the client classifier (matching the
/// Slice B read path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError {
    pub code: String,
    pub message: String,
}

impl SourceError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

// ── the walk ─────────────────────────────────────────────────────────────

/// Clamp a requested hop count into `[1, MAX_HOPS]`, applying the default.
pub fn resolve_hops(requested: Option<u32>) -> u32 {
    requested.unwrap_or(DEFAULT_HOPS).clamp(1, MAX_HOPS)
}

/// The deadline for a `hops`-deep walk: `200ms + 300ms × hops` (OI-1).
fn budget_for(hops: u32) -> Duration {
    BUDGET_BASE + BUDGET_PER_HOP * hops
}

/// Walk the neighbourhood of `name` in `workspace` to `hops` depth over
/// `source`, then assemble the [`SymbolContext`]. Returns `Ok(None)` when the
/// focal symbol isn't in the workspace (→ a `not_found` reply).
///
/// `read_body` reads the focal's file when `include_body` is set; injected so
/// tests drive body extraction without touching the filesystem. In production
/// it's [`read_file_to_string`].
pub async fn walk<S, R>(
    source: &S,
    workspace: &str,
    name: &str,
    hops: u32,
    include_body: bool,
    read_body: R,
) -> Result<Option<SymbolContext>, SourceError>
where
    S: NeighborhoodSource + Sync,
    R: Fn(&str) -> Option<String>,
{
    let started = Instant::now();
    let hops = hops.clamp(1, MAX_HOPS);
    let deadline = budget_for(hops);

    // 1. Resolve the focal — `None` ⇒ not in this workspace.
    let Some(focal_file) = source.focal_file(workspace, name).await? else {
        return Ok(None);
    };

    // 2. 1-hop facets: types used + siblings.
    let type_rows = source.types_used_by(workspace, name).await?;
    let sibling_rows = source.siblings_of(workspace, name).await?;

    // 3. Call-graph BFS (callers + callees), independent seen-sets.
    let mut hops_traversed = 0u32;
    let callers = bfs_calls(
        source,
        workspace,
        name,
        hops,
        started,
        deadline,
        Direction::Callers,
        &mut hops_traversed,
    )
    .await?;
    let callees = bfs_calls(
        source,
        workspace,
        name,
        hops,
        started,
        deadline,
        Direction::Callees,
        &mut hops_traversed,
    )
    .await?;

    // 4. Focal body (only when asked + file readable + locatable).
    let (line, body) = if include_body {
        focal_body(&focal_file, name, &read_body)
    } else {
        (0, None)
    };

    let symbol = Symbol {
        kind: focal_kind(&type_rows),
        id: name.to_owned(),
        name: name.to_owned(),
        file: PathBuf::from(&focal_file),
        line,
        body,
        // Filled by the live wrapper (`symbol_context`) — see the field docs.
        blame: None,
    };

    let took_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

    Ok(Some(SymbolContext {
        symbol,
        callers,
        callees,
        types_used: project_types(type_rows),
        siblings: project_siblings(sibling_rows),
        hops_traversed,
        took_ms,
    }))
}

/// Traversal direction for [`bfs_calls`].
#[derive(Clone, Copy)]
enum Direction {
    Callers,
    Callees,
}

/// Breadth-first expansion along `CALLS` in one direction up to `hops`,
/// deduping against a per-direction seen-set and stopping early if the graph
/// runs dry or the per-hop deadline is crossed. Bumps `*hops_traversed` to the
/// deepest level that yielded a new symbol.
#[allow(clippy::too_many_arguments)]
async fn bfs_calls<S: NeighborhoodSource + Sync>(
    source: &S,
    workspace: &str,
    focal: &str,
    hops: u32,
    started: Instant,
    deadline: Duration,
    dir: Direction,
    hops_traversed: &mut u32,
) -> Result<Vec<RelatedSymbol>, SourceError> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(focal.to_owned());
    let mut frontier = vec![focal.to_owned()];
    let mut out: Vec<RelatedSymbol> = Vec::new();

    for hop in 1..=hops {
        if started.elapsed() >= deadline {
            break; // per-hop budget hit — surface partial depth.
        }
        let rows = match dir {
            Direction::Callers => source.callers_of(workspace, &frontier).await?,
            Direction::Callees => source.callees_of(workspace, &frontier).await?,
        };

        let mut next: Vec<String> = Vec::new();
        for r in rows {
            if r.name.is_empty() || !seen.insert(r.name.clone()) {
                continue;
            }
            out.push(RelatedSymbol {
                kind: NodeKind::Function, // CALLS participants are functions.
                id: r.name.clone(),
                name: r.name.clone(),
                file: PathBuf::from(r.file),
                line: 0,
                rel_type: ContextRel::Calls,
                hop_distance: hop,
            });
            next.push(r.name);
        }

        if next.is_empty() {
            break; // ran dry before `hops`.
        }
        *hops_traversed = (*hops_traversed).max(hop);
        next.sort();
        frontier = next;
    }

    out.sort_by(|a, b| (a.hop_distance, a.name.as_str()).cmp(&(b.hop_distance, b.name.as_str())));
    Ok(out)
}

/// Focal kind from its outgoing type edges, mirroring Slice B's heuristic:
/// an `IMPORTS` source → `Module`, else an `INHERITS` source → `Class`, else
/// `Function`.
fn focal_kind(types: &[TypeRow]) -> NodeKind {
    if types.iter().any(|t| t.rel == RelType::Imports) {
        NodeKind::Module
    } else if types.iter().any(|t| t.rel == RelType::Inherits) {
        NodeKind::Class
    } else {
        NodeKind::Function
    }
}

/// Project type-edge rows into 1-hop related symbols. `INHERITS` target →
/// `Class`, `IMPORTS` target → `Module`, others → `Function`. Deterministic
/// (sorted), deduped on `(name, rel)`.
fn project_types(rows: Vec<TypeRow>) -> Vec<RelatedSymbol> {
    let mut seen: BTreeSet<(String, ContextRel)> = BTreeSet::new();
    let mut out: Vec<RelatedSymbol> = Vec::new();
    for r in rows {
        if r.name.is_empty() {
            continue;
        }
        let rel = ContextRel::from_rel(r.rel);
        if !seen.insert((r.name.clone(), rel)) {
            continue;
        }
        let kind = match r.rel {
            RelType::Inherits => NodeKind::Class,
            RelType::Imports => NodeKind::Module,
            _ => NodeKind::Function,
        };
        out.push(RelatedSymbol {
            kind,
            id: r.name.clone(),
            name: r.name,
            file: PathBuf::from(r.file),
            line: 0,
            rel_type: rel,
            hop_distance: 1,
        });
    }
    out.sort_by(|a, b| {
        (a.name.as_str(), rel_wire(a.rel_type)).cmp(&(b.name.as_str(), rel_wire(b.rel_type)))
    });
    out
}

/// Project sibling rows into 1-hop `SiblingOf` related symbols. Kind is
/// unknown for siblings (no edge to classify by) → `Function` default.
fn project_siblings(rows: Vec<NeighborRow>) -> Vec<RelatedSymbol> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<RelatedSymbol> = Vec::new();
    for r in rows {
        if r.name.is_empty() || !seen.insert(r.name.clone()) {
            continue;
        }
        out.push(RelatedSymbol {
            kind: NodeKind::Function,
            id: r.name.clone(),
            name: r.name.clone(),
            file: PathBuf::from(r.file),
            line: 0,
            rel_type: ContextRel::SiblingOf,
            hop_distance: 1,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Stable wire string for a [`ContextRel`] (deterministic sort key).
fn rel_wire(r: ContextRel) -> &'static str {
    match r {
        ContextRel::Calls => "CALLS",
        ContextRel::Imports => "IMPORTS",
        ContextRel::Inherits => "INHERITS",
        ContextRel::Configures => "CONFIGURES",
        ContextRel::Exposes => "EXPOSES",
        ContextRel::SiblingOf => "SIBLING_OF",
    }
}

// ── body extraction ──────────────────────────────────────────────────────

/// Read a file to a string, or `None` if it can't be read (missing /
/// non-UTF-8 / permissions). The production `read_body` for [`walk`].
pub fn read_file_to_string(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Locate the focal's body in its file and return `(line, body)`.
/// `(0, None)` when the file is unreadable or the definition isn't found.
fn focal_body<R: Fn(&str) -> Option<String>>(
    file: &str,
    name: &str,
    read_body: &R,
) -> (u32, Option<String>) {
    if file.is_empty() {
        return (0, None);
    }
    let Some(contents) = read_body(file) else {
        return (0, None);
    };
    match locate_symbol_line(&contents, name) {
        Some(line) => (line, extract_body(&contents, line)),
        None => (0, None),
    }
}

/// Definition keywords across the languages ingest currently parses. A line
/// carrying one of these *and* the symbol name as a whole word is treated as
/// the definition site.
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "def",
    "class",
    "struct",
    "trait",
    "enum",
    "impl",
    "type",
    "const",
    "static",
    "func",
    "function",
    "interface",
    "module",
    "mod",
    "let",
    "var",
    "pub",
];

/// Find the 1-based line where `name` is defined, by a simple, language-
/// agnostic heuristic (better-bounded extraction via tree-sitter is a Phase 2
/// polish item — see module docs):
///   1. the first line containing `name` as a whole word **and** a definition
///      keyword, else
///   2. the first line containing `name` as a whole word at all.
///
/// `None` if the name never appears.
pub fn locate_symbol_line(contents: &str, name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    let mut first_mention: Option<u32> = None;
    for (idx, raw) in contents.lines().enumerate() {
        if !contains_word(raw, name) {
            continue;
        }
        let line = idx as u32 + 1;
        first_mention.get_or_insert(line);
        if has_def_keyword(raw) {
            return Some(line);
        }
    }
    first_mention
}

/// Extract a symbol body: the lines from `line` (1-based, inclusive) up to but
/// not including the next blank line, or end-of-file. `None` if `line` is out
/// of range. A deliberately simple v1 bound (module docs).
pub fn extract_body(contents: &str, line: u32) -> Option<String> {
    if line == 0 {
        return None;
    }
    let lines: Vec<&str> = contents.lines().collect();
    let start = (line - 1) as usize;
    if start >= lines.len() {
        return None;
    }
    let mut body: Vec<&str> = Vec::new();
    for &l in &lines[start..] {
        if l.trim().is_empty() {
            break;
        }
        body.push(l);
    }
    if body.is_empty() {
        return None;
    }
    Some(body.join("\n"))
}

/// True iff `word` appears in `haystack` bounded by non-identifier characters
/// (so `foo` doesn't match inside `foobar` / `do_foo`).
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// True if any whitespace-delimited token of `line` is a definition keyword.
fn has_def_keyword(line: &str) -> bool {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|tok| DEF_KEYWORDS.contains(&tok))
}

/// Identifier byte: `[A-Za-z0-9_]`. Names from tree-sitter are simple
/// identifiers (or `::`-joined module paths, whose segments are identifiers),
/// so ASCII bounds are sufficient for the whole-word check.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ── live Bolt source + the verb ──────────────────────────────────────────

use neo4rs::{query as cypher_query, Graph};
use serde_json::Value;
use wylde_shared::ipc::{IpcError, Reply};

use crate::graph::BoltClient;

/// Direct callers (incoming `CALLS`) of any name in `$frontier`, with a
/// representative file (`min(c.path)` over the workspace's chunks, or null for
/// an external symbol). One row per distinct caller.
const CALLERS_CYPHER: &str = "
UNWIND $frontier AS fname
MATCH (n:Entity)-[:CALLS]->(:Entity {name: fname})
OPTIONAL MATCH (n)-[:MENTIONED_IN]->(c:Chunk {workspace: $ws})
RETURN n.name AS name, min(c.path) AS file
";

/// Direct callees (outgoing `CALLS`) of any name in `$frontier`.
const CALLEES_CYPHER: &str = "
UNWIND $frontier AS fname
MATCH (:Entity {name: fname})-[:CALLS]->(n:Entity)
OPTIONAL MATCH (n)-[:MENTIONED_IN]->(c:Chunk {workspace: $ws})
RETURN n.name AS name, min(c.path) AS file
";

/// Types the focal `INHERITS`/`IMPORTS` (1-hop), with the edge type so the
/// projection can classify kind.
const TYPES_CYPHER: &str = "
MATCH (:Entity {name: $name})-[r:INHERITS|IMPORTS]->(t:Entity)
OPTIONAL MATCH (t)-[:MENTIONED_IN]->(c:Chunk {workspace: $ws})
RETURN t.name AS name, type(r) AS rel, min(c.path) AS file
";

/// Entities sharing a workspace chunk (file) with the focal — its file
/// siblings. Excludes the focal itself.
const SIBLINGS_CYPHER: &str = "
MATCH (:Entity {name: $name})-[:MENTIONED_IN]->(c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(s:Entity)
WHERE s.name <> $name
RETURN s.name AS name, min(c.path) AS file
";

/// The focal's representative file. Aggregates to a single row; an empty/null
/// `file` means the focal isn't mentioned in this workspace (→ `not_found`).
const FOCAL_FILE_CYPHER: &str = "
MATCH (:Entity {name: $name})-[:MENTIONED_IN]->(c:Chunk {workspace: $ws})
RETURN min(c.path) AS file
";

/// Live [`NeighborhoodSource`] over a pooled neo4rs [`Graph`]. Each query is
/// bounded by `per_query_timeout` (the Bolt connect timeout) as a backstop;
/// the authoritative OI-1 per-hop budget is enforced client-side by
/// `wylde-workspaces-client` (`200ms + 300ms × hops`) and, between hops, by
/// [`walk`]'s own deadline check.
pub struct LiveSource<'a> {
    graph: &'a Graph,
    per_query_timeout: Duration,
}

impl<'a> LiveSource<'a> {
    pub fn new(graph: &'a Graph, per_query_timeout: Duration) -> Self {
        Self {
            graph,
            per_query_timeout,
        }
    }

    /// Run a `(name, file)` read with a list `$frontier` + scalar `$ws`.
    async fn name_file_rows(
        &self,
        cypher: &str,
        frontier: &[String],
        ws: &str,
    ) -> Result<Vec<NeighborRow>, SourceError> {
        let q = cypher_query(cypher)
            .param("frontier", frontier.to_vec())
            .param("ws", ws.to_owned());
        let fut =
            async {
                let mut rows =
                    self.graph.execute(q).await.map_err(|e| {
                        SourceError::new("bolt_query", format!("neighborhood: {e}"))
                    })?;
                let mut out = Vec::new();
                while let Some(row) = rows.next().await.map_err(|e| {
                    SourceError::new("bolt_decode", format!("neighborhood decode: {e}"))
                })? {
                    let name: String = row.get("name").unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    out.push(NeighborRow {
                        name,
                        file: row.get("file").unwrap_or_default(),
                    });
                }
                Ok::<_, SourceError>(out)
            };
        self.bounded(fut, "neighbors").await
    }

    /// Apply the per-query timeout backstop to a Bolt future.
    async fn bounded<T, F>(&self, fut: F, what: &str) -> Result<T, SourceError>
    where
        F: std::future::Future<Output = Result<T, SourceError>>,
    {
        match tokio::time::timeout(self.per_query_timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(SourceError::new(
                "bolt_query",
                format!("{what} timed out after {:?}", self.per_query_timeout),
            )),
        }
    }
}

impl NeighborhoodSource for LiveSource<'_> {
    fn focal_file(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, SourceError>> + Send {
        let q = cypher_query(FOCAL_FILE_CYPHER)
            .param("name", name.to_owned())
            .param("ws", workspace.to_owned());
        async move {
            let fut = async {
                let mut rows = self
                    .graph
                    .execute(q)
                    .await
                    .map_err(|e| SourceError::new("bolt_query", format!("focal: {e}")))?;
                let file: String = match rows.next().await {
                    Ok(Some(row)) => row.get("file").unwrap_or_default(),
                    Ok(None) => String::new(),
                    Err(e) => {
                        return Err(SourceError::new(
                            "bolt_decode",
                            format!("focal decode: {e}"),
                        ))
                    }
                };
                Ok::<_, SourceError>(if file.is_empty() { None } else { Some(file) })
            };
            self.bounded(fut, "focal").await
        }
    }

    fn callers_of(
        &self,
        workspace: &str,
        frontier: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send {
        let frontier = frontier.to_vec();
        let ws = workspace.to_owned();
        async move { self.name_file_rows(CALLERS_CYPHER, &frontier, &ws).await }
    }

    fn callees_of(
        &self,
        workspace: &str,
        frontier: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send {
        let frontier = frontier.to_vec();
        let ws = workspace.to_owned();
        async move { self.name_file_rows(CALLEES_CYPHER, &frontier, &ws).await }
    }

    fn types_used_by(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<TypeRow>, SourceError>> + Send {
        let q = cypher_query(TYPES_CYPHER)
            .param("name", name.to_owned())
            .param("ws", workspace.to_owned());
        async move {
            let fut =
                async {
                    let mut rows = self
                        .graph
                        .execute(q)
                        .await
                        .map_err(|e| SourceError::new("bolt_query", format!("types: {e}")))?;
                    let mut out = Vec::new();
                    while let Some(row) = rows.next().await.map_err(|e| {
                        SourceError::new("bolt_decode", format!("types decode: {e}"))
                    })? {
                        let name: String = row.get("name").unwrap_or_default();
                        let rel_raw: String = row.get("rel").unwrap_or_default();
                        let Some(rel) = RelType::from_wire(&rel_raw) else {
                            continue;
                        };
                        if name.is_empty() {
                            continue;
                        }
                        out.push(TypeRow {
                            name,
                            file: row.get("file").unwrap_or_default(),
                            rel,
                        });
                    }
                    Ok::<_, SourceError>(out)
                };
            self.bounded(fut, "types").await
        }
    }

    fn siblings_of(
        &self,
        workspace: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send {
        let q = cypher_query(SIBLINGS_CYPHER)
            .param("name", name.to_owned())
            .param("ws", workspace.to_owned());
        async move {
            let fut =
                async {
                    let mut rows =
                        self.graph.execute(q).await.map_err(|e| {
                            SourceError::new("bolt_query", format!("siblings: {e}"))
                        })?;
                    let mut out = Vec::new();
                    while let Some(row) = rows.next().await.map_err(|e| {
                        SourceError::new("bolt_decode", format!("siblings decode: {e}"))
                    })? {
                        let name: String = row.get("name").unwrap_or_default();
                        if name.is_empty() {
                            continue;
                        }
                        out.push(NeighborRow {
                            name,
                            file: row.get("file").unwrap_or_default(),
                        });
                    }
                    Ok::<_, SourceError>(out)
                };
            self.bounded(fut, "siblings").await
        }
    }
}

/// `workspaces.symbol_context` — fetch one symbol's structural context from
/// the live graph.
///
/// `Ok(Some(ctx))` on success, `Ok(None)` when `symbol_id` isn't a symbol in
/// the workspace (→ `not_found`), `Err` only on a bad request (blank id) or a
/// graph-backend failure (the `bolt_*` wire code is preserved for the client
/// classifier, like Slice B).
pub async fn symbol_context(
    workspace_id: &str,
    symbol_id: &str,
    hops: Option<u32>,
    include_body: bool,
    include_blame: bool,
) -> Result<Option<SymbolContext>, IpcError> {
    let ws = workspace_id.trim();
    let sym = symbol_id.trim();
    if ws.is_empty() {
        return Err(IpcError::new("bad_request", "workspace_id is required"));
    }
    if sym.is_empty() {
        return Err(IpcError::new("bad_request", "symbol_id is required"));
    }
    let hops = resolve_hops(hops);

    let client = BoltClient::new();
    let timeout = client.config().connect_timeout;
    let graph = client.graph_handle().await?;
    let source = LiveSource::new(graph, timeout);

    let mut ctx = match walk(&source, ws, sym, hops, include_body, read_file_to_string)
        .await
        .map_err(|e| IpcError::new(e.code, e.message))?
    {
        Some(ctx) => ctx,
        None => return Ok(None),
    };

    // TBS Slice L: recent blame over the focal's body lines, AFTER the walk
    // so the graph path stays pure. A git subprocess is blocking IO → the
    // blocking pool; any failure (no git / no repo / untracked) is a silent
    // None — blame is enrichment, never a verb failure.
    if include_blame && ctx.symbol.line > 0 {
        let file = ctx.symbol.file.to_string_lossy().to_string();
        let start = ctx.symbol.line;
        let body_lines = ctx
            .symbol
            .body
            .as_deref()
            .map(|b| b.lines().count() as u32)
            .unwrap_or(1)
            .max(1);
        let end = start + body_lines - 1;
        ctx.symbol.blame =
            tokio::task::spawn_blocking(move || super::blame::blame_lines(&file, start, end))
                .await
                .ok()
                .flatten();
    }
    Ok(Some(ctx))
}

/// `workspaces.symbol_context` action handler. Payload:
/// `{ workspace_id, symbol_id, hops?, include_body?, include_blame? }`
/// (`hops` default 1, `include_body` default true, `include_blame` default
/// true — Slice L; blame appears only for git-tracked focal files). Reply
/// data is the [`SymbolContext`]; `not_found` when the symbol isn't in the
/// workspace.
pub async fn handle_symbol_context(payload: Value) -> Reply {
    let workspace_id = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Accept either `symbol_id` (canonical) or `symbol` (the name) as an alias.
    let symbol_id = payload
        .get("symbol_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("symbol").and_then(Value::as_str))
        .unwrap_or("");
    let hops = payload
        .get("hops")
        .and_then(Value::as_u64)
        .map(|h| h as u32);
    let include_body = payload
        .get("include_body")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let include_blame = payload
        .get("include_blame")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    match symbol_context(workspace_id, symbol_id, hops, include_body, include_blame).await {
        Ok(Some(ctx)) => match serde_json::to_value(&ctx) {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err_msg("serde", format!("serialize symbol_context: {e}")),
        },
        Ok(None) => Reply::err_msg(
            "not_found",
            format!("symbol {symbol_id:?} not found in workspace {workspace_id:?}"),
        ),
        Err(e) => Reply::err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── body extraction (pure) ──────────────────────────────────────────

    const SRC: &str = "\
use std::fmt;

fn alpha() {
    beta();
    helper();
}

fn beta() {}
";

    #[test]
    fn locate_prefers_definition_line_over_mention() {
        // `beta` is *called* on line 4 and *defined* on line 8; the def wins.
        assert_eq!(locate_symbol_line(SRC, "beta"), Some(8));
    }

    #[test]
    fn locate_falls_back_to_first_mention() {
        // `helper` is only ever called, never defined here → first mention.
        assert_eq!(locate_symbol_line(SRC, "helper"), Some(5));
    }

    #[test]
    fn locate_returns_none_for_absent_name() {
        assert_eq!(locate_symbol_line(SRC, "nonexistent"), None);
    }

    #[test]
    fn locate_is_whole_word_only() {
        let src = "fn alphabet() {}\nfn alpha() {}\n";
        // `alpha` must not match inside `alphabet`.
        assert_eq!(locate_symbol_line(src, "alpha"), Some(2));
    }

    #[test]
    fn extract_body_stops_at_blank_line() {
        // `alpha` defined on line 3, body runs to the blank line 7.
        let body = extract_body(SRC, 3).unwrap();
        assert!(body.starts_with("fn alpha() {"));
        assert!(body.contains("beta();"));
        assert!(body.contains("helper();"));
        assert!(body.ends_with('}'));
        assert!(!body.contains("fn beta"), "must stop at the blank line");
    }

    #[test]
    fn extract_body_runs_to_eof_when_no_trailing_blank() {
        let src = "fn only() {\n    work();\n}";
        assert_eq!(extract_body(src, 1).unwrap(), src);
    }

    #[test]
    fn extract_body_out_of_range_is_none() {
        assert_eq!(extract_body(SRC, 999), None);
        assert_eq!(extract_body(SRC, 0), None);
    }

    #[test]
    fn focal_body_threads_locate_and_extract() {
        let read = |_: &str| Some(SRC.to_owned());
        let (line, body) = focal_body("a.rs", "alpha", &read);
        assert_eq!(line, 3);
        assert!(body.unwrap().contains("beta();"));
    }

    #[test]
    fn focal_body_handles_unreadable_file() {
        let read = |_: &str| None;
        assert_eq!(focal_body("missing.rs", "alpha", &read), (0, None));
    }

    #[test]
    fn focal_body_empty_file_path_is_none() {
        let read = |_: &str| Some(SRC.to_owned());
        assert_eq!(focal_body("", "alpha", &read), (0, None));
    }

    // ── projection (pure, fake rows → SymbolContext facets) ─────────────

    fn nrow(name: &str, file: &str) -> NeighborRow {
        NeighborRow {
            name: name.to_owned(),
            file: file.to_owned(),
        }
    }
    fn trow(name: &str, file: &str, rel: RelType) -> TypeRow {
        TypeRow {
            name: name.to_owned(),
            file: file.to_owned(),
            rel,
        }
    }

    #[test]
    fn project_types_maps_kind_by_edge() {
        let rows = vec![
            trow("Render", "", RelType::Inherits),
            trow("std::fmt", "", RelType::Imports),
        ];
        let out = project_types(rows);
        let render = out.iter().find(|r| r.name == "Render").unwrap();
        assert_eq!(render.kind, NodeKind::Class);
        assert_eq!(render.rel_type, ContextRel::Inherits);
        assert_eq!(render.hop_distance, 1);
        let fmt = out.iter().find(|r| r.name == "std::fmt").unwrap();
        assert_eq!(fmt.kind, NodeKind::Module);
        assert_eq!(fmt.rel_type, ContextRel::Imports);
    }

    #[test]
    fn project_types_dedups_and_sorts() {
        let rows = vec![
            trow("B", "", RelType::Imports),
            trow("A", "", RelType::Imports),
            trow("A", "", RelType::Imports), // dup
        ];
        let out = project_types(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "A");
        assert_eq!(out[1].name, "B");
    }

    #[test]
    fn project_siblings_dedups_sorts_and_labels() {
        let rows = vec![
            nrow("zeta", "x.rs"),
            nrow("alpha", "x.rs"),
            nrow("zeta", "x.rs"),
        ];
        let out = project_siblings(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "alpha");
        assert!(out.iter().all(|r| r.rel_type == ContextRel::SiblingOf));
        assert!(out.iter().all(|r| r.hop_distance == 1));
    }

    #[test]
    fn focal_kind_priority_matches_slice_b() {
        assert_eq!(
            focal_kind(&[
                trow("m", "", RelType::Imports),
                trow("B", "", RelType::Inherits)
            ]),
            NodeKind::Module,
            "import endpoint wins"
        );
        assert_eq!(
            focal_kind(&[trow("B", "", RelType::Inherits)]),
            NodeKind::Class
        );
        assert_eq!(focal_kind(&[]), NodeKind::Function);
    }

    #[test]
    fn context_rel_serialises_screaming_snake() {
        assert_eq!(
            serde_json::to_value(ContextRel::SiblingOf).unwrap(),
            serde_json::Value::String("SIBLING_OF".to_owned())
        );
        assert_eq!(
            serde_json::to_value(ContextRel::Calls).unwrap(),
            serde_json::Value::String("CALLS".to_owned())
        );
    }

    #[test]
    fn resolve_hops_applies_default_and_clamp() {
        assert_eq!(resolve_hops(None), DEFAULT_HOPS);
        assert_eq!(resolve_hops(Some(0)), 1, "0 clamps up to 1");
        assert_eq!(resolve_hops(Some(3)), 3);
        assert_eq!(resolve_hops(Some(999)), MAX_HOPS, "clamps to the cap");
    }

    #[test]
    fn budget_matches_oi1_formula() {
        // 200 + 300*1 = 500ms; 200 + 300*3 = 1100ms.
        assert_eq!(budget_for(1), Duration::from_millis(500));
        assert_eq!(budget_for(3), Duration::from_millis(1100));
    }

    // ── BFS over an in-memory call graph (hop_distance correctness) ──────

    /// An in-memory [`NeighborhoodSource`]: a directed `CALLS` adjacency
    /// (caller → callees), a co-file sibling map, type edges, and the set of
    /// names that live in the workspace (have a file).
    #[derive(Default)]
    struct MockSource {
        /// caller → its direct callees.
        calls: HashMap<String, Vec<String>>,
        files: HashMap<String, String>,
        types: HashMap<String, Vec<TypeRow>>,
        siblings: HashMap<String, Vec<NeighborRow>>,
    }

    impl MockSource {
        fn file_of(&self, name: &str) -> String {
            self.files.get(name).cloned().unwrap_or_default()
        }
    }

    impl NeighborhoodSource for MockSource {
        fn focal_file(
            &self,
            _ws: &str,
            name: &str,
        ) -> impl std::future::Future<Output = Result<Option<String>, SourceError>> + Send {
            let f = self.files.get(name).cloned();
            async move { Ok(f) }
        }

        fn callers_of(
            &self,
            _ws: &str,
            frontier: &[String],
        ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send
        {
            // Reverse adjacency: who calls any frontier name.
            let mut out: Vec<NeighborRow> = Vec::new();
            for (caller, callees) in &self.calls {
                if callees.iter().any(|c| frontier.contains(c)) {
                    out.push(nrow(caller, &self.file_of(caller)));
                }
            }
            async move { Ok(out) }
        }

        fn callees_of(
            &self,
            _ws: &str,
            frontier: &[String],
        ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send
        {
            let mut out: Vec<NeighborRow> = Vec::new();
            for f in frontier {
                for callee in self.calls.get(f).into_iter().flatten() {
                    out.push(nrow(callee, &self.file_of(callee)));
                }
            }
            async move { Ok(out) }
        }

        fn types_used_by(
            &self,
            _ws: &str,
            name: &str,
        ) -> impl std::future::Future<Output = Result<Vec<TypeRow>, SourceError>> + Send {
            let t = self.types.get(name).cloned().unwrap_or_default();
            async move { Ok(t) }
        }

        fn siblings_of(
            &self,
            _ws: &str,
            name: &str,
        ) -> impl std::future::Future<Output = Result<Vec<NeighborRow>, SourceError>> + Send
        {
            let s = self.siblings.get(name).cloned().unwrap_or_default();
            async move { Ok(s) }
        }
    }

    fn graph_fixture() -> MockSource {
        // Call chain: caller2 → caller1 → focal → callee1 → callee2
        //                                  focal → callee3
        let mut calls: HashMap<String, Vec<String>> = HashMap::new();
        calls.insert("caller1".into(), vec!["focal".into()]);
        calls.insert("caller2".into(), vec!["caller1".into()]);
        calls.insert("focal".into(), vec!["callee1".into(), "callee3".into()]);
        calls.insert("callee1".into(), vec!["callee2".into()]);

        let mut files: HashMap<String, String> = HashMap::new();
        for n in [
            "focal", "caller1", "caller2", "callee1", "callee2", "callee3",
        ] {
            files.insert(n.into(), format!("C:/ws/src/{n}.rs"));
        }

        let mut types: HashMap<String, Vec<TypeRow>> = HashMap::new();
        types.insert(
            "focal".into(),
            vec![
                trow("BaseTrait", "", RelType::Inherits),
                trow("std::io", "", RelType::Imports),
            ],
        );

        let mut siblings: HashMap<String, Vec<NeighborRow>> = HashMap::new();
        siblings.insert(
            "focal".into(),
            vec![
                nrow("neighbor_a", "C:/ws/src/focal.rs"),
                nrow("neighbor_b", "C:/ws/src/focal.rs"),
            ],
        );

        MockSource {
            calls,
            files,
            types,
            siblings,
        }
    }

    #[tokio::test]
    async fn walk_one_hop_surfaces_direct_neighbours_only() {
        let src = graph_fixture();
        let ctx = walk(&src, "ws", "focal", 1, false, |_| None)
            .await
            .unwrap()
            .expect("focal resolves");

        let callees: Vec<&str> = ctx.callees.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(callees, vec!["callee1", "callee3"], "direct callees only");
        assert!(ctx.callees.iter().all(|r| r.hop_distance == 1));

        let callers: Vec<&str> = ctx.callers.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(callers, vec!["caller1"], "direct caller only");

        assert_eq!(ctx.hops_traversed, 1);
        // 1-hop facets present regardless of depth.
        assert_eq!(ctx.types_used.len(), 2);
        assert_eq!(ctx.siblings.len(), 2);
        // include_body=false ⇒ no body, line 0.
        assert!(ctx.symbol.body.is_none());
        assert_eq!(ctx.symbol.line, 0);
        // Focal kind from its type edges (import endpoint → Module).
        assert_eq!(ctx.symbol.kind, NodeKind::Module);
    }

    #[tokio::test]
    async fn walk_multi_hop_assigns_hop_distance() {
        let src = graph_fixture();
        let ctx = walk(&src, "ws", "focal", 3, false, |_| None)
            .await
            .unwrap()
            .unwrap();

        let at = |v: &[RelatedSymbol], name: &str| {
            v.iter().find(|r| r.name == name).map(|r| r.hop_distance)
        };
        // callees: callee1/callee3 @1, callee2 @2 (via callee1). Dry at 3.
        assert_eq!(at(&ctx.callees, "callee1"), Some(1));
        assert_eq!(at(&ctx.callees, "callee3"), Some(1));
        assert_eq!(at(&ctx.callees, "callee2"), Some(2));
        // callers: caller1 @1, caller2 @2 (via caller1).
        assert_eq!(at(&ctx.callers, "caller1"), Some(1));
        assert_eq!(at(&ctx.callers, "caller2"), Some(2));
        // Deepest reach is 2 even though 3 was requested (graph too small).
        assert_eq!(ctx.hops_traversed, 2);
    }

    #[tokio::test]
    async fn walk_dedups_cycles() {
        // focal → a → focal  (a 2-cycle). focal must not reappear as its own
        // callee, and `a` is visited once.
        let mut calls: HashMap<String, Vec<String>> = HashMap::new();
        calls.insert("focal".into(), vec!["a".into()]);
        calls.insert("a".into(), vec!["focal".into()]);
        let mut files = HashMap::new();
        files.insert("focal".to_owned(), "C:/ws/focal.rs".to_owned());
        files.insert("a".to_owned(), "C:/ws/a.rs".to_owned());
        let src = MockSource {
            calls,
            files,
            ..Default::default()
        };

        let ctx = walk(&src, "ws", "focal", 5, false, |_| None)
            .await
            .unwrap()
            .unwrap();
        let names: Vec<&str> = ctx.callees.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a"], "cycle back to focal is dropped");
    }

    #[tokio::test]
    async fn walk_unknown_symbol_is_none() {
        let src = graph_fixture();
        let out = walk(&src, "ws", "ghost", 1, false, |_| None).await.unwrap();
        assert!(out.is_none(), "a symbol with no workspace file ⇒ not_found");
    }

    #[tokio::test]
    async fn walk_over_50plus_entities_is_well_under_budget() {
        // A wide-then-deep synthetic call graph: focal calls 10 first-level
        // callees, each of those calls 5 second-level callees (50), and a
        // chain of 6 callers leads into focal — 60+ distinct entities, larger
        // than the §11 perf-suite floor. The walk is in-memory here (no DB),
        // so this asserts the *walk logic itself* is linear and nowhere near
        // the OI-1 budget; the live Bolt budget is the #[ignore] integration.
        let mut calls: HashMap<String, Vec<String>> = HashMap::new();
        let mut files: HashMap<String, String> = HashMap::new();
        let mut all: Vec<String> = vec!["focal".into()];

        let mut lvl1 = Vec::new();
        for i in 0..10 {
            let c = format!("callee_{i}");
            lvl1.push(c.clone());
            all.push(c.clone());
            let mut lvl2 = Vec::new();
            for j in 0..5 {
                let g = format!("callee_{i}_{j}");
                lvl2.push(g.clone());
                all.push(g);
            }
            calls.insert(c, lvl2);
        }
        calls.insert("focal".into(), lvl1);
        // Caller chain: caller_5 → caller_4 → … → caller_0 → focal.
        let mut prev = "focal".to_owned();
        for i in 0..6 {
            let c = format!("caller_{i}");
            calls.insert(c.clone(), vec![prev]);
            all.push(c.clone());
            prev = c;
        }
        for name in &all {
            files.insert(name.clone(), format!("C:/ws/src/{name}.rs"));
        }
        let src = MockSource {
            calls,
            files,
            ..Default::default()
        };
        assert!(all.len() >= 50, "fixture has {} entities", all.len());

        let started = Instant::now();
        let ctx = walk(&src, "ws", "focal", 3, false, |_| None)
            .await
            .unwrap()
            .unwrap();
        let elapsed = started.elapsed();

        // 10 direct + 50 indirect callees reachable within 3 hops.
        assert_eq!(ctx.callees.len(), 60);
        assert_eq!(
            ctx.callees.iter().filter(|r| r.hop_distance == 1).count(),
            10
        );
        assert_eq!(
            ctx.callees.iter().filter(|r| r.hop_distance == 2).count(),
            50
        );
        // 3 caller hops reachable within the budget.
        assert_eq!(ctx.callers.len(), 3);
        assert_eq!(ctx.hops_traversed, 3);
        // The pure walk is microseconds; assert it's comfortably inside the
        // 1.1s 3-hop budget with wide margin (DB latency is the real cost).
        assert!(
            elapsed < Duration::from_millis(1100),
            "walk logic took {elapsed:?}, must be well under the 3-hop budget"
        );
    }

    #[tokio::test]
    async fn walk_loads_body_when_requested() {
        let src = graph_fixture();
        let file = "fn focal() {\n    work();\n}\n";
        let ctx = walk(&src, "ws", "focal", 1, true, |_| Some(file.to_owned()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ctx.symbol.line, 1);
        assert_eq!(
            ctx.symbol.body.as_deref(),
            Some("fn focal() {\n    work();\n}")
        );
    }
}
