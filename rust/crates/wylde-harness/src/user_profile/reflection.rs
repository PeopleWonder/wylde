//! Reflection cycle — LLM-proposed profile updates, spam-controlled.
//!
//! Reflection is how the assistant *suggests* it has learned something
//! durable about the user ("you keep asking me to be terse — pin that?").
//! It never edits the profile; it mints a [`ProfileProposal`] into the
//! pending queue for the user to accept / edit / reject (Plan v2 §4.6 /
//! OI-18 — user-edit-wins-always).
//!
//! ## Spam control (OI-7) — the loaded defaults
//!
//! Reflection is deliberately rate-limited so the user isn't nagged:
//! * **≤ 10 proposals per conversation** ([`MAX_PROPOSALS_PER_CONVERSATION`]).
//! * **1 h cooldown between proposals to the same field**
//!   ([`FIELD_COOLDOWN_SECS`]).
//! * **confidence ≥ 0.7** ([`CONFIDENCE_THRESHOLD`]).
//!
//! ## Rejection suppression (OI-11)
//!
//! A rejected suggestion is suppressed for **30 days**
//! ([`REJECTION_SUPPRESSION_SECS`]) — re-proposing the same
//! `(field, proposed)` inside that window is refused. No fine-tuning
//! loop in v1; this is the whole feedback mechanism.
//!
//! [`admit`] is the pure gate that enforces all four rules against the
//! current store; [`propose`] runs the gate and persists on success.
//!
//! ## The reflection *trigger* is a scaffold
//!
//! [`reflect_after_turn`] is wired into the chat turn driver post-turn
//! (Build Order §6 Slice D) but its candidate *extraction*
//! ([`scan_user_message`]) is intentionally minimal — a single
//! high-precision heuristic ("call me X" → a name proposal). Real
//! LLM-driven extraction (a dedicated reflection turn synthesising
//! candidates from the whole exchange) is a polish item; the plumbing,
//! the gate, and the persistence are what this slice locks down.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::user_profile::profile::{ProfileProposal, RejectedRecord};
use crate::user_profile::store::{self, ProfileStore};

/// OI-7 — at most this many proposals per conversation.
pub const MAX_PROPOSALS_PER_CONVERSATION: usize = 10;
/// OI-7 — minimum seconds between proposals to the *same* field.
pub const FIELD_COOLDOWN_SECS: i64 = 3_600;
/// OI-7 — minimum model confidence to surface a proposal.
pub const CONFIDENCE_THRESHOLD: f64 = 0.7;
/// OI-11 — a rejected `(field, proposed)` is suppressed this long.
pub const REJECTION_SUPPRESSION_SECS: i64 = 30 * 86_400;

/// A would-be proposal before it's admitted + minted into the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposalCandidate {
    pub field: String,
    pub proposed: String,
    pub current: Option<String>,
    pub rationale: String,
    pub confidence: f64,
    pub conversation_id: Option<String>,
}

/// Why a candidate was refused by [`admit`]. Each maps to a stable
/// `code` the `user_profile.propose` reply surfaces so callers (and
/// tests) can branch on the reason without string-matching prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitError {
    /// Below [`CONFIDENCE_THRESHOLD`].
    LowConfidence,
    /// This conversation already hit [`MAX_PROPOSALS_PER_CONVERSATION`].
    ConversationQuotaFull,
    /// A proposal to this field landed within [`FIELD_COOLDOWN_SECS`].
    FieldCooldown,
    /// This exact `(field, proposed)` was rejected within
    /// [`REJECTION_SUPPRESSION_SECS`] (OI-11).
    Suppressed,
    /// Refused as a duplicate of an already-pending proposal.
    DuplicatePending,
}

impl AdmitError {
    /// Stable machine-readable code for the IPC reply.
    pub fn code(self) -> &'static str {
        match self {
            AdmitError::LowConfidence => "low_confidence",
            AdmitError::ConversationQuotaFull => "conversation_quota_full",
            AdmitError::FieldCooldown => "field_cooldown",
            AdmitError::Suppressed => "suppressed",
            AdmitError::DuplicatePending => "duplicate_pending",
        }
    }

    /// Human-readable explanation for the reply / logs.
    pub fn message(self) -> &'static str {
        match self {
            AdmitError::LowConfidence => "proposal confidence is below the 0.7 threshold",
            AdmitError::ConversationQuotaFull => {
                "this conversation already has the maximum 10 pending proposals"
            }
            AdmitError::FieldCooldown => "a proposal to this field was made within the last hour",
            AdmitError::Suppressed => "an identical proposal was rejected within the last 30 days",
            AdmitError::DuplicatePending => "an identical proposal is already pending",
        }
    }
}

/// Pure spam-control gate (OI-7 + OI-11). Checks `cand` against the
/// current `store` at time `now` (unix seconds). Order is cheapest-first
/// and each rule is independently unit-tested.
pub fn admit(store: &ProfileStore, cand: &ProposalCandidate, now: i64) -> Result<(), AdmitError> {
    // 1. Confidence floor.
    if cand.confidence < CONFIDENCE_THRESHOLD {
        return Err(AdmitError::LowConfidence);
    }

    // 2. Rejection suppression (OI-11) — same field + same value.
    if store.rejected.iter().any(|r| {
        r.field == cand.field
            && r.proposed == cand.proposed
            && now.saturating_sub(r.rejected_at) < REJECTION_SUPPRESSION_SECS
    }) {
        return Err(AdmitError::Suppressed);
    }

    // 3. Exact-duplicate of something already pending → refuse quietly.
    if store
        .pending
        .iter()
        .any(|p| p.field == cand.field && p.proposed == cand.proposed)
    {
        return Err(AdmitError::DuplicatePending);
    }

    // 4. Per-field cooldown (OI-7) — counts both a still-pending proposal
    //    to this field and a recent rejection of it, so neither vector
    //    can re-trigger inside the hour.
    let pending_recent = store
        .pending
        .iter()
        .any(|p| p.field == cand.field && now.saturating_sub(p.created_at) < FIELD_COOLDOWN_SECS);
    let rejected_recent = store
        .rejected
        .iter()
        .any(|r| r.field == cand.field && now.saturating_sub(r.rejected_at) < FIELD_COOLDOWN_SECS);
    if pending_recent || rejected_recent {
        return Err(AdmitError::FieldCooldown);
    }

    // 5. Per-conversation quota (OI-7).
    if let Some(cid) = cand.conversation_id.as_deref() {
        let count = store
            .pending
            .iter()
            .filter(|p| p.conversation_id.as_deref() == Some(cid))
            .count();
        if count >= MAX_PROPOSALS_PER_CONVERSATION {
            return Err(AdmitError::ConversationQuotaFull);
        }
    }

    Ok(())
}

/// Mint a [`ProfileProposal`] from an admitted candidate.
pub fn mint(cand: ProposalCandidate, now: i64) -> ProfileProposal {
    ProfileProposal {
        id: uuid::Uuid::new_v4().simple().to_string(),
        field: cand.field,
        proposed: cand.proposed,
        current: cand.current,
        rationale: cand.rationale,
        confidence: cand.confidence,
        conversation_id: cand.conversation_id,
        created_at: now,
    }
}

/// Run the gate against the live store and, on success, persist the
/// minted proposal into the pending queue. Returns the new proposal on
/// admission, or the [`AdmitError`] explaining the refusal.
///
/// The whole load→gate→push→save runs under the store lock
/// ([`store::with_store`]) so two concurrent proposals can't both slip
/// past the quota.
pub fn propose(cand: ProposalCandidate) -> Result<ProfileProposal, AdmitError> {
    let now = now_secs();
    store::with_store(|s| match admit(s, &cand, now) {
        Ok(()) => {
            let p = mint(cand.clone(), now);
            s.pending.push(p.clone());
            Ok(p)
        }
        Err(e) => Err(e),
    })
    // A write failure is rare (disk). Treat it as a non-admission so the
    // caller surfaces a refusal rather than a phantom success.
    .unwrap_or(Err(AdmitError::LowConfidence))
}

/// Record a rejection for OI-11 suppression. Trims the log to keep only
/// records still inside the suppression window (so it can't grow
/// unbounded). Called by the `user_profile.reject` handler.
pub fn record_rejection(field: &str, proposed: &str, now: i64, store: &mut ProfileStore) {
    store.rejected.push(RejectedRecord {
        field: field.to_owned(),
        proposed: proposed.to_owned(),
        rejected_at: now,
    });
    store
        .rejected
        .retain(|r| now.saturating_sub(r.rejected_at) < REJECTION_SUPPRESSION_SECS);
}

// ── Reflection trigger (scaffold) ─────────────────────────────────────

/// Minimal candidate extraction from one user message. **Scaffold** —
/// one high-precision heuristic so the post-turn path is exercised and
/// testable end-to-end; real LLM-driven synthesis is a later polish
/// item (see module docs).
///
/// Today it recognises an explicit naming request — "call me <Name>" /
/// "my name is <Name>" — and proposes a `name` update with modest
/// confidence (0.75, just over the gate) when the profile doesn't
/// already carry that name.
pub fn scan_user_message(
    text: &str,
    current_name: Option<&str>,
    conversation_id: Option<&str>,
) -> Option<ProposalCandidate> {
    let lower = text.to_ascii_lowercase();
    let marker = ["call me ", "my name is "]
        .into_iter()
        .find_map(|m| lower.find(m).map(|i| i + m.len()))?;
    // Take the matched name from the ORIGINAL text (preserve casing),
    // stopping at the first sentence/clause boundary.
    let tail = &text[marker..];
    let name: String = tail
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '\'' || *c == ' ')
        .collect::<String>()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_owned();
    if name.is_empty() || current_name == Some(name.as_str()) {
        return None;
    }
    Some(ProposalCandidate {
        field: "name".to_owned(),
        proposed: name,
        current: current_name.map(str::to_owned),
        rationale: "You asked to be addressed this way.".to_owned(),
        confidence: 0.75,
        conversation_id: conversation_id.map(str::to_owned),
    })
}

/// Post-turn reflection hook. Called by the chat turn driver after a
/// completed turn (Build Order §6 Slice D). Best-effort and infallible
/// from the caller's view — any refusal (gate, write error) is swallowed
/// so reflection can never affect the turn reply. Returns the minted
/// proposal id when one was admitted (mainly for tests / logging).
pub fn reflect_after_turn(conversation_id: &str, user_message: &str) -> Option<String> {
    let current_name = store::read().profile.name;
    let cand = scan_user_message(user_message, current_name.as_deref(), Some(conversation_id))?;
    match propose(cand) {
        Ok(p) => {
            tracing::debug!(
                proposal_id = %p.id,
                field = %p.field,
                "user_profile: post-turn reflection surfaced a proposal"
            );
            Some(p.id)
        }
        Err(e) => {
            tracing::trace!(
                reason = e.code(),
                "user_profile: reflection candidate not admitted"
            );
            None
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_profile::test_support::TestEnv;

    fn cand(field: &str, proposed: &str, conf: f64, cid: Option<&str>) -> ProposalCandidate {
        ProposalCandidate {
            field: field.to_owned(),
            proposed: proposed.to_owned(),
            current: None,
            rationale: "because".to_owned(),
            confidence: conf,
            conversation_id: cid.map(str::to_owned),
        }
    }

    #[test]
    fn admit_rejects_low_confidence() {
        let store = ProfileStore::default();
        let c = cand("style", "terse", 0.69, None);
        assert_eq!(admit(&store, &c, 1000), Err(AdmitError::LowConfidence));
        // Exactly at the threshold passes.
        let c = cand("style", "terse", 0.70, None);
        assert!(admit(&store, &c, 1000).is_ok());
    }

    #[test]
    fn admit_enforces_per_field_cooldown() {
        let mut store = ProfileStore::default();
        store
            .pending
            .push(mint(cand("style", "older", 0.9, None), 1000));
        // Same field, 30 min later → cooled down.
        let c = cand("style", "newer", 0.9, None);
        assert_eq!(
            admit(&store, &c, 1000 + 1800),
            Err(AdmitError::FieldCooldown)
        );
        // Past the hour → allowed.
        assert!(admit(&store, &c, 1000 + 3601).is_ok());
        // A different field is unaffected.
        let other = cand("name", "Sam", 0.9, None);
        assert!(admit(&store, &other, 1000 + 1800).is_ok());
    }

    #[test]
    fn admit_enforces_conversation_quota() {
        let mut store = ProfileStore::default();
        // Fill the quota for conversation "c1" with distinct fields so the
        // cooldown rule doesn't fire first.
        for i in 0..MAX_PROPOSALS_PER_CONVERSATION {
            store.pending.push(mint(
                cand(&format!("preference:k{i}"), "v", 0.9, Some("c1")),
                1000,
            ));
        }
        let c = cand("preference:knew", "v", 0.9, Some("c1"));
        assert_eq!(
            admit(&store, &c, 5000),
            Err(AdmitError::ConversationQuotaFull)
        );
        // A different conversation has its own budget.
        let other = cand("preference:knew", "v", 0.9, Some("c2"));
        assert!(admit(&store, &other, 5000).is_ok());
    }

    #[test]
    fn admit_suppresses_rejected_within_window() {
        let mut store = ProfileStore::default();
        record_rejection("style", "terse", 1000, &mut store);
        // Same field+value inside 30d → suppressed.
        let c = cand("style", "terse", 0.9, None);
        assert_eq!(
            admit(&store, &c, 1000 + 10 * 86_400),
            Err(AdmitError::Suppressed)
        );
        // Past 30d → no longer suppressed (and the cooldown is long past).
        assert!(admit(&store, &c, 1000 + REJECTION_SUPPRESSION_SECS + 1).is_ok());
        // A *different* value for the same field isn't suppressed (cooldown
        // also clear by now).
        let c2 = cand("style", "verbose", 0.9, None);
        assert!(admit(&store, &c2, 1000 + 4000).is_ok());
    }

    #[test]
    fn admit_refuses_exact_duplicate_pending() {
        let mut store = ProfileStore::default();
        store
            .pending
            .push(mint(cand("name", "Sam", 0.9, None), 1000));
        let dup = cand("name", "Sam", 0.9, None);
        assert_eq!(admit(&store, &dup, 5000), Err(AdmitError::DuplicatePending));
    }

    #[test]
    fn propose_persists_on_admission() {
        let _env = TestEnv::new();
        let p = propose(cand("style", "terse", 0.9, Some("c1"))).unwrap();
        assert_eq!(p.field, "style");
        let store = store::read();
        assert_eq!(store.pending.len(), 1);
        assert_eq!(store.pending[0].id, p.id);
    }

    #[test]
    fn propose_refuses_below_threshold_without_persisting() {
        let _env = TestEnv::new();
        assert_eq!(
            propose(cand("style", "x", 0.5, None)),
            Err(AdmitError::LowConfidence)
        );
        assert!(store::read().pending.is_empty());
    }

    #[test]
    fn record_rejection_trims_expired_records() {
        let mut store = ProfileStore::default();
        // An ancient rejection that should be pruned by a fresh one.
        store.rejected.push(RejectedRecord {
            field: "old".into(),
            proposed: "x".into(),
            rejected_at: 0,
        });
        record_rejection(
            "name",
            "Sam",
            REJECTION_SUPPRESSION_SECS + 100,
            &mut store,
        );
        // The ancient one (age > 30d at `now`) is gone; the fresh one stays.
        assert_eq!(store.rejected.len(), 1);
        assert_eq!(store.rejected[0].field, "name");
    }

    #[test]
    fn scan_detects_naming_request() {
        let c = scan_user_message("Hey, call me Sam please", None, Some("c1")).unwrap();
        assert_eq!(c.field, "name");
        assert_eq!(c.proposed, "Sam");
        assert!(c.confidence >= CONFIDENCE_THRESHOLD);

        // "my name is" variant.
        let c = scan_user_message("Actually my name is Wylde.", None, None).unwrap();
        assert_eq!(c.proposed, "Wylde");

        // No-op when the name already matches, or no marker present.
        assert!(scan_user_message("call me Sam", Some("Sam"), None).is_none());
        assert!(scan_user_message("what's the weather", None, None).is_none());
    }

    #[test]
    fn reflect_after_turn_admits_then_suppresses_repeat() {
        let _env = TestEnv::new();
        let id = reflect_after_turn("c1", "please call me Sam");
        assert!(id.is_some());
        assert_eq!(store::read().pending.len(), 1);
        // A second identical detection is refused (duplicate-pending), so
        // the queue doesn't grow.
        let again = reflect_after_turn("c1", "call me Sam");
        assert!(again.is_none());
        assert_eq!(store::read().pending.len(), 1);
    }
}
