//! The one check a port-liveness ping structurally cannot do: prove the graph
//! DB booted against **real data**, not an empty folder.
//!
//! Alpha shipped with Neo4j silently reading an empty database from a directory
//! literally named `${WYLDE_MEMGRAPH_DATA}` (the conf never expanded the var).
//! `bolt://…:7687` was listening the whole time, so every "is the port up?"
//! check was green while the product read nothing (roadmap headline bug #2,
//! fixed in c28e991). This queries node counts over Bolt and fails closed
//! unless the graph actually contains content — the same driver (`neo4rs`) and
//! env knobs (`GRAPH_BOLT_URL` / `GRAPH_USER` / `GRAPH_PASSWORD`) the product's
//! `BoltClient` uses, so the gate sees exactly what the app would.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use neo4rs::{ConfigBuilder, Graph};

/// Default Bolt URL — matches `wylde_harness::…::bolt::DEFAULT_BOLT_URL`.
const DEFAULT_BOLT_URL: &str = "bolt://127.0.0.1:7687";

/// The node counts that decide "populated". `Chunk` is the strongest single
/// signal (indexed text/source fragments); `Entity` (code symbols) is the
/// second. Either being non-zero proves the DB read real data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphCounts {
    pub chunks: i64,
    pub entities: i64,
}

impl GraphCounts {
    /// The DB is populated if it holds any chunks or entities. A totally empty
    /// graph — the exact `${VAR}` empty-boot symptom — has both at zero.
    pub fn is_populated(&self) -> bool {
        self.chunks > 0 || self.entities > 0
    }
}

/// Resolve the Bolt URL the same way the product client does: `GRAPH_BOLT_URL`
/// wins; else `bolt://127.0.0.1:<GRAPH_BOLT_PORT|7687>`.
fn bolt_url() -> String {
    if let Ok(url) = std::env::var("GRAPH_BOLT_URL") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    if let Ok(port) = std::env::var("GRAPH_BOLT_PORT") {
        if let Ok(port) = port.trim().parse::<u16>() {
            return format!("bolt://127.0.0.1:{port}");
        }
    }
    DEFAULT_BOLT_URL.to_string()
}

/// Connect to Bolt and read the `Chunk` / `Entity` node counts, bounded by
/// `timeout`. Any connect/query failure (or a hang) is an `Err` — the caller
/// treats that as a FAIL, never a pass.
pub fn graph_counts(timeout: Duration) -> Result<GraphCounts> {
    // neo4rs is tokio-async; drive it on a private current-thread runtime and
    // bound the whole thing with a timeout so an unreachable/hung DB fails
    // closed instead of wedging the preflight.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for the Bolt query")?;

    rt.block_on(async {
        match tokio::time::timeout(timeout, query_counts()).await {
            Ok(res) => res,
            Err(_) => Err(anyhow!(
                "Bolt query timed out after {}s (DB unreachable or hung)",
                timeout.as_secs_f64()
            )),
        }
    })
}

async fn query_counts() -> Result<GraphCounts> {
    let uri = bolt_url();
    let user = std::env::var("GRAPH_USER").unwrap_or_default();
    let password = std::env::var("GRAPH_PASSWORD").unwrap_or_default();

    let cfg = ConfigBuilder::default()
        .uri(uri.clone())
        .user(user)
        .password(password)
        .build()
        .with_context(|| format!("building neo4rs config for {uri}"))?;
    let graph = Graph::connect(cfg)
        .await
        .with_context(|| format!("connecting to Bolt at {uri}"))?;

    let chunks = count_label(&graph, "Chunk")
        .await
        .context("counting (:Chunk) nodes")?;
    let entities = count_label(&graph, "Entity")
        .await
        .context("counting (:Entity) nodes")?;
    Ok(GraphCounts { chunks, entities })
}

/// `MATCH (:Label) RETURN count(*) AS n` for a single label. The label is a
/// compile-time literal here (never user input), so interpolating it into the
/// Cypher is safe.
async fn count_label(graph: &Graph, label: &str) -> Result<i64> {
    let cypher = format!("MATCH (:{label}) RETURN count(*) AS n");
    let mut rows = graph
        .execute(neo4rs::query(&cypher))
        .await
        .with_context(|| format!("executing `{cypher}`"))?;
    match rows.next().await.context("reading count row")? {
        Some(row) => row.get::<i64>("n").context("decoding count as i64"),
        None => bail!("count query returned no rows"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn populated_requires_a_nonzero_count() {
        assert!(GraphCounts { chunks: 5, entities: 0 }.is_populated());
        assert!(GraphCounts { chunks: 0, entities: 3 }.is_populated());
        // The empty-graph symptom: both zero → not populated.
        assert!(!GraphCounts { chunks: 0, entities: 0 }.is_populated());
    }

    #[test]
    fn bolt_url_prefers_explicit_env() {
        // Deterministic: exercise only the default path (no env set in the
        // test shell). The env-override branches are trivial string formatting.
        std::env::remove_var("GRAPH_BOLT_URL");
        std::env::remove_var("GRAPH_BOLT_PORT");
        assert_eq!(bolt_url(), DEFAULT_BOLT_URL);
    }
}
