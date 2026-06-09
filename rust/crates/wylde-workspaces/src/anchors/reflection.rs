//! LLM-proposed anchors — the reflection flow (Plan v2 §4.5, OI-7, OI-18).
//!
//! The model can *propose* anchors it notices recurring in conversation, but
//! it never persists one: every proposal is **user-accept-always** (OI-18).
//! This module is the data primitive — it produces a candidate (not saved) and
//! enforces the **spam-control** limits so reflection can't flood the user:
//!
//!   * **≤ 10 proposals per conversation** (any kind).
//!   * **1-hour cooldown** between proposals to the same user.
//!   * **confidence ≥ 0.7** — lower-confidence guesses are dropped.
//!
//! All three are Plan v2 §4.5 defaults (tunable in Settings → Thought Bubbles).
//! The verb passes the conversation's running counters in; this slice keeps no
//! per-conversation proposal state of its own (there's no UI consumer yet — the
//! caller owns the counters).

use super::anchor::Anchor;

/// Max proposals (anchors + notes + profile updates) per conversation (OI-7).
pub const MAX_PROPOSALS_PER_CONVERSATION: u32 = 10;
/// Minimum seconds between proposals to the same user (OI-7).
pub const COOLDOWN_SECS: f64 = 3600.0;
/// Minimum model confidence to surface a proposal (OI-7).
pub const MIN_CONFIDENCE: f32 = 0.7;

/// Why a proposal was suppressed by spam-control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The per-conversation proposal budget is spent.
    BudgetExhausted,
    /// Still inside the cooldown window since the last proposal.
    Cooldown,
    /// Model confidence below [`MIN_CONFIDENCE`].
    LowConfidence,
    /// The proposed identifier isn't a valid `{{token}}`.
    InvalidIdentifier,
}

impl RejectReason {
    /// A stable wire string the verb returns in `{candidate: null, reason}`.
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::BudgetExhausted => "budget_exhausted",
            RejectReason::Cooldown => "cooldown",
            RejectReason::LowConfidence => "low_confidence",
            RejectReason::InvalidIdentifier => "invalid_identifier",
        }
    }
}

/// The running spam-control counters for one conversation, supplied by the
/// caller (the chat turn driver owns this state).
#[derive(Clone, Copy, Debug, Default)]
pub struct ReflectionBudget {
    /// Proposals already surfaced in this conversation.
    pub proposals_so_far: u32,
    /// Epoch seconds of the last proposal to this user, if any.
    pub last_proposal_at: Option<f64>,
}

/// A candidate anchor the model proposes — **not** persisted. The user accepts
/// it via `anchors.create`; until then it only exists in this struct.
#[derive(Clone, Debug, PartialEq)]
pub struct AnchorProposal {
    pub anchor: Anchor,
    pub confidence: f32,
    /// Why the model thinks this is worth anchoring (shown in the accept UI).
    pub rationale: String,
}

/// Decide whether a proposal at `confidence` may be surfaced now, given the
/// conversation `budget` and the current time `now` (epoch seconds). Order of
/// checks is stable so the returned reason is deterministic.
pub fn allow_proposal(
    budget: ReflectionBudget,
    confidence: f32,
    now: f64,
) -> Result<(), RejectReason> {
    if confidence < MIN_CONFIDENCE {
        return Err(RejectReason::LowConfidence);
    }
    if budget.proposals_so_far >= MAX_PROPOSALS_PER_CONVERSATION {
        return Err(RejectReason::BudgetExhausted);
    }
    if let Some(last) = budget.last_proposal_at {
        if now - last < COOLDOWN_SECS {
            return Err(RejectReason::Cooldown);
        }
    }
    Ok(())
}

/// Build a candidate proposal if spam-control allows it. The `anchor`'s
/// identifier must be a valid token. Returns the candidate (not saved) or the
/// reason it was suppressed.
pub fn propose(
    anchor: Anchor,
    confidence: f32,
    rationale: impl Into<String>,
    budget: ReflectionBudget,
    now: f64,
) -> Result<AnchorProposal, RejectReason> {
    if !anchor.has_valid_identifier() {
        return Err(RejectReason::InvalidIdentifier);
    }
    allow_proposal(budget, confidence, now)?;
    Ok(AnchorProposal {
        anchor,
        confidence,
        rationale: rationale.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::anchor::{workspace_anchor, AnchorKind, AnchorTarget};

    fn anchor(id: &str) -> Anchor {
        workspace_anchor(
            "ws",
            id,
            AnchorKind::Concept,
            AnchorTarget::Concept { text: "t".into() },
            "d",
        )
    }

    #[test]
    fn confidence_below_threshold_is_rejected() {
        let b = ReflectionBudget::default();
        assert_eq!(
            allow_proposal(b, 0.69, 1000.0),
            Err(RejectReason::LowConfidence)
        );
        assert!(allow_proposal(b, 0.7, 1000.0).is_ok(), "exactly 0.7 passes");
    }

    #[test]
    fn budget_exhausted_after_ten() {
        let b = ReflectionBudget {
            proposals_so_far: MAX_PROPOSALS_PER_CONVERSATION,
            last_proposal_at: None,
        };
        assert_eq!(
            allow_proposal(b, 0.9, 1_000_000.0),
            Err(RejectReason::BudgetExhausted)
        );
        let ok = ReflectionBudget {
            proposals_so_far: 9,
            last_proposal_at: None,
        };
        assert!(allow_proposal(ok, 0.9, 1_000_000.0).is_ok(), "10th allowed");
    }

    #[test]
    fn cooldown_window_blocks_then_clears() {
        let last = 1_000_000.0;
        let b = ReflectionBudget {
            proposals_so_far: 1,
            last_proposal_at: Some(last),
        };
        // 59 min later — still cooling down.
        assert_eq!(
            allow_proposal(b, 0.9, last + 3540.0),
            Err(RejectReason::Cooldown)
        );
        // 1h later — clear.
        assert!(allow_proposal(b, 0.9, last + COOLDOWN_SECS).is_ok());
    }

    #[test]
    fn propose_returns_candidate_when_allowed() {
        let p = propose(
            anchor("recurring_idea"),
            0.85,
            "came up 3 times",
            ReflectionBudget::default(),
            0.0,
        )
        .expect("allowed");
        assert_eq!(p.anchor.identifier, "recurring_idea");
        assert_eq!(p.confidence, 0.85);
        assert_eq!(p.rationale, "came up 3 times");
    }

    #[test]
    fn propose_rejects_invalid_identifier_first() {
        let mut a = anchor("ok");
        a.identifier = "bad name".into();
        assert_eq!(
            propose(a, 0.99, "r", ReflectionBudget::default(), 0.0),
            Err(RejectReason::InvalidIdentifier)
        );
    }

    #[test]
    fn reject_reason_wire_strings() {
        assert_eq!(RejectReason::Cooldown.as_str(), "cooldown");
        assert_eq!(RejectReason::LowConfidence.as_str(), "low_confidence");
        assert_eq!(RejectReason::BudgetExhausted.as_str(), "budget_exhausted");
        assert_eq!(
            RejectReason::InvalidIdentifier.as_str(),
            "invalid_identifier"
        );
    }
}
