//! Compact circle packing — the shared "arrange these nodes tightly around a
//! point" step used by both deterministic backends ([`super::hierarchical`]
//! groups by module, [`super::stable_grid`] groups by service; each then packs
//! the contained nodes around the group's anchor position).
//!
//! The pack is a **phyllotaxis (sunflower) spiral**: even, non-clumping, and
//! deterministic. It mirrors C-scaffold's `scaffold_layout` spread (golden
//! angle) but is parameterised by a centre + spacing so a caller can drop a
//! cluster of nodes around any anchor.

use crate::graph::model::Position;

/// The golden angle (137.507°) in radians — the phyllotaxis constant that gives
/// an even, gap-free spread. Same constant C-scaffold uses for its spiral.
const GOLDEN_ANGLE: f32 = 2.399_963_2;

/// Place `ids` on a compact sunflower spiral centred on `(cx, cy)`, with
/// `spacing` model-px between successive ring steps. The first id lands on the
/// centre; later ids spiral outward. **Order matters** — pass ids pre-sorted
/// for a deterministic result. `z` is 0 (v1 is 2D; Plan v2 §10).
///
/// Returns owned `(id, Position)` pairs so the caller can fold them straight
/// into a `model::Layout`.
pub fn circle_pack<I, S>(cx: f32, cy: f32, spacing: f32, ids: I) -> Vec<(String, Position)>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    ids.into_iter()
        .enumerate()
        .map(|(i, id)| {
            let idx = i as f32;
            let angle = idx * GOLDEN_ANGLE;
            let radius = spacing * idx.sqrt();
            (
                id.into(),
                Position {
                    x: cx + radius * angle.cos(),
                    y: cy + radius * angle.sin(),
                    z: 0.0,
                },
            )
        })
        .collect()
}

/// The packing radius a group of `count` nodes occupies at `spacing` — the
/// outermost spiral arm. Backends use it to keep groups from overlapping.
pub fn pack_radius(count: usize, spacing: f32) -> f32 {
    if count <= 1 {
        0.0
    } else {
        spacing * ((count - 1) as f32).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_node_lands_on_centre() {
        let p = circle_pack(10.0, -5.0, 30.0, ["only"]);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, "only");
        assert!((p[0].1.x - 10.0).abs() < 1e-4 && (p[0].1.y + 5.0).abs() < 1e-4);
        assert_eq!(p[0].1.z, 0.0);
    }

    #[test]
    fn every_id_is_placed_exactly_once() {
        let ids = ["a", "b", "c", "d", "e"];
        let p = circle_pack(0.0, 0.0, 25.0, ids);
        assert_eq!(p.len(), 5);
        let names: Vec<&str> = p.iter().map(|(id, _)| id.as_str()).collect();
        for id in ids {
            assert!(names.contains(&id));
        }
    }

    #[test]
    fn pack_is_centred_and_deterministic() {
        let a = circle_pack(100.0, 200.0, 20.0, ["x", "y", "z"]);
        let b = circle_pack(100.0, 200.0, 20.0, ["x", "y", "z"]);
        assert_eq!(a, b, "same input → same pack");
        // All members fall within the packing radius of the centre.
        let r = pack_radius(3, 20.0) + 1e-3;
        for (_, p) in &a {
            let d = ((p.x - 100.0).powi(2) + (p.y - 200.0).powi(2)).sqrt();
            assert!(d <= r, "node {d} within pack radius {r}");
        }
    }

    #[test]
    fn pack_radius_grows_with_count() {
        assert_eq!(pack_radius(0, 30.0), 0.0);
        assert_eq!(pack_radius(1, 30.0), 0.0);
        assert!(pack_radius(10, 30.0) > pack_radius(3, 30.0));
    }
}
