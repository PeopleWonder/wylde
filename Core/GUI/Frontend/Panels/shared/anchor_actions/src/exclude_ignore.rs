//! Exclude vs Ignore — the §5.8 state machine, shared verbatim across the
//! three surfaces.
//!
//! The critical distinction (Plan v2 §5.8, maintained everywhere):
//!
//! | Action  | Scope                | Behaviour |
//! |---------|----------------------|-----------|
//! | Exclude | This message only    | Inactive now; resets next message |
//! | Ignore  | Durable (3 tiers)    | Still visible, spawns default-inactive; ↺ reactivates per message |

/// Which durable tier an Ignore lives in (Slice M's stores).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IgnoreTier {
    Conversation,
    Workspace,
    Global,
}

impl IgnoreTier {
    pub fn label(self) -> &'static str {
        match self {
            IgnoreTier::Conversation => "conversation",
            IgnoreTier::Workspace => "workspace",
            IgnoreTier::Global => "global",
        }
    }
}

/// One anchor occurrence's activation state for the *current message*.
/// Durable ignore membership is carried alongside (`ignored_tiers`); this
/// enum is the per-message overlay on top of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activation {
    /// Rides along with the send.
    #[default]
    Active,
    /// ✕ — excluded for this message only.
    Excluded,
    /// ↺ — an ignored item the user reactivated for this message.
    Reactivated,
}

impl Activation {
    /// Will this occurrence's context actually accompany the send, given
    /// whether any durable ignore tier covers it?
    pub fn is_included(self, is_ignored: bool) -> bool {
        match self {
            Activation::Excluded => false,
            Activation::Reactivated => true,
            Activation::Active => !is_ignored,
        }
    }

    /// The ✕ click: exclude ⇄ restore. (Plan §5.4 — ✕ excludes, ↺ restores;
    /// one affordance, two glyphs.)
    pub fn toggle_excluded(self) -> Activation {
        match self {
            Activation::Excluded => Activation::Active,
            _ => Activation::Excluded,
        }
    }

    /// The ↺ click on an ignored item: reactivate ⇄ back to default-inactive.
    pub fn toggle_reactivated(self) -> Activation {
        match self {
            Activation::Reactivated => Activation::Active,
            _ => Activation::Reactivated,
        }
    }

    /// A new message begins: every per-message override resets (Excluded
    /// resets per §5.8's "this time, not this"; Reactivated likewise — the
    /// ignore is the durable default).
    pub fn reset_for_new_message(self) -> Activation {
        Activation::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_follows_5_8() {
        // Plain active item rides along; ignored item doesn't…
        assert!(Activation::Active.is_included(false));
        assert!(!Activation::Active.is_included(true));
        // …unless reactivated. Excluded never rides.
        assert!(Activation::Reactivated.is_included(true));
        assert!(!Activation::Excluded.is_included(false));
        assert!(!Activation::Excluded.is_included(true));
    }

    #[test]
    fn toggles_are_involutive_and_message_reset_clears() {
        let a = Activation::Active;
        assert_eq!(a.toggle_excluded(), Activation::Excluded);
        assert_eq!(a.toggle_excluded().toggle_excluded(), Activation::Active);
        assert_eq!(a.toggle_reactivated(), Activation::Reactivated);
        assert_eq!(
            a.toggle_reactivated().toggle_reactivated(),
            Activation::Active
        );
        for s in [
            Activation::Active,
            Activation::Excluded,
            Activation::Reactivated,
        ] {
            assert_eq!(s.reset_for_new_message(), Activation::Active);
        }
    }

    #[test]
    fn tier_labels_match_the_wire() {
        assert_eq!(IgnoreTier::Conversation.label(), "conversation");
        assert_eq!(IgnoreTier::Workspace.label(), "workspace");
        assert_eq!(IgnoreTier::Global.label(), "global");
    }
}
