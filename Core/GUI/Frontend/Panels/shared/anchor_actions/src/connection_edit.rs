//! Add / Remove Connection (Plan v2 §5.6–5.7, OI-22) — the pure
//! draft → validate → commit model every surface shares.
//!
//! A connection is an entry in an anchor's `related_to` list. The flows
//! that create one differ per surface (the Vocabulary editor's picker, the
//! expanded card's peer-to-peer clicks, the graph's drawing mode) but the
//! *rules* are identical and live here: no self-links, no duplicates, and
//! the commit produces the new `related_to` list for the caller's
//! `anchors.update`.

/// Why a draft can't commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    /// `from == to` — an anchor can't relate to itself.
    SelfLink,
    /// The link already exists in `related_to`.
    Duplicate,
    /// No target picked yet.
    NoTarget,
}

impl ConnectionError {
    pub fn message(&self) -> &'static str {
        match self {
            ConnectionError::SelfLink => "an anchor can't connect to itself",
            ConnectionError::Duplicate => "these anchors are already connected",
            ConnectionError::NoTarget => "pick a target anchor first",
        }
    }
}

/// An in-flight Add Connection: the source anchor + the (maybe) picked
/// target. Surfaces drive it differently — drawing mode sets `to` on drop,
/// the picker sets it on click — and both commit through [`Self::commit`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionDraft {
    pub from: String,
    pub to: Option<String>,
}

impl ConnectionDraft {
    pub fn new(from: impl Into<String>) -> Self {
        ConnectionDraft {
            from: from.into(),
            to: None,
        }
    }

    pub fn pick(&mut self, target: impl Into<String>) {
        self.to = Some(target.into());
    }

    /// Validate against the source anchor's current `related_to` and return
    /// the **new list** to persist. Pure — the caller runs the update verb
    /// (and pushes the inverse onto the undo stack).
    pub fn commit(&self, current_related: &[String]) -> Result<Vec<String>, ConnectionError> {
        let Some(to) = self.to.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(ConnectionError::NoTarget);
        };
        if to == self.from {
            return Err(ConnectionError::SelfLink);
        }
        if current_related.iter().any(|r| r == to) {
            return Err(ConnectionError::Duplicate);
        }
        let mut next = current_related.to_vec();
        next.push(to.to_owned());
        Ok(next)
    }
}

/// Remove Connection (Plan §5.7): the new `related_to` without `target`.
/// `None` when the link wasn't there (nothing to persist).
pub fn remove_connection(current_related: &[String], target: &str) -> Option<Vec<String>> {
    if !current_related.iter().any(|r| r == target) {
        return None;
    }
    Some(
        current_related
            .iter()
            .filter(|r| *r != target)
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn related() -> Vec<String> {
        vec!["wire_format".to_owned(), "ipc".to_owned()]
    }

    #[test]
    fn commit_appends_a_valid_target() {
        let mut d = ConnectionDraft::new("the_pipe");
        assert_eq!(d.commit(&related()), Err(ConnectionError::NoTarget));
        d.pick("retry_policy");
        let next = d.commit(&related()).expect("valid");
        assert_eq!(next, vec!["wire_format", "ipc", "retry_policy"]);
    }

    #[test]
    fn self_links_and_duplicates_rejected() {
        let mut d = ConnectionDraft::new("the_pipe");
        d.pick("the_pipe");
        assert_eq!(d.commit(&related()), Err(ConnectionError::SelfLink));
        d.pick("ipc");
        assert_eq!(d.commit(&related()), Err(ConnectionError::Duplicate));
        assert!(!ConnectionError::Duplicate.message().is_empty());
    }

    #[test]
    fn remove_returns_the_pruned_list_or_none() {
        assert_eq!(
            remove_connection(&related(), "ipc"),
            Some(vec!["wire_format".to_owned()])
        );
        assert_eq!(remove_connection(&related(), "never_there"), None);
    }
}
