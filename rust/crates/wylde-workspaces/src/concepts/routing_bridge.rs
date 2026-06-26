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
    // `described_by` rides along for the R1.5b vocab seed-lift.
    let concepts: Vec<ConceptCentroid> = store::load(workspace_id)
        .into_iter()
        .filter_map(|c| {
            let described_by = c.described_by.clone();
            c.centroid.filter(|v| !v.is_empty()).map(|centroid| ConceptCentroid {
                id: c.id,
                label: c.label,
                centroid,
                described_by,
            })
        })
        .collect();
    if concepts.is_empty() {
        return None;
    }

    let clean = strip_markers(query_text);
    let vocab = matched_vocabulary(workspace_id, &clean);

    // R1.5a/b — load the typed relation graph and let the spread engine reshape
    // the flat seed. An empty graph (the default) is the engine's identity, so
    // this stays byte-equivalent to R1 until the user authors relations.
    let graph = super::relations_bridge::load(workspace_id);

    // H6 — the hierarchy's containment adjacency, a SEPARATE decayed propagation
    // channel (definitional-hierarchy OQ-6). Toggle-gated at its source: when the
    // master hierarchy toggle is OFF (the default) this is an empty `Vec` built
    // without touching the hierarchy stores, so the spread step is byte-identical
    // to today; ON-but-no-edges is likewise empty ⇒ identity.
    let containment = super::hierarchy_bridge::containment_adjacency(workspace_id);

    Some(route(
        clean,
        query_vec,
        &concepts,
        vocab,
        &containment,
        &graph,
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

    /// A unit centroid whose cosine against the query `[1,0,0]` is exactly `c0`.
    fn centroid_for_cosine(c0: f32) -> Vec<f32> {
        vec![c0, (1.0 - c0 * c0).max(0.0).sqrt(), 0.0]
    }

    /// **R1.5b end-to-end proof** (the addendum's canonical claim, exercised
    /// through the REAL verb handler + REAL routing path — deterministic, no
    /// live service). Nextcloud sits flat next to Wylde (the conflation the raw
    /// cosine can't separate) and depends-on DDNS (whose own cosine is flat).
    /// Author "Nextcloud IS-NOT Wylde" + "Nextcloud depends-on DDNS" via the
    /// `relations.add` verb, route, and prove: the exclusion SUPPRESSES Wylde
    /// and the dependency PULLS IN DDNS — flipping their rank versus the
    /// seed-only baseline.
    #[tokio::test]
    async fn relations_suppress_exclusions_and_pull_dependencies() {
        let _env = TestEnv::new();
        let ws = "rel-proof-00000";

        // Seed the live-shaped concept set with realistic flat cosines:
        // Nextcloud 0.64, Wylde 0.62 (near-tie — the conflation), DDNS 0.30.
        super::super::store::save(
            ws,
            &[
                concept_with_centroid("nextcloud", "Nextcloud", centroid_for_cosine(0.64)),
                concept_with_centroid("wylde", "Wylde", centroid_for_cosine(0.62)),
                concept_with_centroid("ddns", "DDNS", centroid_for_cosine(0.30)),
            ],
        )
        .unwrap();

        // Author the two edges through the real verb handler.
        let add = |from: &'static str, to: &'static str, kind: &'static str| {
            super::super::relations_bridge::handle_add(serde_json::json!({
                "workspace_id": ws,
                "from": {"node":"concept","id":from},
                "to": {"node":"concept","id":to},
                "kind": kind,
            }))
        };
        assert!(add("nextcloud", "wylde", "negative").await.ok);
        assert!(add("nextcloud", "ddns", "dependency").await.ok);

        // Route the query (co-linear with Nextcloud) through the real path.
        let q = vec![1.0, 0.0, 0.0];
        let set = route_with_vec(ws, &q, "how do I set up nextcloud").expect("routes");

        let by = |id: &str| set.concepts.iter().find(|c| c.id == id).unwrap().clone();
        let wylde = by("wylde");
        let ddns = by("ddns");

        // BEFORE (seed cosine): the flat distribution R1 logged.
        assert!(
            wylde.seed_score > ddns.seed_score,
            "seed: Wylde ≫ DDNS (flat cosine can't tell)"
        );

        // AFTER (settled): exclusion pushed Wylde DOWN, dependency pulled DDNS UP…
        assert!(
            wylde.score < wylde.seed_score,
            "exclusion suppressed Wylde below its cosine"
        );
        assert!(
            ddns.score > ddns.seed_score,
            "dependency pulled DDNS above its cosine"
        );
        // …and the RANK FLIPPED — DDNS now outranks the excluded Wylde.
        assert!(ddns.score > wylde.score, "the gap the raw cosine couldn't make");

        // Provenance proves WHY (the explainable payload).
        assert!(matches!(
            wylde.provenance,
            wylde_concept_routing::Provenance::Inhibited { .. }
        ));
        assert!(matches!(
            ddns.provenance,
            wylde_concept_routing::Provenance::Dependency { .. }
        ));
        assert!(
            set.reshaped_by_relations(),
            "the relation graph reshaped the activation"
        );

        // And it's all visible in the before→after proof log.
        let line = set.relation_log_line();
        assert!(line.contains("⊘Wylde"), "log marks Wylde inhibited: {line}");
        assert!(line.contains("↳DDNS"), "log marks DDNS dependency-pulled: {line}");
    }

    /// **H6 end-to-end wiring proof** (through the REAL `route_with_vec` path):
    /// a child concept whose own cosine is flat is lifted by its parent's
    /// activation along the hierarchy containment edge — but ONLY when the master
    /// hierarchy toggle is ON. Toggle OFF ⇒ byte-identical to today (the child's
    /// settled score equals its seed cosine, provenance `Seed`).
    #[tokio::test]
    async fn containment_lifts_a_flat_child_only_when_toggle_on() {
        use wylde_concept_hierarchy::HierarchyConfig;
        let _env = TestEnv::new();
        let ws = "cont-route-0000";
        // Auth fires (≈0.9); Token's own cosine is flat (≈0.05). Token is a child
        // of Auth (parent_concepts), so the projection draws an Auth⊃Token
        // containment edge.
        let mut token = concept_with_centroid("token", "Token", centroid_for_cosine(0.05));
        token.parent_concepts = vec!["auth".into()];
        super::store::save(
            ws,
            &[
                concept_with_centroid("auth", "Auth", centroid_for_cosine(0.9)),
                token,
            ],
        )
        .unwrap();
        let q = vec![1.0, 0.0, 0.0];

        // Toggle OFF (default): the containment channel is empty ⇒ Token keeps its
        // flat seed, provenance Seed (identity-when-off, proven end-to-end).
        let _ = HierarchyConfig::persist(HierarchyConfig { enabled: false });
        let off = route_with_vec(ws, &q, "auth").expect("routes");
        let t_off = off.concepts.iter().find(|c| c.id == "token").unwrap();
        assert!(
            (t_off.score - t_off.seed_score).abs() < 1e-6,
            "OFF ⇒ Token unreshaped ({} vs {})",
            t_off.score,
            t_off.seed_score
        );
        assert!(matches!(
            t_off.provenance,
            wylde_concept_routing::Provenance::Seed
        ));
        assert!(!off.reshaped_by_relations(), "OFF ⇒ routing identical to today");

        // Toggle ON: Auth (≈0.9) flows DOWN the containment edge (weak) to Token,
        // lifting its settled score above its flat cosine, with Containment prov.
        HierarchyConfig::persist(HierarchyConfig { enabled: true }).unwrap();
        let on = route_with_vec(ws, &q, "auth").expect("routes");
        let t_on = on.concepts.iter().find(|c| c.id == "token").unwrap();
        assert!(
            t_on.score > t_on.seed_score + 1e-6,
            "ON ⇒ containment lifts the flat child ({} > {})",
            t_on.score,
            t_on.seed_score
        );
        assert!(matches!(
            t_on.provenance,
            wylde_concept_routing::Provenance::Containment { .. }
        ));
        // Restore the OFF default for any later test in the binary.
        let _ = HierarchyConfig::persist(HierarchyConfig { enabled: false });
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
