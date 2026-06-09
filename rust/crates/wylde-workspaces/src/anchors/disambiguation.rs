//! Multi-match resolution + the "Anchor this as `{{…}}`?" prompt seed.
//!
//! When the composer recognises a word, it asks the anchor stores what that
//! token resolves to. Three outcomes (Plan v2 §5.1–5.2):
//!
//!   * **Resolved** — exactly one anchor. Spawn its bubble directly.
//!   * **Ambiguous** — several anchors share the token (e.g. a workspace and a
//!     global definition both exist). The composer shows a `?N` chip and a
//!     disambiguation dropdown.
//!   * **Unrecognised** — no anchor. The composer offers an *"Anchor this as
//!     `{{token}}`?"* affordance to mint a new one.
//!
//! Pure data helpers — no I/O, no UI. The store does the lookups; this
//! classifies the result and seeds the prompt.

use serde_json::{json, Value};

use super::anchor::Anchor;

/// The result of resolving one `{{token}}` against the merged anchor set.
#[derive(Clone, Debug, PartialEq)]
pub enum Disambiguation {
    /// Exactly one anchor — spawn it.
    Resolved(Anchor),
    /// Several candidates — the user must pick.
    Ambiguous(Vec<Anchor>),
    /// No anchor — offer to create one for `token`.
    Unrecognized { token: String },
}

impl Disambiguation {
    /// Classify `candidates` (already looked up from the stores) for `token`.
    pub fn resolve(token: &str, mut candidates: Vec<Anchor>) -> Self {
        match candidates.len() {
            0 => Disambiguation::Unrecognized {
                token: token.to_owned(),
            },
            1 => Disambiguation::Resolved(candidates.pop().expect("len==1")),
            _ => Disambiguation::Ambiguous(candidates),
        }
    }

    /// The number of candidates — what the composer renders in the `?N` /
    /// count chip.
    pub fn count(&self) -> usize {
        match self {
            Disambiguation::Resolved(_) => 1,
            Disambiguation::Ambiguous(v) => v.len(),
            Disambiguation::Unrecognized { .. } => 0,
        }
    }

    /// Whether the user must choose between candidates.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Disambiguation::Ambiguous(_))
    }
}

/// The "Anchor this as `{{token}}`?" prompt seed for an unrecognised token —
/// the affordance the composer shows to mint a new anchor.
pub fn anchor_this_seed(token: &str) -> Value {
    json!({
        "token": token,
        "message": format!("Anchor this as {{{{{token}}}}}?"),
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
    fn single_candidate_resolves() {
        let d = Disambiguation::resolve("x", vec![anchor("x")]);
        assert!(matches!(d, Disambiguation::Resolved(_)));
        assert_eq!(d.count(), 1);
        assert!(!d.is_ambiguous());
    }

    #[test]
    fn multiple_candidates_are_ambiguous() {
        let d = Disambiguation::resolve("x", vec![anchor("x"), anchor("x")]);
        assert!(d.is_ambiguous());
        assert_eq!(d.count(), 2);
    }

    #[test]
    fn no_candidate_is_unrecognized() {
        let d = Disambiguation::resolve("ghost", vec![]);
        assert_eq!(
            d,
            Disambiguation::Unrecognized {
                token: "ghost".into()
            }
        );
        assert_eq!(d.count(), 0);
    }

    #[test]
    fn anchor_this_seed_formats_token() {
        let seed = anchor_this_seed("new_thing");
        assert_eq!(seed["token"], "new_thing");
        assert_eq!(seed["message"], "Anchor this as {{new_thing}}?");
    }
}
