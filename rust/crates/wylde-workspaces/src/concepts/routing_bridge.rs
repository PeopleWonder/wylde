//! The server-side seam between the workspace's concept store and the isolated
//! `wylde-concept-routing` crate (concept-routing plan §2 wiring seam).
//!
//! This is the **only impure part** of routing: it loads the encrypted concept
//! store + the anchor (vocabulary) store, then hands the pure router the
//! already-embedded query vector and the centroids. It lives here, in the
//! workspaces service, because that is where both the centroids and the RAG
//! query embed already exist — so routing reuses that one embed with **no
//! extra round-trip and no IPC hop** (plan §6.1, §8 risk 4).
//!
//! **R1 = route-and-log.** [`route_with_vec`] returns the
//! [`CandidateSet`](wylde_concept_routing::CandidateSet) and the caller logs it
//! as threshold-calibration data; nothing is injected. The
//! [`crate::config`]-free design (thresholds come from
//! [`RoutingConfig::current`]) keeps the toggle a single source of truth shared
//! with the harness.
//!
//! **Removal test:** delete this file + the `wylde-concept-routing` dep + the
//! one `route` branch in `prompt/inject.rs`, and the workspaces service is back
//! to pre-routing behaviour.

use wylde_concept_routing::{route, CandidateSet, ConceptCentroid, RoutingConfig, VocabMatch};

use super::store;
use crate::anchors;

/// Cap on vocabulary matches folded into the candidate set (keeps the log + the
/// future menu bounded; the workspace anchor set can be large).
const VOCAB_MATCH_LIMIT: usize = 12;

/// Route a turn's query into concept space for `workspace_id`, reusing the
/// `query_vec` the RAG path already embedded.
///
/// `query_text` is the composed retrieval query *as embedded* (it may carry the
/// `[active_file: …]` / `[anchors: …]` markers); those markers are stripped for
/// the human-readable echo and for vocabulary matching so they don't
/// self-match. The returned [`CandidateSet`] always reflects the real cutoff
/// decision — including "routed nothing" (the clean raw-RAG fallback signal).
///
/// Returns `None` only when the workspace has **no centroid-bearing concepts**
/// (Phase-0 directory stand-ins carry no centroid, exactly as
/// `concepts/search.rs` skips them) — there is nothing to route against, so the
/// caller logs the skip and proceeds with plain RAG.
pub fn route_with_vec(
    workspace_id: &str,
    query_vec: &[f32],
    query_text: &str,
) -> Option<CandidateSet> {
    if query_vec.is_empty() {
        return None;
    }

    // Concepts with a real centroid — the routing units. Directory stand-ins
    // (no centroid) are excluded, matching the semantic half of concept search.
    let concepts: Vec<ConceptCentroid> = store::load(workspace_id)
        .into_iter()
        .filter_map(|c| {
            c.centroid.filter(|v| !v.is_empty()).map(|centroid| ConceptCentroid {
                id: c.id,
                label: c.label,
                centroid,
            })
        })
        .collect();
    if concepts.is_empty() {
        return None;
    }

    let clean = strip_markers(query_text);
    let vocab = matched_vocabulary(workspace_id, &clean);

    Some(route(
        clean,
        query_vec,
        &concepts,
        vocab,
        &RoutingConfig::current(),
    ))
}

/// The matched-vocabulary half: load the workspace's anchor identifiers and
/// match them against the (marker-stripped) query by whole-word presence.
fn matched_vocabulary(workspace_id: &str, clean_query: &str) -> Vec<VocabMatch> {
    let terms: Vec<String> = anchors::store::load(workspace_id)
        .into_iter()
        .map(|a| a.identifier)
        .filter(|s| !s.trim().is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    wylde_concept_routing::router::match_vocabulary(clean_query, &terms, VOCAB_MATCH_LIMIT)
}

/// Strip the `[active_file: …]` / `[anchors: …]` cross-crate markers (and the
/// blank lines they sit behind) from the composed query, leaving the natural
/// user text. Best-effort line filter — a marker the harness adds is always on
/// its own line behind a `\n\n` (see `context_gather.rs`).
fn strip_markers(query_text: &str) -> String {
    query_text
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("[active_file:") || t.starts_with("[anchors:"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::{Concept, ConceptSource};
    use crate::test_support::TestEnv;

    fn concept_with_centroid(id: &str, label: &str, centroid: Vec<f32>) -> Concept {
        let mut c = Concept::new(id, label, "desc", ConceptSource::Manual);
        c.centroid = Some(centroid);
        c
    }

    #[test]
    fn strip_markers_removes_active_file_and_anchors() {
        let q = "how does auth work\n\n[active_file: src/auth.rs]\n[anchors: vpn auth]";
        assert_eq!(strip_markers(q), "how does auth work");
    }

    #[test]
    fn strip_markers_keeps_plain_query() {
        assert_eq!(strip_markers("plain question"), "plain question");
    }

    #[test]
    fn none_when_no_centroid_concepts() {
        let _env = TestEnv::new();
        let ws = "no-centroids-000000";
        // A directory stand-in (no centroid) must not make routing engage.
        store::save(
            ws,
            &[Concept::new("dir:x", "X", "d", ConceptSource::DirectoryCluster)],
        )
        .unwrap();
        assert!(route_with_vec(ws, &[1.0, 0.0], "q").is_none());
    }

    #[test]
    fn routes_against_centroids_when_present() {
        let _env = TestEnv::new();
        let ws = "with-centroids-000000";
        store::save(
            ws,
            &[
                concept_with_centroid("a", "Auth", vec![1.0, 0.0, 0.0]),
                concept_with_centroid("b", "Graph", vec![0.0, 1.0, 0.0]),
            ],
        )
        .unwrap();
        // Query co-linear with Auth's centroid.
        let set = route_with_vec(ws, &[1.0, 0.0, 0.0], "auth question").expect("routes");
        assert_eq!(set.concepts[0].id, "a");
        assert!(set.concepts[0].score > 0.99);
        assert_eq!(set.query_echo, "auth question");
    }
}
