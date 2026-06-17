//! Concept-proposal curation (TBS concept-system Phase 2, thesis §7 S2.3) —
//! the concept analogue of [`crate::anchors::proposals`].
//!
//! AI-proposed concepts (an LLM suggesting a theme the clustering missed, or a
//! name for a cluster) route through the same **propose → accept/reject** loop
//! anchors use: a candidate waits in `concept_proposals.json` until the user
//! reviews it; accept lands it in `concepts.json` via the normal store path,
//! reject records a 30-day suppression so the same id can't immediately
//! resurface. User-accept-always is structural — nothing here writes
//! `concepts.json`.
//!
//! The bulk semantic build ([`super::semantic`]) writes concepts directly (it's
//! the auto-populate path); this loop is the *individual* curation channel and
//! the "reject this garbage concept" gesture.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::concept::Concept;
use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/concept_proposals.json`.
pub fn proposals_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("concept_proposals.json")
}

/// Default suppression window (days) for a rejected concept proposal.
/// Reuses the anchor knob so the two curation loops behave identically.
pub const REJECTION_SUPPRESS_DAYS_DEFAULT: f64 = 30.0;

fn suppress_secs() -> f64 {
    std::env::var("WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|d| *d >= 0.0)
        .unwrap_or(REJECTION_SUPPRESS_DAYS_DEFAULT)
        * 86_400.0
}

/// One pending concept proposal awaiting review.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingConceptProposal {
    pub concept: Concept,
    pub confidence: f32,
    pub rationale: String,
    #[serde(default)]
    pub proposed_at: f64,
}

/// The whole `concept_proposals.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConceptProposalsFile {
    pub pending: Vec<PendingConceptProposal>,
    /// concept id → epoch seconds of the rejection.
    pub rejections: HashMap<String, f64>,
}

/// Load a workspace's concept proposals. Fail-soft: empty on missing/torn.
pub fn load(workspace_id: &str) -> ConceptProposalsFile {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&proposals_path(workspace_id))
    else {
        return ConceptProposalsFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(workspace_id: &str, file: &ConceptProposalsFile) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(file).unwrap();
    wylde_shared::encryption::write_at_rest(&proposals_path(workspace_id), body.as_bytes())
}

/// Outcome of queueing a candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueOutcome {
    Queued,
    AlreadyPending,
    /// Rejected within the suppression window.
    Suppressed,
}

/// Queue a candidate concept for review. Dedupes by id; a rejection inside the
/// window suppresses it (expired rejections are pruned as a side effect).
pub fn queue(
    workspace_id: &str,
    proposal: PendingConceptProposal,
    now: f64,
) -> std::io::Result<QueueOutcome> {
    let mut file = load(workspace_id);
    let window = suppress_secs();
    file.rejections.retain(|_, at| now - *at < window);

    if let Some(at) = file.rejections.get(&proposal.concept.id) {
        if now - at < window {
            return Ok(QueueOutcome::Suppressed);
        }
    }
    if file.pending.iter().any(|p| p.concept.id == proposal.concept.id) {
        return Ok(QueueOutcome::AlreadyPending);
    }
    file.pending.push(proposal);
    save(workspace_id, &file)?;
    Ok(QueueOutcome::Queued)
}

/// Remove + return a pending proposal by id (the accept path hands it to
/// `store::create`/`upsert`; this never writes `concepts.json`).
pub fn take(workspace_id: &str, id: &str) -> std::io::Result<Option<PendingConceptProposal>> {
    let mut file = load(workspace_id);
    let idx = file.pending.iter().position(|p| p.concept.id == id);
    let Some(idx) = idx else {
        return Ok(None);
    };
    let taken = file.pending.remove(idx);
    save(workspace_id, &file)?;
    Ok(Some(taken))
}

/// Reject a pending proposal: remove it + record the suppression. `Ok(false)`
/// when nothing was pending under that id.
pub fn reject(workspace_id: &str, id: &str, now: f64) -> std::io::Result<bool> {
    let mut file = load(workspace_id);
    let before = file.pending.len();
    file.pending.retain(|p| p.concept.id != id);
    let removed = file.pending.len() != before;
    if removed {
        file.rejections.insert(id.to_owned(), now);
        save(workspace_id, &file)?;
    }
    Ok(removed)
}

/// Is `id` currently suppressed (rejected within the window)? Used by the bulk
/// build to skip re-proposing a concept the user already rejected.
pub fn is_suppressed(workspace_id: &str, id: &str, now: f64) -> bool {
    load(workspace_id)
        .rejections
        .get(id)
        .is_some_and(|at| now - *at < suppress_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;
    use crate::test_support::TestEnv;

    fn proposal(id: &str) -> PendingConceptProposal {
        PendingConceptProposal {
            concept: Concept::new(id, id, "d", ConceptSource::Manual),
            confidence: 0.9,
            rationale: "recurs".to_owned(),
            proposed_at: 1.0,
        }
    }

    #[test]
    fn queue_take_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-cprop-0000";
        assert_eq!(queue(ws, proposal("c1"), 1.0).unwrap(), QueueOutcome::Queued);
        assert_eq!(
            queue(ws, proposal("c1"), 2.0).unwrap(),
            QueueOutcome::AlreadyPending
        );
        let taken = take(ws, "c1").unwrap().expect("pending");
        assert_eq!(taken.concept.id, "c1");
        assert!(take(ws, "c1").unwrap().is_none());
    }

    #[test]
    fn reject_suppresses_then_expires() {
        let _env = TestEnv::new();
        let ws = "ws-cprop-rej-00";
        let day = 86_400.0;
        queue(ws, proposal("c2"), 0.0).unwrap();
        assert!(reject(ws, "c2", 10.0).unwrap());
        assert!(is_suppressed(ws, "c2", 10.0 + day));
        assert_eq!(
            queue(ws, proposal("c2"), 10.0 + 29.0 * day).unwrap(),
            QueueOutcome::Suppressed
        );
        assert_eq!(
            queue(ws, proposal("c2"), 10.0 + 31.0 * day).unwrap(),
            QueueOutcome::Queued,
            "past the window it may resurface"
        );
    }

    #[test]
    fn missing_file_loads_empty() {
        let _env = TestEnv::new();
        assert_eq!(load("ws-never-cprop"), ConceptProposalsFile::default());
    }
}
