//! Named graph-settings profiles (Slice C-settings).
//!
//! A profile is a **full snapshot** of every graph tunable (Plan v2 §10 /
//! Build Order Appendix B: `GraphSettings` + `ThemeSettings` +
//! `InteractionSettings`), so switching profiles swaps the whole view feel
//! at once. The library is global (one `graph_profiles.json` for all
//! workspaces); each workspace remembers only which profile it last used
//! ([`super::workspace_pointer`]).
//!
//! Everything is `#[serde(default)]`-tolerant: profiles saved by older
//! builds keep loading as knobs are added.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::graph::cluster::ClusterConfig;
use crate::graph::layout::LayoutKind;
use crate::graph::navigation::NavConfig;

/// Graph structure/behaviour settings (Appendix B `GraphSettings`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphSettings {
    /// Layout backend by persisted name (`LayoutKind::name`); unknown names
    /// fall back to the default backend.
    pub layout: String,
    /// Auto-clustering knobs (C-cluster).
    pub cluster: ClusterConfig,
}

impl Default for GraphSettings {
    fn default() -> Self {
        GraphSettings {
            layout: LayoutKind::default().name().to_owned(),
            cluster: ClusterConfig::default(),
        }
    }
}

impl GraphSettings {
    pub fn layout_kind(&self) -> LayoutKind {
        LayoutKind::from_name(&self.layout).unwrap_or_default()
    }
}

/// Visual mode settings (Appendix B `ThemeSettings`). The Theme itself is
/// the locked Visual Style v1 — what a profile snapshots is which *mode* of
/// it renders (and, later, per-profile overrides).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub dark: bool,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        ThemeSettings { dark: true }
    }
}

/// Input-feel settings (Appendix B `InteractionSettings`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InteractionSettings {
    /// Space-map navigation knobs (C-navigation).
    pub navigation: NavConfig,
}

/// One named, complete settings snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphProfile {
    pub name: String,
    pub graph: GraphSettings,
    pub theme: ThemeSettings,
    pub interaction: InteractionSettings,
}

impl GraphProfile {
    /// Snapshot the live knobs under `name`.
    pub fn capture(
        name: &str,
        layout: LayoutKind,
        cluster: ClusterConfig,
        navigation: NavConfig,
        dark: bool,
    ) -> GraphProfile {
        GraphProfile {
            name: name.to_owned(),
            graph: GraphSettings {
                layout: layout.name().to_owned(),
                cluster,
            },
            theme: ThemeSettings { dark },
            interaction: InteractionSettings { navigation },
        }
    }
}

/// The default profile's reserved name.
pub const DEFAULT_PROFILE: &str = "Default";

/// The global profile library + per-workspace bookmarks — the exact shape of
/// `graph_profiles.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileLibrary {
    pub profiles: Vec<GraphProfile>,
    /// workspace id → last-used profile name (Plan §10: "per-workspace
    /// stores only a `last_profile` bookmark string").
    pub workspace_pointers: HashMap<String, String>,
}

impl ProfileLibrary {
    /// A library guaranteed to contain the code-default profile — what a
    /// fresh install (or a corrupt file) resolves to.
    pub fn with_default() -> ProfileLibrary {
        let mut lib = ProfileLibrary::default();
        lib.ensure_default();
        lib
    }

    /// Insert the code-default profile if no profile carries its name.
    pub fn ensure_default(&mut self) {
        if self.get(DEFAULT_PROFILE).is_none() {
            self.profiles.insert(
                0,
                GraphProfile {
                    name: DEFAULT_PROFILE.to_owned(),
                    ..GraphProfile::default()
                },
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<&GraphProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    /// Insert or replace by name. Empty names are rejected (no-op `false`).
    pub fn upsert(&mut self, profile: GraphProfile) -> bool {
        if profile.name.trim().is_empty() {
            return false;
        }
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
        true
    }

    /// Remove a profile by name; bookmarks pointing at it are dropped so
    /// workspaces fall back to the default. The default profile itself is
    /// not removable (`false`).
    pub fn remove(&mut self, name: &str) -> bool {
        if name == DEFAULT_PROFILE {
            return false;
        }
        let before = self.profiles.len();
        self.profiles.retain(|p| p.name != name);
        let removed = self.profiles.len() != before;
        if removed {
            self.workspace_pointers.retain(|_, v| v != name);
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_round_trips_through_json() {
        let p = GraphProfile::capture(
            "Focus",
            LayoutKind::Hierarchical,
            ClusterConfig {
                auto_threshold_nodes: 100,
                ..ClusterConfig::default()
            },
            NavConfig {
                zoom_step_factor: 1.25,
                ..NavConfig::default()
            },
            false,
        );
        let json = serde_json::to_string(&p).unwrap();
        let back: GraphProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.graph.layout_kind(), LayoutKind::Hierarchical);
        assert_eq!(back.graph.cluster.auto_threshold_nodes, 100);
        assert!((back.interaction.navigation.zoom_step_factor - 1.25).abs() < 1e-6);
        assert!(!back.theme.dark);
    }

    #[test]
    fn old_profile_missing_fields_loads_with_defaults() {
        // A profile written before a knob existed: only a name.
        let back: GraphProfile = serde_json::from_str(r#"{ "name": "Old" }"#).unwrap();
        assert_eq!(back.name, "Old");
        assert_eq!(back.graph.layout_kind(), LayoutKind::default());
        assert_eq!(back.graph.cluster, ClusterConfig::default());
        assert_eq!(back.interaction.navigation, NavConfig::default());
        assert!(back.theme.dark, "dark is the default mode");
        // Unknown layout names also degrade to the default backend.
        let odd: GraphSettings = serde_json::from_str(r#"{ "layout": "holographic" }"#).unwrap();
        assert_eq!(odd.layout_kind(), LayoutKind::default());
    }

    #[test]
    fn library_always_offers_the_default_profile() {
        let lib = ProfileLibrary::with_default();
        assert!(lib.get(DEFAULT_PROFILE).is_some());
        assert_eq!(lib.names(), vec![DEFAULT_PROFILE]);
    }

    #[test]
    fn upsert_replaces_by_name_and_rejects_empty() {
        let mut lib = ProfileLibrary::with_default();
        let mut p = GraphProfile::capture(
            "Focus",
            LayoutKind::StableGrid,
            ClusterConfig::default(),
            NavConfig::default(),
            true,
        );
        assert!(lib.upsert(p.clone()));
        assert_eq!(lib.profiles.len(), 2);

        p.theme.dark = false;
        assert!(lib.upsert(p));
        assert_eq!(lib.profiles.len(), 2, "same name replaces");
        assert!(!lib.get("Focus").unwrap().theme.dark);

        assert!(!lib.upsert(GraphProfile::default()), "empty name rejected");
        assert_eq!(lib.profiles.len(), 2);
    }

    #[test]
    fn remove_guards_default_and_prunes_pointers() {
        let mut lib = ProfileLibrary::with_default();
        lib.upsert(GraphProfile::capture(
            "Focus",
            LayoutKind::default(),
            ClusterConfig::default(),
            NavConfig::default(),
            true,
        ));
        lib.set_pointer("ws-1", "Focus");
        lib.set_pointer("ws-2", DEFAULT_PROFILE);

        assert!(!lib.remove(DEFAULT_PROFILE), "default is permanent");
        assert!(lib.remove("Focus"));
        assert!(lib.get("Focus").is_none());
        assert_eq!(lib.pointer("ws-1"), None, "dangling bookmark pruned");
        assert_eq!(lib.pointer("ws-2"), Some(DEFAULT_PROFILE));
        assert!(!lib.remove("Focus"), "second remove is a no-op");
    }
}
