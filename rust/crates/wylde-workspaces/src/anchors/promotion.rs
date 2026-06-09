//! Workspace → global promotion heuristic (Plan v2 §4.4, OI-21).
//!
//! An anchor that proves useful across **several conversations** is a
//! candidate for global vocabulary. The heuristic surfaces a prompt after the
//! anchor has been used in **3 distinct contexts** (conversations); promotion
//! is **never** implicit — the prompt is always user-confirmed.
//!
//! This module is the *data* half: it tracks distinct-context usage and emits
//! the prompt seed. The actual global create (and its collision handling,
//! OI-5) happens through the harness `anchors.create` verb; the collision
//! prompt is built in [`wylde_shared::anchor::already_exists_global_details`].

use std::collections::HashSet;

use serde_json::{json, Value};

use super::anchor::{Anchor, AnchorScope};

/// Distinct contexts (conversations) an anchor must be used in before the
/// promotion prompt fires.
pub const PROMOTION_THRESHOLD: usize = 3;

/// Tracks the distinct contexts a workspace anchor has been used in, to drive
/// the 3-context promotion heuristic. One tracker per anchor; the count is
/// derived from a deduplicated set of context ids, so re-using an anchor twice
/// in the same conversation counts once (it's *cross-context* breadth that
/// signals global relevance, not raw frequency).
#[derive(Clone, Debug, Default)]
pub struct PromotionTracker {
    contexts: HashSet<String>,
}

impl PromotionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a use of the anchor in `context_id` (e.g. a conversation id).
    /// Returns `true` if this was a **new** context (breadth increased).
    pub fn record(&mut self, context_id: &str) -> bool {
        self.contexts.insert(context_id.to_owned())
    }

    /// How many distinct contexts the anchor has been used in.
    pub fn distinct_contexts(&self) -> usize {
        self.contexts.len()
    }

    /// Whether the promotion prompt should fire now: the anchor is still
    /// workspace-scoped and it's been used in at least [`PROMOTION_THRESHOLD`]
    /// distinct contexts.
    pub fn should_prompt(&self, anchor: &Anchor) -> bool {
        should_prompt(anchor, self.distinct_contexts())
    }
}

/// Pure heuristic: a workspace-scoped anchor used across `distinct_contexts`
/// contexts is a promotion candidate once that reaches [`PROMOTION_THRESHOLD`].
/// An already-global anchor is never a candidate.
pub fn should_prompt(anchor: &Anchor, distinct_contexts: usize) -> bool {
    matches!(anchor.scope, AnchorScope::Workspace { .. })
        && distinct_contexts >= PROMOTION_THRESHOLD
}

/// The seed for the "Promote `{{X}}` to Global?" prompt the GUI renders.
/// Carries the identifier + current definition so the dialog needs no extra
/// round-trip. The three actions ([Yes, promote] / [Keep workspace-scoped] /
/// [Ask again later]) are the GUI's; this is just the data behind them.
pub fn prompt_seed(anchor: &Anchor, distinct_contexts: usize) -> Value {
    json!({
        "identifier": anchor.identifier,
        "definition": anchor.description,
        "distinct_contexts": distinct_contexts,
        "threshold": PROMOTION_THRESHOLD,
        "message": format!(
            "This anchor has been used in {distinct_contexts} separate contexts. \
             Promote {{{{{}}}}} to Global?",
            anchor.identifier
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::anchor::{workspace_anchor, AnchorKind, AnchorTarget};
    use wylde_shared::anchor::AnchorScope;

    fn ws_anchor() -> Anchor {
        workspace_anchor(
            "ws-1",
            "the_pipe_protocol",
            AnchorKind::Concept,
            AnchorTarget::Concept { text: "t".into() },
            "How services talk.",
        )
    }

    #[test]
    fn heuristic_fires_after_three_distinct_contexts() {
        let anchor = ws_anchor();
        let mut t = PromotionTracker::new();
        assert!(t.record("conv-a"));
        assert!(!t.should_prompt(&anchor), "1 context: no prompt");
        assert!(t.record("conv-b"));
        assert!(!t.should_prompt(&anchor), "2 contexts: no prompt");
        assert!(t.record("conv-c"));
        assert!(t.should_prompt(&anchor), "3 contexts: prompt fires");
        assert_eq!(t.distinct_contexts(), 3);
    }

    #[test]
    fn repeated_use_in_same_context_counts_once() {
        let anchor = ws_anchor();
        let mut t = PromotionTracker::new();
        assert!(t.record("conv-a"));
        assert!(!t.record("conv-a"), "same context is not new breadth");
        assert!(!t.record("conv-a"));
        assert_eq!(t.distinct_contexts(), 1);
        assert!(!t.should_prompt(&anchor), "frequency != breadth");
    }

    #[test]
    fn global_anchor_is_never_a_candidate() {
        let mut anchor = ws_anchor();
        anchor.scope = AnchorScope::Global;
        // Even well past the threshold, a global anchor never re-prompts.
        assert!(!should_prompt(&anchor, 10));
    }

    #[test]
    fn prompt_seed_carries_definition_and_count() {
        let anchor = ws_anchor();
        let seed = prompt_seed(&anchor, 3);
        assert_eq!(seed["identifier"], "the_pipe_protocol");
        assert_eq!(seed["definition"], "How services talk.");
        assert_eq!(seed["distinct_contexts"], 3);
        assert!(seed["message"]
            .as_str()
            .unwrap()
            .contains("{{the_pipe_protocol}}"));
    }
}
