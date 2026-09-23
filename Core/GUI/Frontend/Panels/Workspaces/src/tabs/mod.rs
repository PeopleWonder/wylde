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
    /// Map a cross-panel focus-bus tab key (`"graph"`, `"editor"`, …) to an
    /// **in-workspace** tab. Returns `None` for keys that aren't a scoped tab
    /// — notably `"registry"`, which is the landing/home (handled by the back
    /// arrow, not a tab selection) and any unknown key.
    pub fn from_focus_key(key: &str) -> Option<WorkspacesTab> {
        match key {
            "files" => Some(WorkspacesTab::Files),
            "editor" => Some(WorkspacesTab::Editor),
            "graph" => Some(WorkspacesTab::Graph),
            "vocabulary" => Some(WorkspacesTab::Vocabulary),
            "settings" => Some(WorkspacesTab::Settings),
            _ => None,
        }
    }

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

    /// Tabs shown in the **in-workspace** tab bar, in display order.
    ///
    /// **Locked UX rework:** Registry is no longer a tab — it is the panel's
    /// landing/home (a list of workspaces). You ENTER a workspace by clicking
    /// its card; only then does this tab bar appear, scoped to that one
    /// workspace, with a back arrow returning to the Registry. So the bar is
    /// the IDE/management surfaces *inside* a workspace: `Files, Editor,
    /// Graph, Vocabulary, Settings` — IDE surfaces lead (file-tree, code,
    /// graph), management (Settings) trails. Adding a tab = add the variant,
    /// give it a label, place it here, wire its body.
    pub const WIRED: &'static [WorkspacesTab] = &[
        WorkspacesTab::Files,
        WorkspacesTab::Editor,
        WorkspacesTab::Graph,
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
    fn registry_is_not_an_in_workspace_tab() {
        // UX rework: Registry is the landing/home, NOT a tab in the
        // in-workspace bar — you enter a workspace to reveal its tabs.
        assert!(!WorkspacesTab::WIRED.contains(&WorkspacesTab::Registry));
    }

    #[test]
    fn wired_order_is_the_locked_ide_layout() {
        // IDE surfaces lead, management (Settings) trails; Registry dropped.
        assert_eq!(
            WorkspacesTab::WIRED,
            &[
                WorkspacesTab::Files,
                WorkspacesTab::Editor,
                WorkspacesTab::Graph,
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
    fn from_focus_key_maps_scoped_tabs_only() {
        assert_eq!(
            WorkspacesTab::from_focus_key("graph"),
            Some(WorkspacesTab::Graph)
        );
        assert_eq!(
            WorkspacesTab::from_focus_key("editor"),
            Some(WorkspacesTab::Editor)
        );
        assert_eq!(
            WorkspacesTab::from_focus_key("files"),
            Some(WorkspacesTab::Files)
        );
        // Registry is the home, not a scoped tab — the back arrow handles it.
        assert_eq!(WorkspacesTab::from_focus_key("registry"), None);
        assert_eq!(WorkspacesTab::from_focus_key("bogus"), None);
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
