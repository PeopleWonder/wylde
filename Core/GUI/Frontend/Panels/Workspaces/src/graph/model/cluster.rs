//! A collapsible group of nodes — the GUI-side mirror of the
//! `workspaces.graph` verb's `Cluster` (Slice B; v1 = one per file parent
//! directory). The *behaviour* (auto-clustering, expand-in-place, zoom
//! thresholds) lands in Slice C-cluster; C-scaffold only carries the data so
//! the wire shape round-trips and later slices have the model in place.
//!
//! Canonical home for `Cluster` (Build Order Appendix B → GUI Workspaces ·
//! `graph/model/cluster.rs`).

use serde::{Deserialize, Serialize};

/// A named group of node ids with a zoom threshold at which it collapses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    #[serde(default)]
    pub member_ids: Vec<String>,
    /// Path-derived breadcrumb (v1); the cluster-hierarchy breadcrumb is
    /// C-cluster's job.
    #[serde(default)]
    pub parent_breadcrumb: Vec<String>,
    #[serde(default = "default_zoom_threshold")]
    pub zoom_threshold: f32,
}

fn default_zoom_threshold() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialises_service_wire_cluster() {
        let v = json!({
            "id": "C:/ws/src",
            "member_ids": ["alpha", "beta"],
            "parent_breadcrumb": ["ws", "src"],
            "zoom_threshold": 1.0
        });
        let c: Cluster = serde_json::from_value(v).unwrap();
        assert_eq!(c.id, "C:/ws/src");
        assert_eq!(c.member_ids.len(), 2);
        assert_eq!(c.parent_breadcrumb, vec!["ws", "src"]);
    }

    #[test]
    fn defaults_fill_when_missing() {
        let c: Cluster = serde_json::from_value(json!({ "id": "x" })).unwrap();
        assert!(c.member_ids.is_empty());
        assert!(c.parent_breadcrumb.is_empty());
        assert_eq!(c.zoom_threshold, 1.0);
    }
}
