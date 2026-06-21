//! Pure scoring — cosine of the query vector against a concept centroid.
//!
//! Deliberately mirrors the shape of the two cosines already in the tree
//! (`wylde-workspaces/src/concepts/search.rs::cosine` and
//! `rag/mod.rs::cosine`): defensive on length mismatch / empty / zero-norm
//! (all → `0.0`), and it normalises rather than assuming unit vectors, because
//! a centroid is a *mean* of unit embeddings and so is not itself unit-length.

/// Cosine similarity in `[-1, 1]` of two equal-length vectors. Returns `0.0`
/// for a length mismatch, an empty input, or a zero-norm vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::cosine;

    #[test]
    fn identical_unit_vectors_are_one() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn normalises_non_unit_centroid() {
        // A centroid (mean of unit vectors) is not unit-length; cosine must
        // still report direction agreement = 1.0 for a co-linear query.
        assert!((cosine(&[1.0, 0.0], &[0.4, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mismatch_and_zero_are_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }
}
