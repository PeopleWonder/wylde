//! Concepts sub-tab → pipe calls (TBS concept-system Phase 1): the per-workspace
//! concept store on `wylde-workspaces` (`workspaces.concepts.*`).
//!
//! `ConceptView` mirrors the service's `Concept` wire shape locally — the same
//! GUI-decoupling convention as `AnchorView` ([`super::ipc`]): the GUI crate
//! doesn't link the service crate; serde defaults keep older records loading.

use serde::Deserialize;
use serde_json::{json, Value};

const SVC_WORKSPACES: &str = "wylde-workspaces";

/// The GUI mirror of one concept record (wylde-workspaces `Concept` wire shape).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ConceptView {
    pub id: String,
    pub label: String,
    pub description: String,
    pub members: Vec<String>,
    pub member_files: Vec<String>,
    pub parent_concepts: Vec<String>,
    pub described_by: Vec<String>,
    /// `directory_cluster | embedding | manual`.
    pub source: String,
}

/// A concept with its hybrid-search component scores (thesis §3.2).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScoredConceptView {
    pub concept: ConceptView,
    pub score: f32,
    pub fuzzy: f32,
    pub semantic: f32,
}

async fn workspaces_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_WORKSPACES,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

/// `workspaces.concepts.search` — hybrid (fuzzy + semantic) search. An empty
/// query returns the full set ordered by label, so this is also the load path.
pub async fn search_concepts(
    ws: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<ScoredConceptView>, String> {
    let v = workspaces_call(
        "workspaces.concepts.search",
        json!({ "workspace_id": ws, "query": query, "limit": limit }),
    )
    .await?;
    Ok(v.get("results")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| serde_json::from_value::<ScoredConceptView>(r.clone()).ok())
                .collect()
        })
        .unwrap_or_default())
}

/// `workspaces.concepts.build` — the Phase-0 cheap-concept pass (label the
/// directory clusters). Returns the number built.
pub async fn build_concepts(ws: &str) -> Result<u64, String> {
    let v = workspaces_call("workspaces.concepts.build", json!({ "workspace_id": ws })).await?;
    Ok(v.get("built").and_then(Value::as_u64).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scored_concept_parses_the_wire_shape() {
        let v = json!({
            "concept": {
                "id": "dir:src/graph", "label": "Graph",
                "description": "the graph layer",
                "members": ["alpha", "beta"], "member_files": ["src/graph/api.rs"],
                "parent_concepts": [], "source": "directory_cluster"
            },
            "score": 1.2, "fuzzy": 1.0, "semantic": 0.3
        });
        let s: ScoredConceptView = serde_json::from_value(v).unwrap();
        assert_eq!(s.concept.id, "dir:src/graph");
        assert_eq!(s.concept.label, "Graph");
        assert_eq!(s.concept.member_files, vec!["src/graph/api.rs"]);
        assert_eq!(s.concept.source, "directory_cluster");
        assert!((s.score - 1.2).abs() < 1e-6);
    }

    #[test]
    fn old_records_default_missing_fields() {
        let s: ScoredConceptView =
            serde_json::from_value(json!({ "concept": { "id": "c1", "label": "C" } })).unwrap();
        assert_eq!(s.concept.id, "c1");
        assert!(s.concept.members.is_empty());
        assert_eq!(s.score, 0.0);
    }
}
