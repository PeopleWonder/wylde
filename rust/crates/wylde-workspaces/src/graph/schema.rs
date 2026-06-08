//! Graph schema constants — node labels + the Entity→Entity relation
//! vocabulary the upsert/relate Cypher writes.
//!
//! A narrow relocation of the harness `memory::memgraph::schema` (Slice 0b):
//! only the names the workspace graph-ingest path needs. The Neo4j server is
//! the source of truth (it accepts any rel type), but we pre-validate so an
//! invalid `rel_type` never reaches the interpolated Cypher.

/// Entity → Entity: direct call-site reference.
pub const REL_CALLS: &str = "CALLS";
/// Entity → Entity: module / package import.
pub const REL_IMPORTS: &str = "IMPORTS";
/// Entity → Entity: class / trait inheritance.
pub const REL_INHERITS: &str = "INHERITS";
/// Entity → Entity: configuration applied to a target entity.
pub const REL_CONFIGURES: &str = "CONFIGURES";
/// Entity → Entity: outward-facing surface exposing a target entity.
pub const REL_EXPOSES: &str = "EXPOSES";
/// Entity → Chunk: an entity is mentioned in a chunk of text.
pub const REL_MENTIONED_IN: &str = "MENTIONED_IN";

/// Code-symbol nodes (functions, classes, modules, configs).
pub const NODE_ENTITY: &str = "Entity";
/// Indexed text chunks (source-file fragments).
pub const NODE_CHUNK: &str = "Chunk";

/// True iff `rel` is one of the five Entity→Entity relation types the graph
/// accepts on a `relate` write.
pub fn relation_type_is_valid(rel: &str) -> bool {
    matches!(
        rel,
        REL_CALLS | REL_IMPORTS | REL_INHERITS | REL_CONFIGURES | REL_EXPOSES
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_type_validation() {
        for rel in ["CALLS", "IMPORTS", "INHERITS", "CONFIGURES", "EXPOSES"] {
            assert!(relation_type_is_valid(rel), "{rel} should be valid");
        }
        for rel in ["", "CALL", "calls", "MENTIONED_IN", "RANDOM"] {
            assert!(!relation_type_is_valid(rel), "{rel} should be rejected");
        }
    }

    #[test]
    fn node_labels_stable() {
        assert_eq!(NODE_ENTITY, "Entity");
        assert_eq!(NODE_CHUNK, "Chunk");
    }
}
