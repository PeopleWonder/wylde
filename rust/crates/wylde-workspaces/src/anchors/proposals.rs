//! Pending-proposal persistence (Slice N, Plan v2 §4.5/§4.6).
//!
//! [`super::reflection`] gates a candidate; this store keeps the survivors
//! until the user reviews them in the Vocabulary tab — accept lands the
//! anchor via the normal `create` path, reject records an **OI-11 30-day
//! suppression** (configurable via `WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS`)
//! so the same identifier can't resurface during the window. User-accept-
//! always (OI-18) is structural: nothing in here writes `anchors.json`.
//!
//! One `proposals.json` per workspace beside `anchors.json`, same at-rest
//! discipline (encrypt + atomic + fail-soft):
//! `{ "pending": [{anchor, confidence, rationale, proposed_at}],
//!    "rejections": { "<identifier>": <rejected_at epoch secs> } }`

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::anchor::{epoch_now, Anchor};
use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/proposals.json`.
pub fn proposals_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("proposals.json")
}

/// Default OI-11 suppression window (days). Overridable via
/// `WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS` (the Settings → Thought Bubbles
/// surface writes that knob later).
pub const REJECTION_SUPPRESS_DAYS_DEFAULT: f64 = 30.0;

fn suppress_secs() -> f64 {
    std::env::var("WYLDE_ANCHOR_REJECTION_SUPPRESS_DAYS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|d| *d >= 0.0)
        .unwrap_or(REJECTION_SUPPRESS_DAYS_DEFAULT)
        * 86_400.0
}

/// One pending proposal awaiting review.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingProposal {
    pub anchor: Anchor,
    pub confidence: f32,
    pub rationale: String,
    #[serde(default)]
    pub proposed_at: f64,
}

/// The whole `proposals.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposalsFile {
    pub pending: Vec<PendingProposal>,
    /// identifier → epoch seconds of the rejection (OI-11).
    pub rejections: HashMap<String, f64>,
}

/// Load a workspace's proposals. Fail-soft: empty on missing/torn.
pub fn load(workspace_id: &str) -> ProposalsFile {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&proposals_path(workspace_id))
    else {
        return ProposalsFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(workspace_id: &str, file: &ProposalsFile) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(file).unwrap();
    wylde_shared::encryption::write_at_rest(&proposals_path(workspace_id), body.as_bytes())
}

/// Outcome of queueing a gated candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueOutcome {
    Queued,
    /// Already pending under this identifier — not duplicated.
    AlreadyPending,
    /// Rejected within the OI-11 window — suppressed.
    Suppressed,
}

/// Queue a spam-control-approved candidate for review. Dedupes by
/// identifier; a rejection inside the suppression window suppresses it
/// (expired rejections are pruned as a side effect).
pub fn queue(
    workspace_id: &str,
    proposal: PendingProposal,
    now: f64,
) -> std::io::Result<QueueOutcome> {
    let mut file = load(workspace_id);
    // Prune expired rejections while we're here.
    let window = suppress_secs();
    file.rejections.retain(|_, at| now - *at < window);

    if let Some(at) = file.rejections.get(&proposal.anchor.identifier) {
        if now - at < window {
            return Ok(QueueOutcome::Suppressed);
        }
    }
    if file
        .pending
        .iter()
        .any(|p| p.anchor.identifier == proposal.anchor.identifier)
    {
        return Ok(QueueOutcome::AlreadyPending);
    }
    file.pending.push(proposal);
    save(workspace_id, &file)?;
    Ok(QueueOutcome::Queued)
}

/// Remove a pending proposal by identifier, returning it (the accept path
/// hands it to `store::create`; this store never writes `anchors.json`).
pub fn take(workspace_id: &str, identifier: &str) -> std::io::Result<Option<PendingProposal>> {
    let mut file = load(workspace_id);
    let idx = file
        .pending
        .iter()
        .position(|p| p.anchor.identifier == identifier);
    let Some(idx) = idx else {
        return Ok(None);
    };
    let taken = file.pending.remove(idx);
    save(workspace_id, &file)?;
    Ok(Some(taken))
}

/// Reject a pending proposal: remove it and record the OI-11 suppression.
/// `Ok(false)` when nothing was pending under that identifier.
pub fn reject(workspace_id: &str, identifier: &str, now: f64) -> std::io::Result<bool> {
    let mut file = load(workspace_id);
    let before = file.pending.len();
    file.pending.retain(|p| p.anchor.identifier != identifier);
    let removed = file.pending.len() != before;
    if removed {
        file.rejections.insert(identifier.to_owned(), now);
        save(workspace_id, &file)?;
    }
    Ok(removed)
}

/// Convenience for the verb: queue with `now = epoch_now()`.
pub fn queue_now(workspace_id: &str, proposal: PendingProposal) -> std::io::Result<QueueOutcome> {
    queue(workspace_id, proposal, epoch_now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::anchor::{workspace_anchor, AnchorKind, AnchorTarget};
    use crate::test_support::TestEnv;

    fn proposal(id: &str) -> PendingProposal {
        PendingProposal {
            anchor: workspace_anchor(
                "ws-prop",
                id,
                AnchorKind::Concept,
                AnchorTarget::Concept {
                    text: format!("def {id}"),
                },
                format!("desc {id}"),
            ),
            confidence: 0.9,
            rationale: "recurred".to_owned(),
            proposed_at: 1_000.0,
        }
    }

    #[test]
    fn queue_take_round_trip() {
        let _env = TestEnv::new();
        let ws = "ws-prop";
        assert_eq!(
            queue(ws, proposal("alpha"), 1_000.0).unwrap(),
            QueueOutcome::Queued
        );
        assert_eq!(
            queue(ws, proposal("alpha"), 1_001.0).unwrap(),
            QueueOutcome::AlreadyPending,
            "deduped by identifier"
        );
        assert_eq!(load(ws).pending.len(), 1);

        let taken = take(ws, "alpha").unwrap().expect("pending");
        assert_eq!(taken.anchor.identifier, "alpha");
        assert!(load(ws).pending.is_empty());
        assert!(take(ws, "alpha").unwrap().is_none(), "second take empty");
    }

    #[test]
    fn reject_suppresses_for_the_window_then_clears() {
        let _env = TestEnv::new();
        let ws = "ws-prop";
        let day = 86_400.0;
        queue(ws, proposal("beta"), 0.0).unwrap();
        assert!(reject(ws, "beta", 10.0).unwrap());
        assert!(!reject(ws, "beta", 11.0).unwrap(), "nothing pending now");

        // Inside the 30-day window → suppressed.
        assert_eq!(
            queue(ws, proposal("beta"), 10.0 + 29.0 * day).unwrap(),
            QueueOutcome::Suppressed
        );
        // Past the window → may resurface (OI-11), and the stale rejection
        // record is pruned.
        assert_eq!(
            queue(ws, proposal("beta"), 10.0 + 31.0 * day).unwrap(),
            QueueOutcome::Queued
        );
        assert!(load(ws).rejections.is_empty(), "expired rejection pruned");
    }

    #[test]
    fn missing_file_loads_empty() {
        let _env = TestEnv::new();
        assert_eq!(load("ws-never"), ProposalsFile::default());
    }
}
