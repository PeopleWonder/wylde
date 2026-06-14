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
    /// Stays the default selection — you pick/add an active workspace here
    /// before the Files/Editor surfaces have anything to show.
    #[default]
    Registry,
    /// The lazy file-tree of the active workspace's folder (IDE S5).
    Files,
    /// The code editor (IDE S3/S4) — a Workspaces TAB only (OQ-8), never a
    /// top-level left-nav panel.
    Editor,
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
            WorkspacesTab::Files => "Files",
            WorkspacesTab::Editor => "Editor",
            WorkspacesTab::Graph => "Graph",
            WorkspacesTab::Vocabulary => "Vocabulary",
            WorkspacesTab::Conversations => "Conversations",
            WorkspacesTab::Settings => "Settings",
        }
    }

    /// Tabs that have a body wired today, in display order.
    ///
    /// **Locked order (IDE OQ-5):** `Files, Editor, Graph, Registry,
    /// Vocabulary, Settings` — the IDE surfaces lead (the panel reads
    /// left→right like an editor: file-tree, code, graph), with the
    /// management tabs (Registry, Settings) demoted to the right. Adding a
    /// tab = add the variant, give it a label, place it here, wire its body.
    pub const WIRED: &'static [WorkspacesTab] = &[
        WorkspacesTab::Files,
        WorkspacesTab::Editor,
        WorkspacesTab::Graph,
        WorkspacesTab::Registry,
        WorkspacesTab::Vocabulary,
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
    fn wired_order_is_the_locked_ide_layout() {
        // IDE OQ-5: IDE surfaces lead, management tabs trail.
        assert_eq!(
            WorkspacesTab::WIRED,
            &[
                WorkspacesTab::Files,
                WorkspacesTab::Editor,
                WorkspacesTab::Graph,
                WorkspacesTab::Registry,
                WorkspacesTab::Vocabulary,
                WorkspacesTab::Settings
            ]
        );
    }

    #[test]
    fn ide_tabs_are_wired_and_labelled() {
        assert!(WorkspacesTab::WIRED.contains(&WorkspacesTab::Files));
        assert!(WorkspacesTab::WIRED.contains(&WorkspacesTab::Editor));
        assert_eq!(WorkspacesTab::Files.label(), "Files");
        assert_eq!(WorkspacesTab::Editor.label(), "Editor");
    }

    #[test]
    fn every_tab_has_a_label() {
        for t in [
            WorkspacesTab::Registry,
            WorkspacesTab::Files,
            WorkspacesTab::Editor,
            WorkspacesTab::Graph,
            WorkspacesTab::Vocabulary,
            WorkspacesTab::Conversations,
            WorkspacesTab::Settings,
        ] {
            assert!(!t.label().is_empty());
        }
    }
}
