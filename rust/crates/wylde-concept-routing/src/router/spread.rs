//! The spreading-activation engine (concept-routing R1.5b, relation-model
//! addendum §3) — the propagation that turns a *flat* cosine seed into a
//! *structured* activation by flowing it through the typed relation graph.
//!
//! ## Why
//!
//! R1 found concept centroid cosines cluster flat (~0.60–0.65): a single
//! threshold can't separate on-topic from off-topic. The answer is structure,
//! not a better scalar. Here the flat cosine is only the **seed**; dependency
//! edges propagate it transitively (recovering recall a flat seed loses) and
//! exclusion edges suppress laterally (opening the gap the raw cosine couldn't).
//!
//! ## The pipeline (addendum §3.1, exact order)
//!
//! 1. **seed-lift** — a matched vocab term lifts its `described_by` concepts
//!    (`seed_vocab_weight`).
//! 2. **dependency spread** — transitive, **bidirectional**, per-hop decayed
//!    (`dep_decay`), bounded by `spread_floor` + `max_hops`. A Dijkstra-flavoured
//!    relaxation (max-heap, `visited_best` cycle guard) so cycles + diamonds
//!    settle to the best path.
//!    * **containment spread** (definitional-hierarchy H6) — an *optional*,
//!      *separate* propagation channel sourced from the hierarchy overlay's
//!      parent/child containment edges (NOT a `RelationKind` — OQ-6). Same
//!      Dijkstra relaxation + cycle guard as the dependency step, but
//!      **asymmetric** (OQ-5): child→parent strong (`containment_up_decay`),
//!      parent→child weak (`containment_down_decay`). Slotted next to the
//!      dependency relaxation, before positive/inhibition, so the IS-NOT
//!      inhibition still has the last word over a containment-boosted node.
//! 3. **positive co-activation** — gentle, 1-hop, symmetric (`positive_decay`).
//! 4. **inhibition** — soft *multiplicative* lateral damp with a floor over
//!    negative edges (`inhibition_strength`, `inhibition_floor`): an overwhelming
//!    raw signal still surfaces.
//!
//! The caller then runs the SAME `policy::select` as R1 over the **settled**
//! activation — "tune the propagation," not the threshold.
//!
//! ## Behaviour-safe contract
//!
//! **Empty relation graph + no seed-lift links + no containment ⇒ identity**
//! (every step is a no-op, the settled activation equals the seed). With the
//! master toggle OFF the engine is never reached at all. The H6 containment
//! channel is doubly safe: the hierarchy toggle OFF ⇒ the wiring layer passes an
//! empty containment adjacency, and even ON an empty adjacency ⇒ the step is a
//! no-op — so containment can only ever *add* behaviour, never change today's.
//! R1.5b is **log-only** — the caller logs the before/after activation; nothing
//! is injected (that is R2).
//!
//! Pure + deterministic: no I/O, no embed, no service. Microseconds on a
//! few-hundred-node sparse graph (addendum §3.3).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use serde::{Deserialize, Serialize};

use crate::config::RelationParams;
use crate::relations::{NodeRef, RelationGraph, RelationKind};

/// Why a node ended up with the activation it did — the explainable provenance
/// (addendum §3.4). Surfaced in the calibration log (R1.5b) and, later, the
/// curation menu (R2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Provenance {
    /// Fired on its own seed (concept centroid cosine, or a matched vocab term).
    #[default]
    Seed,
    /// A matched vocab term lifted this concept's activation.
    SeedLift { from: NodeRef },
    /// Pulled in via a depends-on chain (`from` = the originating seed node).
    Dependency { from: NodeRef, hops: u8 },
    /// Co-activated by a relates-to (positive) edge.
    Positive { from: NodeRef },
    /// Pulled in along a hierarchy **containment** edge (parent/child) — the
    /// definitional-hierarchy H6 propagation channel, a *separate* adjacency
    /// from the typed relations (OQ-6) so `concept_relations.json`'s wire shape
    /// stays frozen. `from` is the originating seed node; `hops` the containment
    /// hop count. Only ever produced when a non-empty containment adjacency is
    /// supplied (toggle ON ⇒ wired); empty/off ⇒ never produced.
    Containment { from: NodeRef, hops: u8 },
    /// Suppressed by an exclusion (negative) edge; `raw` is the pre-inhibition
    /// activation, so the menu/log can show how far it was pushed down.
    Inhibited { by: NodeRef, raw: f32 },
}

/// The settled activation over every reached node, plus per-node provenance.
#[derive(Clone, Debug, Default)]
pub struct SpreadResult {
    pub activation: HashMap<NodeRef, f32>,
    pub provenance: HashMap<NodeRef, Provenance>,
}

impl SpreadResult {
    /// Activation for one node (`0.0` if it was never reached).
    pub fn activation_of(&self, node: &NodeRef) -> f32 {
        self.activation.get(node).copied().unwrap_or(0.0)
    }
    /// Provenance for one node (`Seed` if unrecorded).
    pub fn provenance_of(&self, node: &NodeRef) -> Provenance {
        self.provenance
            .get(node)
            .cloned()
            .unwrap_or(Provenance::Seed)
    }
}

/// Run the spreading-activation pipeline over `seed`, reshaping it with the
/// relation `graph` under `p`. `vocab_to_concepts` carries the `described_by`
/// links (vocab → the concepts it names) for the seed-lift step.
///
/// `containment` is the **optional** hierarchy containment adjacency
/// (definitional-hierarchy H6): each `(parent, child)` pair is one containment
/// edge, sourced by the wiring layer from the hierarchy overlay/projection and
/// mapped into this crate's node space. It is a *separate* propagation channel
/// from the typed relations (OQ-6); pass `&[]` to disable it. The hierarchy
/// toggle OFF ⇒ the wiring layer passes `&[]`, and an empty `containment` is the
/// step's identity, so containment can only ever add behaviour.
///
/// `seed` is the initial activation: concept nodes keyed to their (already
/// `seed_weight`-scaled) cosine, matched-vocab nodes to their match strength.
/// Returns the settled activation + provenance for every reached node.
///
/// **Identity guarantee:** when `graph` is empty *and* `vocab_to_concepts` is
/// empty *and* `containment` is empty the result's activation equals `seed` and
/// every provenance is `Seed`.
pub fn spread(
    seed: HashMap<NodeRef, f32>,
    vocab_to_concepts: &[(NodeRef, NodeRef)],
    containment: &[(NodeRef, NodeRef)],
    graph: &RelationGraph,
    p: &RelationParams,
) -> SpreadResult {
    let mut a = seed;
    let mut prov: HashMap<NodeRef, Provenance> = HashMap::new();

    // ── 1. SEED-LIFT: a matched vocab term lifts its described_by concepts ──
    for (vocab, concept) in vocab_to_concepts {
        let Some(&av) = a.get(vocab) else { continue }; // only matched vocab carry a seed
        let lifted = p.seed_vocab_weight * av;
        let cur = a.get(concept).copied().unwrap_or(0.0);
        if lifted > cur {
            a.insert(concept.clone(), lifted);
            prov.insert(
                concept.clone(),
                Provenance::SeedLift {
                    from: vocab.clone(),
                },
            );
        }
    }

    // Nothing else to do without ANY propagation source — the identity
    // fast-path. Containment is a separate channel, so it must keep the engine
    // alive even when the relation graph is empty (and vice-versa).
    if graph.is_empty() && containment.is_empty() {
        return SpreadResult {
            activation: a,
            provenance: prov,
        };
    }

    let adj = graph.adjacency();

    // ── 2. DEPENDENCY SPREAD: transitive, bidirectional, decayed ───────────
    // Dijkstra-flavoured relaxation: a max-heap of (activation, node) so the
    // strongest contribution wins each path; `visited_best` is the cycle guard
    // (dep_decay < 1 + spread_floor > 0 bound the improving updates ⇒ it
    // always terminates).
    let mut visited_best: HashMap<NodeRef, f32> = HashMap::new();
    let mut heap: BinaryHeap<Frontier> = BinaryHeap::new();
    for (node, &act) in a.iter() {
        if act >= p.spread_floor {
            visited_best.insert(node.clone(), act);
            heap.push(Frontier {
                act,
                hops: 0,
                node: node.clone(),
                origin: node.clone(),
            });
        }
    }
    while let Some(f) = heap.pop() {
        // Stale heap entry (a better activation for this node was found later).
        if f.act < *visited_best.get(&f.node).unwrap_or(&0.0) {
            continue;
        }
        // Hop cap (belt-and-braces with the floor): don't expand past max_hops.
        if f.hops >= p.max_hops {
            continue;
        }
        let Some(edges) = adj.get(&f.node) else {
            continue;
        };
        for edge in edges {
            if edge.kind != RelationKind::Dependency {
                continue;
            }
            // Bidirectional: the neighbour is whichever endpoint isn't `f.node`.
            let v = if edge.from == f.node {
                &edge.to
            } else {
                &edge.from
            };
            let contributed = f.act * p.dep_decay;
            if contributed < p.spread_floor {
                continue; // floor cutoff bounds the spread
            }
            if contributed > a.get(v).copied().unwrap_or(0.0) {
                a.insert(v.clone(), contributed);
                prov.insert(
                    v.clone(),
                    Provenance::Dependency {
                        from: f.origin.clone(),
                        hops: f.hops + 1,
                    },
                );
                visited_best.insert(v.clone(), contributed);
                heap.push(Frontier {
                    act: contributed,
                    hops: f.hops + 1,
                    node: v.clone(),
                    origin: f.origin.clone(),
                });
            }
        }
    }

    // ── 2b. CONTAINMENT SPREAD: asymmetric, decayed, cycle-safe (H6) ───────
    // A separate propagation channel from the typed relations (OQ-6): activation
    // flows along the hierarchy's parent/child containment edges, child→parent
    // STRONG (`containment_up_decay`) and parent→child WEAK
    // (`containment_down_decay`) — a leaf strongly implies its category, a
    // category only weakly implies any one child. Same Dijkstra relaxation +
    // `visited_best` cycle guard as the dependency step. Guarded by a non-empty
    // adjacency so an empty containment (toggle OFF, or no edges) is a pure
    // no-op — the identity guarantee. Runs BEFORE inhibition so a strong IS-NOT
    // still gets the last word over a containment-boosted node.
    if !containment.is_empty() {
        // Per-node neighbours as (neighbour, decay-to-apply). An edge (P, C)
        // gives C an UP neighbour P (strong) and P a DOWN neighbour C (weak).
        let mut cont_adj: HashMap<NodeRef, Vec<(NodeRef, f32)>> = HashMap::new();
        for (parent, child) in containment {
            cont_adj
                .entry(child.clone())
                .or_default()
                .push((parent.clone(), p.containment_up_decay));
            cont_adj
                .entry(parent.clone())
                .or_default()
                .push((child.clone(), p.containment_down_decay));
        }

        let mut cont_best: HashMap<NodeRef, f32> = HashMap::new();
        let mut cont_heap: BinaryHeap<Frontier> = BinaryHeap::new();
        for (node, &act) in a.iter() {
            if act >= p.spread_floor {
                cont_best.insert(node.clone(), act);
                cont_heap.push(Frontier {
                    act,
                    hops: 0,
                    node: node.clone(),
                    origin: node.clone(),
                });
            }
        }
        while let Some(f) = cont_heap.pop() {
            if f.act < *cont_best.get(&f.node).unwrap_or(&0.0) {
                continue; // stale entry
            }
            if f.hops >= p.max_hops {
                continue; // hop cap (belt-and-braces with the floor)
            }
            let Some(neighbours) = cont_adj.get(&f.node) else {
                continue;
            };
            for (v, decay) in neighbours {
                let contributed = f.act * decay;
                if contributed < p.spread_floor {
                    continue; // floor cutoff bounds the spread + cycles
                }
                if contributed > a.get(v).copied().unwrap_or(0.0) {
                    a.insert(v.clone(), contributed);
                    prov.insert(
                        v.clone(),
                        Provenance::Containment {
                            from: f.origin.clone(),
                            hops: f.hops + 1,
                        },
                    );
                    cont_best.insert(v.clone(), contributed);
                    cont_heap.push(Frontier {
                        act: contributed,
                        hops: f.hops + 1,
                        node: v.clone(),
                        origin: f.origin.clone(),
                    });
                }
            }
        }
    }

    // ── 3. POSITIVE co-activation: gentle, 1-hop, symmetric ────────────────
    // Read from a pre-positive snapshot so the pass is order-independent.
    let snapshot = a.clone();
    for edge in graph.of_kind(RelationKind::Positive) {
        let (x, y) = (&edge.from, &edge.to);
        let ax = snapshot.get(x).copied().unwrap_or(0.0);
        let ay = snapshot.get(y).copied().unwrap_or(0.0);
        let cand_y = ax * p.positive_decay;
        if cand_y > a.get(y).copied().unwrap_or(0.0) {
            a.insert(y.clone(), cand_y);
            prov.insert(y.clone(), Provenance::Positive { from: x.clone() });
        }
        let cand_x = ay * p.positive_decay;
        if cand_x > a.get(x).copied().unwrap_or(0.0) {
            a.insert(x.clone(), cand_x);
            prov.insert(x.clone(), Provenance::Positive { from: y.clone() });
        }
    }

    // ── 4. INHIBITION: soft multiplicative lateral damp with a floor ───────
    // Read pressure from a pre-inhibition snapshot (so both directions use the
    // excluder's pre-inhibition strength); apply multiplicatively; keep the
    // STRONGEST suppression when several negative edges hit one node.
    let pre = a.clone();
    for edge in graph.of_kind(RelationKind::Negative) {
        let (x, y) = (&edge.from, &edge.to);
        let px = pre.get(x).copied().unwrap_or(0.0);
        let py = pre.get(y).copied().unwrap_or(0.0);

        // y suppressed by x. `px.max(0.0)` so a negative cosine can't AMPLIFY;
        // the floor guarantees an overwhelming raw signal still surfaces.
        let factor_y = (1.0 - p.inhibition_strength * px.max(0.0)).max(p.inhibition_floor);
        let new_y = py * factor_y;
        if new_y < a.get(y).copied().unwrap_or(py) {
            a.insert(y.clone(), new_y);
            prov.insert(
                y.clone(),
                Provenance::Inhibited {
                    by: x.clone(),
                    raw: py,
                },
            );
        }
        // x suppressed by y (symmetric).
        let factor_x = (1.0 - p.inhibition_strength * py.max(0.0)).max(p.inhibition_floor);
        let new_x = px * factor_x;
        if new_x < a.get(x).copied().unwrap_or(px) {
            a.insert(x.clone(), new_x);
            prov.insert(
                x.clone(),
                Provenance::Inhibited {
                    by: y.clone(),
                    raw: px,
                },
            );
        }
    }

    SpreadResult {
        activation: a,
        provenance: prov,
    }
}

/// A heap entry for the dependency relaxation. Ordered by activation (max-heap),
/// with a stable node-label tiebreak so the traversal is deterministic.
struct Frontier {
    act: f32,
    hops: u8,
    node: NodeRef,
    origin: NodeRef,
}

impl PartialEq for Frontier {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Frontier {}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap on activation; deterministic tiebreak on node label so equal
        // activations pop in a stable order. (Reverse the label compare so the
        // smaller label is the *greater* heap element — purely for stability.)
        self.act
            .total_cmp(&other.act)
            .then_with(|| other.node.label().cmp(&self.node.label()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relations::Relation;

    fn params() -> RelationParams {
        RelationParams::default()
    }

    fn seed(pairs: &[(NodeRef, f32)]) -> HashMap<NodeRef, f32> {
        pairs.iter().cloned().collect()
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn empty_graph_no_lift_is_identity() {
        let s = seed(&[(NodeRef::concept("a"), 0.62), (NodeRef::concept("b"), 0.58)]);
        let out = spread(s.clone(), &[], &[], &RelationGraph::empty(), &params());
        assert_eq!(out.activation, s, "empty graph + no lift ⇒ settled == seed");
        assert!(out.provenance.is_empty(), "no reshaping ⇒ no provenance");
    }

    #[test]
    fn vocab_seed_lift_raises_described_concept() {
        // {{nextcloud}} matched (seed 1.0) and it describes the Nextcloud
        // concept, whose cosine is a flat 0.40. seed_vocab_weight 0.7 lifts it.
        let nc_concept = NodeRef::concept("nextcloud");
        let nc_vocab = NodeRef::vocab("nextcloud");
        let s = seed(&[(nc_concept.clone(), 0.40), (nc_vocab.clone(), 1.0)]);
        let links = vec![(nc_vocab.clone(), nc_concept.clone())];
        let out = spread(s, &links, &[], &RelationGraph::empty(), &params());
        assert!(approx(out.activation_of(&nc_concept), 0.7), "lifted to 0.7");
        assert!(matches!(
            out.provenance_of(&nc_concept),
            Provenance::SeedLift { .. }
        ));
    }

    #[test]
    fn dependency_spread_pulls_in_a_flat_dependency() {
        // Nextcloud fires (0.64); DDNS's own cosine is a flat 0.30 (below the
        // 0.50 floor R4 would use). The depends-on edge pulls DDNS up to
        // 0.64*0.5 = 0.32 at hop 1 — and the proof is the provenance.
        let nc = NodeRef::vocab("nextcloud");
        let ddns = NodeRef::vocab("ddns");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                nc.clone(),
                ddns.clone(),
                RelationKind::Dependency,
                None,
            )],
        };
        let s = seed(&[(nc.clone(), 0.64), (ddns.clone(), 0.30)]);
        let out = spread(s, &[], &[], &g, &params());
        assert!(
            approx(out.activation_of(&ddns), 0.32),
            "0.64*dep_decay(0.5)"
        );
        match out.provenance_of(&ddns) {
            Provenance::Dependency { from, hops } => {
                assert_eq!(from, nc);
                assert_eq!(hops, 1);
            }
            o => panic!("expected Dependency provenance, got {o:?}"),
        }
    }

    #[test]
    fn dependency_spread_is_bidirectional() {
        // Edge authored A → B (A depends-on B). Seeding B must still pull in A
        // (the backward "blast radius" direction).
        let a = NodeRef::concept("a");
        let b = NodeRef::concept("b");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                a.clone(),
                b.clone(),
                RelationKind::Dependency,
                None,
            )],
        };
        let out = spread(seed(&[(b.clone(), 0.8)]), &[], &[], &g, &params());
        assert!(
            approx(out.activation_of(&a), 0.4),
            "backward spread 0.8*0.5"
        );
    }

    #[test]
    fn dependency_decays_per_hop_and_respects_floor() {
        // Chain a→b→c→d. seed a=1.0, dep_decay=0.5 ⇒ 1, .5, .25, .125 — but the
        // 0.05 floor still admits all; check the per-hop decay numbers.
        let nodes: Vec<NodeRef> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| NodeRef::concept(*n))
            .collect();
        let rels = vec![
            Relation::normalized(
                nodes[0].clone(),
                nodes[1].clone(),
                RelationKind::Dependency,
                None,
            ),
            Relation::normalized(
                nodes[1].clone(),
                nodes[2].clone(),
                RelationKind::Dependency,
                None,
            ),
            Relation::normalized(
                nodes[2].clone(),
                nodes[3].clone(),
                RelationKind::Dependency,
                None,
            ),
        ];
        let g = RelationGraph { relations: rels };
        let out = spread(seed(&[(nodes[0].clone(), 1.0)]), &[], &[], &g, &params());
        assert!(approx(out.activation_of(&nodes[1]), 0.5));
        assert!(approx(out.activation_of(&nodes[2]), 0.25));
        assert!(approx(out.activation_of(&nodes[3]), 0.125));
    }

    #[test]
    fn max_hops_caps_the_spread() {
        // Same chain but max_hops=2 ⇒ d (hop 3) is never reached.
        let nodes: Vec<NodeRef> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| NodeRef::concept(*n))
            .collect();
        let rels = vec![
            Relation::normalized(
                nodes[0].clone(),
                nodes[1].clone(),
                RelationKind::Dependency,
                None,
            ),
            Relation::normalized(
                nodes[1].clone(),
                nodes[2].clone(),
                RelationKind::Dependency,
                None,
            ),
            Relation::normalized(
                nodes[2].clone(),
                nodes[3].clone(),
                RelationKind::Dependency,
                None,
            ),
        ];
        let g = RelationGraph { relations: rels };
        let p = RelationParams {
            max_hops: 2,
            ..params()
        };
        let out = spread(seed(&[(nodes[0].clone(), 1.0)]), &[], &[], &g, &p);
        assert!(approx(out.activation_of(&nodes[2]), 0.25), "hop 2 reached");
        assert!(
            approx(out.activation_of(&nodes[3]), 0.0),
            "hop 3 capped out"
        );
    }

    #[test]
    fn spread_floor_bounds_low_contributions() {
        // seed 0.08, dep_decay 0.5 ⇒ neighbour would be 0.04 < floor 0.05 ⇒ not
        // pulled in.
        let a = NodeRef::concept("a");
        let b = NodeRef::concept("b");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                a.clone(),
                b.clone(),
                RelationKind::Dependency,
                None,
            )],
        };
        let out = spread(seed(&[(a.clone(), 0.08)]), &[], &[], &g, &params());
        assert!(
            approx(out.activation_of(&b), 0.0),
            "below spread_floor ⇒ no spread"
        );
    }

    #[test]
    fn dependency_cycle_terminates_and_settles() {
        // a→b, b→c, c→a (a cycle). Must terminate (visited_best guard) and
        // settle to the best decayed path.
        let a = NodeRef::concept("a");
        let b = NodeRef::concept("b");
        let c = NodeRef::concept("c");
        let g = RelationGraph {
            relations: vec![
                Relation::normalized(a.clone(), b.clone(), RelationKind::Dependency, None),
                Relation::normalized(b.clone(), c.clone(), RelationKind::Dependency, None),
                Relation::normalized(c.clone(), a.clone(), RelationKind::Dependency, None),
            ],
        };
        let out = spread(seed(&[(a.clone(), 1.0)]), &[], &[], &g, &params());
        // a stays 1.0 (its own seed beats any decayed return trip); b,c decayed.
        assert!(approx(out.activation_of(&a), 1.0));
        assert!(approx(out.activation_of(&b), 0.5));
        assert!(
            approx(out.activation_of(&c), 0.5),
            "min path to c is 1 hop via c→a"
        );
    }

    #[test]
    fn positive_edge_co_activates_symmetrically() {
        let x = NodeRef::concept("x");
        let y = NodeRef::concept("y");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                x.clone(),
                y.clone(),
                RelationKind::Positive,
                None,
            )],
        };
        // x strong, y absent ⇒ y gets x*positive_decay (0.8*0.3=0.24).
        let out = spread(seed(&[(x.clone(), 0.8)]), &[], &[], &g, &params());
        assert!(approx(out.activation_of(&y), 0.24));
        assert!(matches!(out.provenance_of(&y), Provenance::Positive { .. }));
    }

    #[test]
    fn negative_edge_softly_inhibits_with_floor() {
        // Nextcloud fires 0.64, Wylde sits 0.62 next to it. A Nextcloud ⊘ Wylde
        // edge damps Wylde: 0.62 * (1 - 0.8*0.64) = 0.62 * 0.488 = 0.3026.
        let nc = NodeRef::concept("nextcloud");
        let wylde = NodeRef::concept("wylde");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                nc.clone(),
                wylde.clone(),
                RelationKind::Negative,
                None,
            )],
        };
        let out = spread(
            seed(&[(nc.clone(), 0.64), (wylde.clone(), 0.62)]),
            &[],
            &[],
            &g,
            &params(),
        );
        let w = out.activation_of(&wylde);
        assert!(
            approx(w, 0.62 * (1.0 - 0.8 * 0.64)),
            "soft multiplicative damp, got {w}"
        );
        assert!(w < 0.62, "Wylde suppressed below its raw cosine");
        match out.provenance_of(&wylde) {
            Provenance::Inhibited { by, raw } => {
                assert_eq!(by, nc);
                assert!(approx(raw, 0.62));
            }
            o => panic!("expected Inhibited, got {o:?}"),
        }
    }

    #[test]
    fn inhibition_floor_lets_overwhelming_signal_survive() {
        // An overwhelming excluder (1.0) with strength 0.8 would damp to 0.2×,
        // but the floor 0.15 is the *minimum* factor; with strength high enough
        // the floor bites and the target keeps floor× its raw value.
        let x = NodeRef::concept("x");
        let y = NodeRef::concept("y");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                x.clone(),
                y.clone(),
                RelationKind::Negative,
                None,
            )],
        };
        // strength 1.5, pressure 1.0 ⇒ 1 - 1.5 = -0.5 → floored to 0.15.
        let p = RelationParams {
            inhibition_strength: 1.5,
            ..params()
        };
        let out = spread(
            seed(&[(x.clone(), 1.0), (y.clone(), 0.9)]),
            &[],
            &[],
            &g,
            &p,
        );
        assert!(
            approx(out.activation_of(&y), 0.9 * 0.15),
            "floor caps the damp"
        );
        assert!(
            out.activation_of(&y) > 0.0,
            "never zeroed — can still surface"
        );
    }

    #[test]
    fn negative_cosine_does_not_amplify() {
        // A negative-cosine excluder must not turn inhibition into amplification.
        let x = NodeRef::concept("x");
        let y = NodeRef::concept("y");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                x.clone(),
                y.clone(),
                RelationKind::Negative,
                None,
            )],
        };
        let out = spread(
            seed(&[(x.clone(), -0.3), (y.clone(), 0.5)]),
            &[],
            &[],
            &g,
            &params(),
        );
        // pressure clamped to 0 ⇒ factor 1.0 ⇒ y unchanged (no amplification).
        assert!(approx(out.activation_of(&y), 0.5));
    }

    #[test]
    fn full_pipeline_dependency_pulls_and_exclusion_suppresses() {
        // The canonical proof in miniature: Nextcloud (0.64) depends-on DDNS
        // (flat 0.30) and is-not Wylde (0.62). After spread: DDNS pulled UP,
        // Wylde pushed DOWN — the gap the flat cosine couldn't make.
        let nc = NodeRef::concept("nextcloud");
        let ddns = NodeRef::concept("ddns");
        let wylde = NodeRef::concept("wylde");
        let g = RelationGraph {
            relations: vec![
                Relation::normalized(nc.clone(), ddns.clone(), RelationKind::Dependency, None),
                Relation::normalized(nc.clone(), wylde.clone(), RelationKind::Negative, None),
            ],
        };
        let s = seed(&[
            (nc.clone(), 0.64),
            (ddns.clone(), 0.30),
            (wylde.clone(), 0.62),
        ]);
        let before = s.clone();
        let out = spread(s, &[], &[], &g, &params());
        assert!(
            out.activation_of(&ddns) > before[&ddns],
            "dependency pulled DDNS up"
        );
        assert!(
            out.activation_of(&wylde) < before[&wylde],
            "exclusion pushed Wylde down"
        );
        // And the ordering flips usefully: DDNS now outranks the excluded Wylde.
        assert!(out.activation_of(&ddns) > out.activation_of(&wylde));
    }

    // ── H6: containment spread (the new, gated, separate channel) ──────────

    #[test]
    fn containment_empty_is_identity_even_with_relations() {
        // An empty containment adjacency must not perturb the dependency result
        // (the canonical dep proof) AND must never stamp a Containment
        // provenance — the spread-level identity-when-empty guarantee, proven
        // with a LIVE relation graph (not just an empty one).
        let nc = NodeRef::vocab("nextcloud");
        let ddns = NodeRef::vocab("ddns");
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                nc.clone(),
                ddns.clone(),
                RelationKind::Dependency,
                None,
            )],
        };
        let s = seed(&[(nc.clone(), 0.64), (ddns.clone(), 0.30)]);
        let out = spread(s, &[], &[], &g, &params());
        assert!(
            approx(out.activation_of(&ddns), 0.32),
            "dep result unchanged"
        );
        assert!(
            out.provenance
                .values()
                .all(|p| !matches!(p, Provenance::Containment { .. })),
            "empty containment ⇒ no Containment provenance anywhere"
        );
    }

    #[test]
    fn containment_propagates_up_strong_and_down_weak() {
        // One containment edge: parent P contains child C.
        let p_node = NodeRef::concept("parent");
        let c_node = NodeRef::concept("child");
        let cont = vec![(p_node.clone(), c_node.clone())];

        // UP (child → parent) is STRONG (containment_up_decay 0.5): seeding the
        // child lifts the parent to 0.8 * 0.5 = 0.4.
        let up = spread(
            seed(&[(c_node.clone(), 0.8)]),
            &[],
            &cont,
            &RelationGraph::empty(),
            &params(),
        );
        assert!(
            approx(up.activation_of(&p_node), 0.4),
            "child→parent strong"
        );
        match up.provenance_of(&p_node) {
            Provenance::Containment { from, hops } => {
                assert_eq!(from, c_node);
                assert_eq!(hops, 1);
            }
            o => panic!("expected Containment provenance, got {o:?}"),
        }

        // DOWN (parent → child) is WEAK (containment_down_decay 0.15): seeding the
        // parent lifts the child only to 0.8 * 0.15 = 0.12.
        let down = spread(
            seed(&[(p_node.clone(), 0.8)]),
            &[],
            &cont,
            &RelationGraph::empty(),
            &params(),
        );
        assert!(
            approx(down.activation_of(&c_node), 0.12),
            "parent→child weak"
        );
        assert!(
            down.activation_of(&c_node) < up.activation_of(&p_node),
            "asymmetry: up-strong beats down-weak (OQ-5)"
        );
    }

    #[test]
    fn containment_runs_with_an_empty_relation_graph() {
        // The early-return must NOT short-circuit when the relation graph is
        // empty but containment is present — containment is its own channel.
        let p_node = NodeRef::concept("p");
        let c_node = NodeRef::concept("c");
        let cont = vec![(p_node.clone(), c_node.clone())];
        let out = spread(
            seed(&[(c_node.clone(), 1.0)]),
            &[],
            &cont,
            &RelationGraph::empty(), // empty relations
            &params(),
        );
        assert!(
            approx(out.activation_of(&p_node), 0.5),
            "containment still fired"
        );
    }

    #[test]
    fn containment_decays_per_hop_up_and_respects_floor_and_cap() {
        // Chain T contains M contains L (so up-edges L→M→T). Seed leaf L = 1.0;
        // up-decay 0.5 ⇒ M = 0.5, T = 0.25.
        let t = NodeRef::concept("top");
        let m = NodeRef::concept("mid");
        let l = NodeRef::concept("leaf");
        let cont = vec![(t.clone(), m.clone()), (m.clone(), l.clone())];
        let out = spread(
            seed(&[(l.clone(), 1.0)]),
            &[],
            &cont,
            &RelationGraph::empty(),
            &params(),
        );
        assert!(approx(out.activation_of(&m), 0.5), "1 hop up");
        assert!(approx(out.activation_of(&t), 0.25), "2 hops up");

        // max_hops caps the climb: with max_hops = 1, T (2 hops) is unreached.
        let p = RelationParams {
            max_hops: 1,
            ..params()
        };
        let capped = spread(
            seed(&[(l.clone(), 1.0)]),
            &[],
            &cont,
            &RelationGraph::empty(),
            &p,
        );
        assert!(approx(capped.activation_of(&m), 0.5), "hop 1 reached");
        assert!(approx(capped.activation_of(&t), 0.0), "hop 2 capped out");
    }

    #[test]
    fn containment_cycle_terminates_and_settles() {
        // A containment cycle A⊃B⊃C⊃A — must terminate (visited_best guard), not
        // spin, and the seed node keeps its own activation.
        let a = NodeRef::concept("a");
        let b = NodeRef::concept("b");
        let c = NodeRef::concept("c");
        let cont = vec![
            (a.clone(), b.clone()),
            (b.clone(), c.clone()),
            (c.clone(), a.clone()),
        ];
        let out = spread(
            seed(&[(a.clone(), 1.0)]),
            &[],
            &cont,
            &RelationGraph::empty(),
            &params(),
        );
        assert!(
            approx(out.activation_of(&a), 1.0),
            "seed keeps its own activation"
        );
        // b is a's child (down-weak 0.15) but also reachable up from c; it
        // settles to its best finite value and the walk halts.
        assert!(out.activation_of(&b) > 0.0 && out.activation_of(&b) <= 1.0);
    }

    #[test]
    fn containment_does_not_override_a_strong_is_not() {
        // Containment lifts Y, but a strong IS-NOT (Negative) edge still gets the
        // LAST word — inhibition runs after the containment step.
        let excluder = NodeRef::concept("excluder");
        let y = NodeRef::concept("y");
        let leaf = NodeRef::concept("leaf");
        // Containment: Y contains leaf. Seeding the leaf lifts Y up-strong to
        // 1.0 * 0.5 = 0.5.
        let cont = vec![(y.clone(), leaf.clone())];
        // Relation graph: excluder IS-NOT Y.
        let g = RelationGraph {
            relations: vec![Relation::normalized(
                excluder.clone(),
                y.clone(),
                RelationKind::Negative,
                None,
            )],
        };
        let out = spread(
            seed(&[(excluder.clone(), 1.0), (leaf.clone(), 1.0)]),
            &[],
            &cont,
            &g,
            &params(),
        );
        // Y was lifted to 0.5 by containment, then damped by the excluder:
        // 0.5 * (1 - 0.8*1.0).max(0.15) = 0.5 * 0.2 = 0.10.
        assert!(
            approx(out.activation_of(&y), 0.5 * 0.2),
            "strong IS-NOT suppresses the containment-boosted node, got {}",
            out.activation_of(&y)
        );
        assert!(
            matches!(out.provenance_of(&y), Provenance::Inhibited { .. }),
            "inhibition has the last word over containment"
        );
    }
}
