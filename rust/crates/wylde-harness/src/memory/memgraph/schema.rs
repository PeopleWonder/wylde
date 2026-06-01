//! Memgraph graph schema constants — node labels, relation types,
//! and the traversal "bucket" names.
//!
//! Mirrors what the live `wylde-memgraph` service expects on its
//! `/traverse`, `/relate`, and `/unrelate` routes. The server is the
//! source of truth (it validates `rel_type` server-side); we duplicate
//! the names here so callers can pre-check before paying for a pipe
//! round-trip, and so the bucket strings used by `traverse`'s
//! `rel_depths` payload stay in one place.

/// Entity → Entity relation: direct call site references.
pub const REL_CALLS: &str = "CALLS";
/// Entity → Entity relation: module / package imports.
pub const REL_IMPORTS: &str = "IMPORTS";
/// Entity → Entity relation: class / trait inheritance.
pub const REL_INHERITS: &str = "INHERITS";
/// Entity → Entity relation: configuration applied to a target entity.
pub const REL_CONFIGURES: &str = "CONFIGURES";
/// Entity → Entity relation: outward-facing surface (route, action,
/// public symbol) exposing a target entity.
pub const REL_EXPOSES: &str = "EXPOSES";
/// Entity → Chunk relation: an entity is mentioned in a chunk of text.
pub const REL_MENTIONED_IN: &str = "MENTIONED_IN";

/// Code-symbol nodes (functions, classes, modules, configs).
pub const NODE_ENTITY: &str = "Entity";
/// Indexed text chunks (source-file fragments).
pub const NODE_CHUNK: &str = "Chunk";

/// Traversal bucket: CALLS / IMPORTS / INHERITS — typically 1-hop.
pub const BUCKET_CALLS_IMPORTS: &str = "calls_imports";
/// Traversal bucket: CONFIGURES / EXPOSES — typically 2-hop.
pub const BUCKET_CONFIGURES_EXPOSES: &str = "configures_exposes";

/// True iff `rel` is one of the five Entity→Entity relation types the
/// server's `/relate` route accepts.
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
    fn relation_type_validation_matches_python_route() {
        for rel in ["CALLS", "IMPORTS", "INHERITS", "CONFIGURES", "EXPOSES"] {
            assert!(relation_type_is_valid(rel), "{rel} should be valid");
        }
        for rel in ["", "CALL", "calls", "MENTIONED_IN", "RANDOM"] {
            assert!(!relation_type_is_valid(rel), "{rel} should be rejected");
        }
    }

    #[test]
    fn node_labels_stable() {
        // If these change, the route Cypher queries change too — pin them.
        assert_eq!(NODE_ENTITY, "Entity");
        assert_eq!(NODE_CHUNK, "Chunk");
    }

    #[test]
    fn bucket_names_match_traverse_route_keys() {
        // `/traverse` looks up these literal keys in body["rel_depths"].
        assert_eq!(BUCKET_CALLS_IMPORTS, "calls_imports");
        assert_eq!(BUCKET_CONFIGURES_EXPOSES, "configures_exposes");
    }
}
