//! Spherical k-means over embedding vectors (TBS concept-system Phase 2,
//! thesis §7 S2.1) — the *semantic* concept source that replaces the Phase-0
//! directory stand-ins.
//!
//! The chunk vectors are L2-normalised `nomic-embed-text` embeddings, so cosine
//! similarity is the dot product and the natural clustering is **spherical**
//! k-means: assign each point to the nearest centroid by cosine, recompute each
//! centroid as the *re-normalised* mean of its members, iterate to convergence.
//!
//! Two properties matter for the concept system:
//!   * **Deterministic.** Init is k-means++ driven by a fixed-seed xorshift RNG
//!     (no `Math.random`/wall-clock), so a re-run reproduces the same clusters —
//!     required for the build verb to be idempotent and unit-testable offline.
//!   * **Overlap-ready.** [`soft_members`] returns, per cluster, every point
//!     within a cosine margin of its *best* cluster — realising the thesis's
//!     "concepts are tags, not a partition" (many-to-many `MEMBER`).
//!
//! Pure + dependency-light (no clustering crate); unit-tested on synthetic
//! well-separated clusters.

/// Result of a [`cluster`] run.
#[derive(Clone, Debug, PartialEq)]
pub struct KmeansResult {
    /// `k` re-normalised centroids (some may be all-zero if a cluster emptied
    /// and could not be reseeded — callers should drop empty clusters).
    pub centroids: Vec<Vec<f32>>,
    /// Hard assignment: the best centroid index for each input point.
    pub assignment: Vec<usize>,
}

/// A tiny deterministic RNG (xorshift64*) — no wall-clock, no `rand` dep, so
/// clustering is reproducible from a fixed seed.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F491_4F6CDD1D)
    }
    /// A float in `[0, 1)`.
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Dot product (cosine for normalised vectors). 0 on a length mismatch.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Re-normalise `v` to unit length in place (no-op for a zero vector).
fn normalize(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    if norm_sq > 0.0 {
        let inv = 1.0 / norm_sq.sqrt();
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

/// The cluster index whose centroid is most cosine-similar to `point`.
fn nearest(point: &[f32], centroids: &[Vec<f32>]) -> usize {
    let mut best = 0usize;
    let mut best_sim = f32::MIN;
    for (i, c) in centroids.iter().enumerate() {
        let sim = dot(point, c);
        if sim > best_sim {
            best_sim = sim;
            best = i;
        }
    }
    best
}

/// k-means++ seeding (cosine-distance weighted), deterministic from `seed`.
fn kmeans_pp_init(points: &[Vec<f32>], k: usize, rng: &mut Rng) -> Vec<Vec<f32>> {
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    // First centroid: a seed-chosen point.
    let first = (rng.next_u64() as usize) % points.len();
    centroids.push(points[first].clone());
    while centroids.len() < k {
        // Weight each point by its squared cosine-distance to the nearest
        // chosen centroid (d = 1 - cos, in [0, 2]).
        let weights: Vec<f32> = points
            .iter()
            .map(|p| {
                let nearest_sim = centroids
                    .iter()
                    .map(|c| dot(p, c))
                    .fold(f32::MIN, f32::max);
                let d = (1.0 - nearest_sim).max(0.0);
                d * d
            })
            .collect();
        let total: f32 = weights.iter().sum();
        if total <= 0.0 {
            // All points coincide with chosen centroids — pad with a repeat.
            centroids.push(points[first].clone());
            continue;
        }
        let mut target = rng.next_f32() * total;
        let mut chosen = points.len() - 1;
        for (i, w) in weights.iter().enumerate() {
            target -= w;
            if target <= 0.0 {
                chosen = i;
                break;
            }
        }
        centroids.push(points[chosen].clone());
    }
    centroids
}

/// Run spherical k-means. `k` is clamped to `1..=points.len()`. Empty clusters
/// are reseeded to the point currently worst-served by its centroid, so the
/// returned `k` clusters are all non-empty when `points.len() >= k`.
pub fn cluster(points: &[Vec<f32>], k: usize, iters: usize, seed: u64) -> KmeansResult {
    let n = points.len();
    let k = k.clamp(1, n.max(1));
    if n == 0 {
        return KmeansResult {
            centroids: Vec::new(),
            assignment: Vec::new(),
        };
    }
    let mut rng = Rng::new(seed);
    let mut centroids = kmeans_pp_init(points, k, &mut rng);
    let dim = points[0].len();
    let mut assignment = vec![0usize; n];

    for _ in 0..iters.max(1) {
        // Assignment step.
        let mut changed = false;
        for (i, p) in points.iter().enumerate() {
            let a = nearest(p, &centroids);
            if a != assignment[i] {
                changed = true;
            }
            assignment[i] = a;
        }
        // Update step: re-normalised means.
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for (i, p) in points.iter().enumerate() {
            let c = assignment[i];
            counts[c] += 1;
            for (s, x) in sums[c].iter_mut().zip(p.iter()) {
                *s += x;
            }
        }
        for (c, sum) in sums.iter_mut().enumerate() {
            if counts[c] == 0 {
                continue; // reseed below
            }
            normalize(sum);
            centroids[c] = std::mem::take(sum);
        }
        // Reseed any empty cluster to the point least similar to its own
        // centroid (the worst-served point becomes a fresh seed).
        for c in 0..k {
            if counts[c] == 0 {
                if let Some((worst, _)) = points
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (i, dot(p, &centroids[assignment[i]])))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                {
                    centroids[c] = points[worst].clone();
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    KmeansResult {
        centroids,
        assignment,
    }
}

/// Soft (overlapping) membership: for each cluster, every point whose cosine to
/// that centroid is within `margin` of its *best* centroid similarity. `margin
/// = 0.0` reduces to hard assignment; a small positive margin lets a point on a
/// boundary belong to several clusters (the many-to-many `MEMBER` property).
pub fn soft_members(points: &[Vec<f32>], centroids: &[Vec<f32>], margin: f32) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); centroids.len()];
    for (i, p) in points.iter().enumerate() {
        let sims: Vec<f32> = centroids.iter().map(|c| dot(p, c)).collect();
        let best = sims.iter().copied().fold(f32::MIN, f32::max);
        for (c, sim) in sims.iter().enumerate() {
            if *sim >= best - margin {
                out[c].push(i);
            }
        }
    }
    out
}

/// A reasonable default `k` for `n` points: ≈ √n, bounded to a browsable range
/// (thesis §6: N ≈ 100–200 concepts at the top end). Overridable by the caller.
pub fn default_k(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let k = (n as f64).sqrt().round() as usize;
    k.clamp(2, 200).min(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: Vec<f32>) -> Vec<f32> {
        let mut v = v;
        normalize(&mut v);
        v
    }

    /// Three tight, well-separated groups in 3-D (one axis each).
    fn three_groups() -> Vec<Vec<f32>> {
        let mut pts = Vec::new();
        for d in 0..3 {
            for j in 0..5 {
                let mut v = vec![0.0f32; 3];
                v[d] = 1.0;
                // tiny jitter on another axis so points aren't identical.
                v[(d + 1) % 3] = 0.02 * j as f32;
                pts.push(norm(v));
            }
        }
        pts
    }

    #[test]
    fn recovers_well_separated_groups() {
        let pts = three_groups();
        let res = cluster(&pts, 3, 25, 42);
        assert_eq!(res.centroids.len(), 3);
        // Points 0..5, 5..10, 10..15 should each share a cluster label.
        for group in [0..5, 5..10, 10..15] {
            let labels: std::collections::BTreeSet<usize> =
                group.clone().map(|i| res.assignment[i]).collect();
            assert_eq!(labels.len(), 1, "group {group:?} split across clusters");
        }
        // The three groups land in three distinct clusters.
        let distinct: std::collections::BTreeSet<usize> = res.assignment.iter().copied().collect();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn deterministic_across_runs() {
        let pts = three_groups();
        let a = cluster(&pts, 3, 25, 7);
        let b = cluster(&pts, 3, 25, 7);
        assert_eq!(a, b, "same seed ⇒ identical clustering");
    }

    #[test]
    fn centroids_are_normalised() {
        let pts = three_groups();
        let res = cluster(&pts, 3, 25, 1);
        for c in &res.centroids {
            let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3, "centroid not unit-length: {norm}");
        }
    }

    #[test]
    fn soft_members_with_zero_margin_is_hard_partition() {
        let pts = three_groups();
        let res = cluster(&pts, 3, 25, 3);
        let soft = soft_members(&pts, &res.centroids, 0.0);
        let total: usize = soft.iter().map(Vec::len).sum();
        assert_eq!(total, pts.len(), "zero margin ⇒ each point in exactly one cluster");
    }

    #[test]
    fn soft_members_margin_creates_overlap() {
        // Two points equidistant between two centroids overlap under a margin.
        let centroids = vec![norm(vec![1.0, 0.0]), norm(vec![0.0, 1.0])];
        let pts = vec![norm(vec![1.0, 1.0])]; // 45° — equal to both
        let soft = soft_members(&pts, &centroids, 0.05);
        let total: usize = soft.iter().map(Vec::len).sum();
        assert_eq!(total, 2, "the boundary point belongs to both clusters");
    }

    #[test]
    fn k_is_clamped_and_empty_input_is_safe() {
        assert!(cluster(&[], 5, 10, 1).centroids.is_empty());
        let pts = three_groups();
        // k larger than n is clamped; never panics.
        let res = cluster(&pts, 1000, 5, 1);
        assert!(res.centroids.len() <= pts.len());
    }

    #[test]
    fn default_k_is_sqrt_bounded() {
        assert_eq!(default_k(0), 0);
        assert_eq!(default_k(1), 1);
        assert_eq!(default_k(100), 10);
        assert!(default_k(1_000_000) <= 200);
    }
}
