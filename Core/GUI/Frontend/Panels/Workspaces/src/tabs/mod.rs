//! Minimal tab system for the Workspaces panel (Build Order §4).
//!
//! The panel is gaining sub-views: the existing Registry, the new Graph
//! (Slice C-scaffold), and — in later slices — Vocabulary (Slice N),
//! Conversations, and Settings. This is the simple enum + switch the brief
//! calls for: no tab-manager abstraction, just a [`WorkspacesTab`] selector
//! the panel renders a button row from and matches on.
//!
//! The enum carries the **full** spec §4 set so the ordering is canonical and
//! later slices only have to wire their body + add themselves to
//! [`WorkspacesTab::WIRED`]; C-scaffold renders buttons for the two tabs that
//! actually have bodies today (Registry, Graph).

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspacesTab {
    /// The MRU workspace list + add/switch/reindex/remove (the original panel).
    #[default]
    Registry,
    /// The visual code graph (Slice C-scaffold → C-settings).
    Graph,
    /// The anchor world-model / vocabulary editor (Slice N).
    Vocabulary,
    /// The per-workspace conversation switcher (moved here in a later slice).
    Conversations,
    /// Graph + panel settings (Slice C-settings).
    Settings,
}

impl WorkspacesTab {
    /// Short tab-bar label.
    pub fn label(self) -> &'static str {
        match self {
            WorkspacesTab::Registry => "Registry",
            WorkspacesTab::Graph => "Graph",
            WorkspacesTab::Vocabulary => "Vocabulary",
            WorkspacesTab::Conversations => "Conversations",
            WorkspacesTab::Settings => "Settings",
        }
    }

    /// Tabs that have a body wired today, in display order. Later slices add
    /// their tab here when they ship its view. Graph sits right after Registry
    /// (before Conversations in the canonical ordering), per the brief;
    /// Settings (Slice C-settings) closes the row.
    pub const WIRED: &'static [WorkspacesTab] = &[
        WorkspacesTab::Registry,
        WorkspacesTab::Graph,
        WorkspacesTab::Settings,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tab_is_registry() {
        assert_eq!(WorkspacesTab::default(), WorkspacesTab::Registry);
    }

    #[test]
    fn graph_follows_registry_in_wired_order() {
        assert_eq!(
            WorkspacesTab::WIRED,
            &[
                WorkspacesTab::Registry,
                WorkspacesTab::Graph,
                WorkspacesTab::Settings
            ]
        );
    }

    #[test]
    fn every_tab_has_a_label() {
        for t in [
            WorkspacesTab::Registry,
            WorkspacesTab::Graph,
            WorkspacesTab::Vocabulary,
            WorkspacesTab::Conversations,
            WorkspacesTab::Settings,
        ] {
            assert!(!t.label().is_empty());
        }
    }
}
