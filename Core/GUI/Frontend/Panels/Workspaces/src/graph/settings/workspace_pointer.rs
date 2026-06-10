//! Per-workspace `last_profile` bookmark (Slice C-settings, Plan v2 §10).
//!
//! Each workspace remembers exactly one string — the name of the profile it
//! last used. The bookmarks live inside the same `graph_profiles.json` as
//! the library (one file, one atomic write), keyed by workspace id; this
//! module owns the pointer semantics: lookups validate the target still
//! exists, and setting an unknown profile is rejected rather than recorded.

use super::profiles::ProfileLibrary;

impl ProfileLibrary {
    /// The workspace's bookmarked profile name — `None` when the workspace
    /// has no bookmark *or* the bookmark dangles (its profile was removed),
    /// so callers always fall back to the default profile.
    pub fn pointer(&self, workspace_id: &str) -> Option<&str> {
        let name = self.workspace_pointers.get(workspace_id)?;
        self.get(name)?;
        Some(name.as_str())
    }

    /// Bookmark `profile` for `workspace_id`. Rejected (`false`) when no such
    /// profile exists — a bookmark must always be followable.
    pub fn set_pointer(&mut self, workspace_id: &str, profile: &str) -> bool {
        if self.get(profile).is_none() {
            return false;
        }
        self.workspace_pointers
            .insert(workspace_id.to_owned(), profile.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::profiles::{GraphProfile, ProfileLibrary, DEFAULT_PROFILE};
    use crate::graph::cluster::ClusterConfig;
    use crate::graph::layout::LayoutKind;
    use crate::graph::navigation::NavConfig;

    fn lib_with_focus() -> ProfileLibrary {
        let mut lib = ProfileLibrary::with_default();
        lib.upsert(GraphProfile::capture(
            "Focus",
            LayoutKind::Hierarchical,
            ClusterConfig::default(),
            NavConfig::default(),
            true,
        ));
        lib
    }

    #[test]
    fn set_and_read_pointer() {
        let mut lib = lib_with_focus();
        assert!(lib.set_pointer("ws-1", "Focus"));
        assert_eq!(lib.pointer("ws-1"), Some("Focus"));
        assert_eq!(lib.pointer("ws-never-seen"), None);
    }

    #[test]
    fn pointer_to_unknown_profile_is_rejected() {
        let mut lib = lib_with_focus();
        assert!(!lib.set_pointer("ws-1", "Nope"));
        assert_eq!(lib.pointer("ws-1"), None);
        assert!(lib.set_pointer("ws-1", DEFAULT_PROFILE));
    }

    #[test]
    fn dangling_pointer_reads_as_none() {
        let mut lib = lib_with_focus();
        lib.set_pointer("ws-1", "Focus");
        // Simulate an external edit that removed the profile but left the
        // bookmark (remove() prunes, but the file is user-editable).
        lib.profiles.retain(|p| p.name != "Focus");
        assert_eq!(lib.pointer("ws-1"), None, "dangling bookmark ignored");
    }
}
