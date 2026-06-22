//! Curate-before-inject menu → harness pipe calls (concept-routing **R2**,
//! plan §4). Phase 1 of the two-phase turn: `chat.preview_context` on the
//! `wylde-harness` service routes the turn's query and returns the candidate
//! set the menu is built from; the user's curated ids ride back on
//! `chat.run_turn` (the Chat panel owns that send, carrying `curated_concepts`).
//!
//! The `*View` types mirror the `wylde-concept-routing` crate's
//! [`CandidateSet`] wire shape locally — the same GUI-decoupling convention as
//! [`super::ipc`] (the GUI crate doesn't link the service crate; serde defaults
//! keep older records loading). Behaviour-safe: this verb only *reads* — it
//! routes, never injects.

use serde::Deserialize;
use serde_json::{json, Value};

use super::ipc::NodeRefView;

const SVC_HARNESS: &str = "wylde-harness";

/// GUI mirror of `wylde_concept_routing::Provenance` (why a concept activated) —
/// the internally-tagged wire shape (`{"kind":"dependency","from":{…},"hops":1}`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProvenanceView {
    #[default]
    Seed,
    SeedLift {
        from: NodeRefView,
    },
    Dependency {
        from: NodeRefView,
        hops: u8,
    },
    Positive {
        from: NodeRefView,
    },
    Inhibited {
        by: NodeRefView,
        raw: f32,
    },
}

impl ProvenanceView {
    /// The other-endpoint node this concept activated *via* (a dependency
    /// source, a vocab term, an excluder), if any — for the "↳ via X" label.
    pub fn via(&self) -> Option<&NodeRefView> {
        match self {
            ProvenanceView::Seed => None,
            ProvenanceView::SeedLift { from }
            | ProvenanceView::Dependency { from, .. }
            | ProvenanceView::Positive { from } => Some(from),
            ProvenanceView::Inhibited { by, .. } => Some(by),
        }
    }
}

/// GUI mirror of one routed concept.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RoutedConceptView {
    pub id: String,
    pub label: String,
    pub score: f32,
    #[serde(default)]
    pub seed_score: f32,
    #[serde(default)]
    pub provenance: ProvenanceView,
    pub activated: bool,
}

/// GUI mirror of one matched vocabulary term.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct VocabMatchView {
    pub identifier: String,
    pub score: f32,
}

/// GUI mirror of the routed [`CandidateSet`] — the menu's data source.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct CandidateSetView {
    #[serde(default)]
    pub query_echo: String,
    #[serde(default)]
    pub concepts: Vec<RoutedConceptView>,
    #[serde(default)]
    pub vocabulary: Vec<VocabMatchView>,
    #[serde(default)]
    pub abs_threshold: f32,
    #[serde(default)]
    pub chosen_cutoff: f32,
    #[serde(default)]
    pub activated_count: usize,
    #[serde(default)]
    pub max_concepts: usize,
}

/// The `chat.preview_context` reply.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PreviewReply {
    /// Master toggle state — `false` ⇒ no menu, the turn runs as today.
    #[serde(default)]
    pub routing_enabled: bool,
    /// `curate_before_inject` — `false` ⇒ auto-apply the default set (no
    /// blocking menu), but the first turn still surfaces it (cadence).
    #[serde(default)]
    pub curate: bool,
    /// The routed candidate set — `None` when nothing routed (raw-RAG fallback)
    /// or the workspace was unreachable.
    #[serde(default)]
    pub candidates: Option<CandidateSetView>,
    /// The injected-concept token budget the menu's indicator measures against.
    #[serde(default)]
    pub inject_token_budget: usize,
}

async fn harness_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

/// `chat.preview_context` — Phase 1: route the turn's query and return the
/// curate-before-inject candidate menu (no injection, no LLM). A transport
/// failure surfaces as `Err` so the caller can fall back to a plain turn.
pub async fn preview_context(
    workspace_id: &str,
    conversation_id: &str,
    user_message: &str,
    active_file: Option<&str>,
) -> Result<PreviewReply, String> {
    let mut payload = json!({
        "workspace_id": workspace_id,
        "conversation_id": conversation_id,
        "user_message": user_message,
    });
    if let Some(f) = active_file.map(str::trim).filter(|s| !s.is_empty()) {
        payload["active_file"] = json!(f);
    }
    let v = harness_call("chat.preview_context", payload).await?;
    serde_json::from_value(v).map_err(|e| format!("preview_context: bad reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_set_parses_full_wire_shape() {
        let v = json!({
            "query_echo": "how does auth work",
            "concepts": [
                { "id": "auth", "label": "Auth", "score": 0.71, "seed_score": 0.71,
                  "provenance": { "kind": "seed" }, "activated": true },
                { "id": "ddns", "label": "DDNS", "score": 0.32, "seed_score": 0.30,
                  "provenance": { "kind": "dependency", "from": {"node":"concept","id":"auth"}, "hops": 1 },
                  "activated": true },
                { "id": "wylde", "label": "Wylde", "score": 0.30, "seed_score": 0.62,
                  "provenance": { "kind": "inhibited", "by": {"node":"concept","id":"auth"}, "raw": 0.62 },
                  "activated": false }
            ],
            "vocabulary": [ { "identifier": "the_pipe", "score": 1.0 } ],
            "abs_threshold": 0.5, "chosen_cutoff": 0.5, "activated_count": 2, "max_concepts": 3
        });
        let set: CandidateSetView = serde_json::from_value(v).unwrap();
        assert_eq!(set.concepts.len(), 3);
        assert_eq!(set.activated_count, 2);
        // Provenance round-trips and exposes the via-node.
        let dep = &set.concepts[1];
        assert!(matches!(
            dep.provenance,
            ProvenanceView::Dependency { hops: 1, .. }
        ));
        assert_eq!(dep.provenance.via(), Some(&NodeRefView::concept("auth")));
        let excl = &set.concepts[2];
        assert!(matches!(excl.provenance, ProvenanceView::Inhibited { .. }));
        assert_eq!(set.vocabulary[0].identifier, "the_pipe");
    }

    #[test]
    fn preview_reply_parses_and_defaults() {
        // Toggle off → no candidates.
        let off: PreviewReply = serde_json::from_value(json!({
            "routing_enabled": false, "curate": false, "candidates": null,
            "inject_token_budget": 1500
        }))
        .unwrap();
        assert!(!off.routing_enabled);
        assert!(off.candidates.is_none());
        assert_eq!(off.inject_token_budget, 1500);

        // Missing fields default safely.
        let bare: PreviewReply = serde_json::from_value(json!({})).unwrap();
        assert!(!bare.routing_enabled && !bare.curate && bare.candidates.is_none());
    }

    #[test]
    fn provenance_seed_has_no_via() {
        assert_eq!(ProvenanceView::Seed.via(), None);
    }
}
