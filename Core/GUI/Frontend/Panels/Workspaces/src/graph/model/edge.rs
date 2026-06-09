//! A typed graph edge — the GUI-side mirror of the `workspaces.graph` verb's
//! `Edge` (Slice B). `rel_type` serialises in SCREAMING_SNAKE_CASE to match
//! Neo4j's `type(r)` strings and the service's `RelType`.
//!
//! Canonical home for `Edge` (Build Order Appendix B → GUI Workspaces ·
//! `graph/model/edge.rs`).

use serde::{Deserialize, Serialize};

/// The Entity→Entity relation vocabulary. Mirrors the service's `RelType`
/// (Calls/Imports/Inherits/Configures/Exposes) plus `RelatedTo` for the
/// future vocabulary overlay (Slice N) and an `Unknown` catch-all so a newer
/// graph still deserialises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RelType {
    Calls,
    Imports,
    Inherits,
    Configures,
    Exposes,
    /// Conceptual link between anchors (vocabulary overlay; Slice N).
    RelatedTo,
    /// Forward-compat catch-all for relation types added later.
    #[serde(other)]
    Unknown,
}

impl RelType {
    /// The `edges` key this relation maps to in the Visual Style theme
    /// (`render/style.rs`). One place owns the rel→theme mapping.
    pub fn theme_key(self) -> &'static str {
        match self {
            RelType::Calls => "calls",
            RelType::Imports => "imports",
            RelType::Inherits => "inherits",
            RelType::Configures => "configures",
            RelType::Exposes => "exposes",
            RelType::RelatedTo => "related_to",
            // Render unknown relations with the neutral "calls" style.
            RelType::Unknown => "calls",
        }
    }
}

/// A directed, typed edge between two node ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub rel_type: RelType,
    #[serde(default = "default_weight")]
    pub weight: f32,
}

fn default_weight() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialises_service_wire_edge() {
        let v = json!({ "src": "alpha", "dst": "beta", "rel_type": "CALLS", "weight": 1.0 });
        let e: Edge = serde_json::from_value(v).unwrap();
        assert_eq!(e.rel_type, RelType::Calls);
        assert_eq!(e.theme_key_for(), "calls");
    }

    impl Edge {
        fn theme_key_for(&self) -> &'static str {
            self.rel_type.theme_key()
        }
    }

    #[test]
    fn all_service_rel_types_round_trip() {
        for (wire, rel) in [
            ("CALLS", RelType::Calls),
            ("IMPORTS", RelType::Imports),
            ("INHERITS", RelType::Inherits),
            ("CONFIGURES", RelType::Configures),
            ("EXPOSES", RelType::Exposes),
            ("RELATED_TO", RelType::RelatedTo),
        ] {
            let v = json!({ "src": "a", "dst": "b", "rel_type": wire });
            let e: Edge = serde_json::from_value(v).unwrap();
            assert_eq!(e.rel_type, rel, "{wire}");
            assert_eq!(
                serde_json::to_value(rel).unwrap(),
                serde_json::Value::String(wire.to_owned())
            );
        }
    }

    #[test]
    fn unknown_rel_type_falls_back() {
        let v = json!({ "src": "a", "dst": "b", "rel_type": "MENTIONED_IN" });
        let e: Edge = serde_json::from_value(v).unwrap();
        assert_eq!(e.rel_type, RelType::Unknown);
        assert_eq!(e.rel_type.theme_key(), "calls");
    }

    #[test]
    fn weight_defaults_to_one() {
        let v = json!({ "src": "a", "dst": "b", "rel_type": "CALLS" });
        let e: Edge = serde_json::from_value(v).unwrap();
        assert_eq!(e.weight, 1.0);
    }
}
