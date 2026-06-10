//! Approval gating for permanent changes (Plan v2 §6 `approval_gate`).
//!
//! Some anchor edits are reversible per-message state; others are permanent
//! (replace a global definition, delete an anchor, promote). Every surface
//! gates those behind the same two-step shape: **request** (capture the
//! pending action + a human description) → **confirm** (take it) or
//! **cancel**. The OI-5 "Replace requires explicit confirmation" flow is an
//! instance of this gate.

/// A pending permanent action of type `T`, awaiting explicit confirmation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApprovalGate<T> {
    pending: Option<(String, T)>,
}

impl<T> ApprovalGate<T> {
    pub fn new() -> Self {
        ApprovalGate { pending: None }
    }

    /// Stage `action` behind the gate, replacing any prior pending action
    /// (one decision at a time — a second request supersedes, never queues).
    pub fn request(&mut self, description: impl Into<String>, action: T) {
        self.pending = Some((description.into(), action));
    }

    /// The pending action's human description, if one is staged.
    pub fn description(&self) -> Option<&str> {
        self.pending.as_ref().map(|(d, _)| d.as_str())
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The explicit yes: take the action out (the caller executes it).
    pub fn confirm(&mut self) -> Option<T> {
        self.pending.take().map(|(_, a)| a)
    }

    /// The no: drop the pending action.
    pub fn cancel(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_confirm_executes_exactly_once() {
        let mut gate: ApprovalGate<&'static str> = ApprovalGate::new();
        assert!(!gate.is_pending());
        gate.request("Replace the global definition of {{x}}?", "replace-x");
        assert!(gate.is_pending());
        assert!(gate.description().unwrap().contains("{{x}}"));
        assert_eq!(gate.confirm(), Some("replace-x"));
        assert!(!gate.is_pending());
        assert_eq!(gate.confirm(), None, "second confirm is empty");
    }

    #[test]
    fn cancel_drops_and_new_request_supersedes() {
        let mut gate = ApprovalGate::new();
        gate.request("delete {{a}}?", 1);
        gate.cancel();
        assert!(!gate.is_pending());

        gate.request("delete {{a}}?", 1);
        gate.request("delete {{b}}?", 2);
        assert_eq!(gate.confirm(), Some(2), "latest request wins");
    }
}
