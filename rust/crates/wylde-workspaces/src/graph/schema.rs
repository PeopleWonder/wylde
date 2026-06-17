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

// ── Concept layer (TBS concept-system, thesis §2.2) ───────────────────────
// The concept graph projection. The JSON store ([`crate::concepts::store`]) is
// authoritative; these node/edge types let the build pass additively project
// concepts into Neo4j so the graph panel can render concept nodes. Never the
// read path for the browse surface.

/// A discovered semantic theme (the **Concepts** layer).
pub const NODE_CONCEPT: &str = "Concept";

/// Concept → Entity|Chunk: the concept's member code (many-to-many ⇒ overlap).
pub const REL_MEMBER: &str = "MEMBER";
/// Concept → Concept | Term → Term: hierarchy (a DAG for concepts).
pub const REL_CHILD_OF: &str = "CHILD_OF";
/// Concept → Term: vocabulary that names a concept.
pub const REL_DESCRIBED_BY: &str = "DESCRIBED_BY";

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

    #[test]
    fn concept_layer_labels_stable() {
        assert_eq!(NODE_CONCEPT, "Concept");
        assert_eq!(REL_MEMBER, "MEMBER");
        assert_eq!(REL_CHILD_OF, "CHILD_OF");
        assert_eq!(REL_DESCRIBED_BY, "DESCRIBED_BY");
    }
}
