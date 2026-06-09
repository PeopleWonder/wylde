//! Which graph layer the panel is showing. C-scaffold renders only
//! `CodeGraph`; `VocabularyGraph` (the anchor world-model) and `Overlay`
//! (both layered on one canvas) arrive with Slice N. Carried now so the
//! renderer + viewport can be written mode-aware from the start (Build Order
//! Appendix B → `graph/model/view_mode.rs`).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    /// The CALLS/IMPORTS/INHERITS code graph from Neo4j. The only mode
    /// C-scaffold renders.
    #[default]
    CodeGraph,
    /// The saved-anchor vocabulary graph (Slice N).
    VocabularyGraph,
    /// Code + vocabulary layered on one canvas (Slice N).
    Overlay,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_code_graph() {
        assert_eq!(ViewMode::default(), ViewMode::CodeGraph);
    }
}
