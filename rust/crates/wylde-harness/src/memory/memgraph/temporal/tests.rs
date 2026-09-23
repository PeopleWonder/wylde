use super::*;

/// Snapshot+restore guard for the toggle env so tests don't leak
/// into siblings. Mutual exclusion via the shared env lock.
struct ToggleGuard {
    _g: std::sync::MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl ToggleGuard {
    fn set(value: Option<&str>) -> Self {
        let g = crate::memory::common::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var(TOGGLE_ENV).ok();
        match value {
            Some(v) => std::env::set_var(TOGGLE_ENV, v),
            None => std::env::remove_var(TOGGLE_ENV),
        }
        Self { _g: g, prev }
    }
}

impl Drop for ToggleGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(TOGGLE_ENV, v),
            None => std::env::remove_var(TOGGLE_ENV),
        }
    }
}

// ── Toggle: default OFF, byte-identical invariant ──────────────────

#[test]
fn toggle_defaults_off_when_unset() {
    let _g = ToggleGuard::set(None);
    assert!(!temporal_memory_enabled());
}

#[test]
fn toggle_on_for_truthy_values() {
    for v in ["1", "true", "TRUE", "on", "Yes", " on "] {
        let _g = ToggleGuard::set(Some(v));
        assert!(temporal_memory_enabled(), "{v:?} should enable");
    }
}

#[test]
fn toggle_off_for_falsey_and_garbage() {
    for v in ["0", "false", "off", "no", "", "maybe", "2"] {
        let _g = ToggleGuard::set(Some(v));
        assert!(!temporal_memory_enabled(), "{v:?} should stay off");
    }
}

// ── Cypher builder pins ────────────────────────────────────────────

#[test]
fn supersede_closes_valid_to_only() {
    let q = temporal_supersede("CALLS");
    assert!(q.contains("[r:CALLS]"));
    assert!(q.contains("r.valid_to = $open"));
    assert!(q.contains("SET   r.valid_to = $at"));
    // P0 must NOT close tx_to on supersession (see module docs).
    assert!(!q.contains("tx_to ="), "P0 supersede must not touch tx_to");
}

#[test]
fn insert_opens_both_axes() {
    let q = temporal_insert("IMPORTS");
    assert!(q.contains("CREATE (a)-[r:IMPORTS"));
    for needle in [
        "valid_from: $at",
        "valid_to: $open",
        "tx_from: $at",
        "tx_to: $open",
    ] {
        assert!(q.contains(needle), "insert missing {needle}");
    }
    // MERGE endpoints, CREATE edge — history must accumulate.
    assert!(q.contains("MERGE (a:Entity"));
    assert!(q.contains("MERGE (b:Entity"));
    assert!(
        !q.contains("MERGE (a)-["),
        "edge must be CREATE'd, not MERGE'd"
    );
}

#[test]
fn weighted_insert_carries_weight() {
    let q = temporal_insert_weighted("MENTIONS");
    assert!(q.contains("weight: $weight"));
    assert!(q.contains("valid_to: $open"));
}

#[test]
fn relate_guarded_creates_only_when_no_open_edge() {
    let q = temporal_relate_guarded("CALLS");
    assert!(q.contains("OPTIONAL MATCH (a)-[ex:CALLS]->(b)"));
    assert!(q.contains("ex.valid_to = $open"));
    assert!(
        q.contains("WHERE ex IS NULL"),
        "no-op guard for weightless edge"
    );
    assert!(q.contains("CREATE (a)-[:CALLS"));
    // Weightless edge carries no weight property.
    assert!(!q.contains("weight"));
}

#[test]
fn upsert_edge_supersedes_and_carries_cumulative_weight() {
    let q = temporal_upsert_edge("MENTIONS");
    assert!(q.contains("OPTIONAL MATCH (a)-[old:MENTIONS]->(b)"));
    assert!(
        q.contains("SET old.valid_to = $at"),
        "supersede prior weighted edge"
    );
    assert!(q.contains("coalesce(old.weight, 0) + $weight_delta AS new_weight"));
    assert!(q.contains("weight: new_weight"));
}

#[test]
fn as_of_uses_half_open_valid_window_under_current_belief() {
    let q = as_of_match("CONFIGURES");
    assert!(q.contains("[r:CONFIGURES]"));
    assert!(q.contains("r.valid_from <= $t"));
    assert!(q.contains("$t < r.valid_to"));
    assert!(q.contains("r.tx_to = $open"), "as-of reads current belief");
}

#[test]
fn backfill_guards_on_null_valid_from_and_opens_intervals() {
    let q = backfill_temporal("EXPOSES");
    assert!(q.contains("WHERE r.valid_from IS NULL"), "idempotent guard");
    assert!(q.contains("coalesce(r.created_at, $epoch_default)"));
    assert!(q.contains("r.valid_to   = $open"));
    assert!(q.contains("r.tx_to      = $open"));
}

// ── Reference model: insert / supersede / as-of / no-op ────────────

#[test]
fn insert_creates_one_open_edge() {
    let mut log = TemporalEdgeLog::new();
    assert!(log.write("a", "b", "CALLS", 100, None));
    assert_eq!(log.len(), 1);
    let e = &log.all()[0];
    assert_eq!((e.valid_from, e.valid_to), (100, OPEN));
    assert_eq!((e.tx_from, e.tx_to), (100, OPEN));
    assert!(e.is_open());
}

#[test]
fn supersede_bounds_prior_edge_and_preserves_history() {
    let mut log = TemporalEdgeLog::new();
    log.write("a", "b", "INHERITS", 100, None);
    // A *changed* fact (different weight) supersedes at t=200.
    assert!(log.write("a", "b", "INHERITS", 200, Some(1.0)));
    assert_eq!(log.len(), 2, "history grows, not collapses");

    let prior = &log.all()[0];
    assert_eq!(prior.valid_from, 100);
    assert_eq!(prior.valid_to, 200, "prior bounded at supersede instant");
    assert_eq!(
        prior.tx_to, OPEN,
        "tx_to stays open in P0 (still current belief)"
    );

    let fresh = &log.all()[1];
    assert_eq!((fresh.valid_from, fresh.valid_to), (200, OPEN));
    assert!(fresh.is_open());
}

#[test]
fn no_op_guard_skips_identical_open_edge() {
    let mut log = TemporalEdgeLog::new();
    assert!(log.write("a", "b", "CALLS", 100, None));
    // Re-writing the identical fact must not churn history.
    assert!(!log.write("a", "b", "CALLS", 150, None));
    assert!(!log.write("a", "b", "CALLS", 999, None));
    assert_eq!(log.len(), 1);
    assert_eq!(log.all()[0].valid_to, OPEN, "untouched");
}

#[test]
fn weight_change_supersedes_but_same_weight_is_noop() {
    let mut log = TemporalEdgeLog::new();
    assert!(log.write("s", "t", "CALLS", 10, Some(1.0)));
    assert!(
        !log.write("s", "t", "CALLS", 20, Some(1.0)),
        "same weight = no-op"
    );
    assert!(
        log.write("s", "t", "CALLS", 30, Some(2.5)),
        "new weight supersedes"
    );
    assert_eq!(log.len(), 2);
    assert_eq!(log.all()[0].valid_to, 30);
    assert_eq!(log.all()[1].weight, Some(2.5));
}

#[test]
fn as_of_returns_the_edge_live_at_t() {
    let mut log = TemporalEdgeLog::new();
    log.write("a", "b", "CALLS", 100, Some(1.0)); // valid [100, 200)
    log.write("a", "b", "CALLS", 200, Some(2.0)); // valid [200, OPEN)

    // Before anything existed.
    assert!(log.as_of(50).is_empty());
    // Inside the first window → first version (weight 1.0).
    let at150 = log.as_of(150);
    assert_eq!(at150.len(), 1);
    assert_eq!(at150[0].weight, Some(1.0));
    // At the supersede boundary (half-open) → the new version.
    let at200 = log.as_of(200);
    assert_eq!(at200.len(), 1);
    assert_eq!(at200[0].weight, Some(2.0));
    // Far future → still the open edge.
    assert_eq!(log.as_of(10_000)[0].weight, Some(2.0));
}

#[test]
fn as_of_excludes_retracted_facts_after_retraction() {
    let mut log = TemporalEdgeLog::new();
    log.write("a", "b", "CALLS", 100, None);
    assert!(log.retract("a", "b", "CALLS", 300));
    // Still visible in its historical window...
    assert_eq!(log.as_of(150).len(), 1);
    // ...but gone at/after the retraction instant (half-open).
    assert!(log.as_of(300).is_empty());
    assert!(log.as_of(500).is_empty());
    // History is preserved, not deleted.
    assert_eq!(log.len(), 1);
    assert_eq!(log.all()[0].valid_to, 300);
}

#[test]
fn retract_with_no_open_edge_is_false() {
    let mut log = TemporalEdgeLog::new();
    assert!(!log.retract("a", "b", "CALLS", 100));
    assert!(log.is_empty());
}

#[test]
fn as_of_isolates_distinct_triples() {
    let mut log = TemporalEdgeLog::new();
    log.write("a", "b", "CALLS", 100, None);
    log.write("a", "b", "IMPORTS", 100, None); // different rel_type
    log.write("c", "d", "CALLS", 100, None); // different endpoints
    assert_eq!(log.as_of(150).len(), 3);
    // Superseding one triple doesn't disturb the others.
    log.retract("a", "b", "CALLS", 120);
    assert_eq!(log.as_of(150).len(), 2);
}

#[test]
fn now_ms_is_positive_and_monotonic_enough() {
    let a = now_ms();
    let b = now_ms();
    assert!(a > 0);
    assert!(b >= a);
}

// ── P1: unrelate / correct / as-believed-at / index / traverse ─────

#[test]
fn retract_cypher_closes_valid_to_only_and_inserts_nothing() {
    let q = temporal_retract("CALLS");
    assert!(q.contains("MATCH (a:Entity {name: $source})-[r:CALLS]->"));
    assert!(q.contains("r.valid_to = $open AND r.tx_to = $open"));
    assert!(q.contains("SET   r.valid_to = $at"));
    // Logical delete: NO replacement edge, and tx_to untouched so the
    // retracted fact stays current belief for as-of of its window.
    assert!(
        !q.contains("CREATE"),
        "unrelate must not insert a replacement"
    );
    assert!(!q.contains("DELETE"), "unrelate must not hard-delete");
    // `r.tx_to = $open` appears in the WHERE guard, but tx_to must
    // never be *set* to a timestamp (that would drop it from current
    // belief and break as-of of its window — §3.2).
    assert!(!q.contains("SET   r.tx_to"), "retract must not set tx_to");
    assert!(!q.contains("tx_to = $at"), "retract must not close tx_to");
}

#[test]
fn correct_cypher_closes_tx_to_and_recreates_with_same_valid_from() {
    let q = temporal_correct("IMPORTS");
    assert!(q.contains("WHERE old.valid_to = $open AND old.tx_to = $open"));
    assert!(
        q.contains("old.valid_from AS vf"),
        "carry the valid window forward"
    );
    assert!(q.contains("SET old.tx_to = $tt"), "correction closes tx_to");
    assert!(
        q.contains("CREATE (a)-[:IMPORTS {valid_from: vf"),
        "reuse valid_from"
    );
    assert!(q.contains("tx_from: $tt"));
    assert!(q.contains("weight: $weight"));
}

#[test]
fn as_believed_at_uses_both_half_open_axes() {
    let q = as_believed_at_match("CONFIGURES");
    assert!(q.contains("[r:CONFIGURES]"));
    assert!(q.contains("r.valid_from <= $t"));
    assert!(q.contains("$t  < r.valid_to"));
    assert!(q.contains("r.tx_from   <= $tt"));
    assert!(
        q.contains("$tt < r.tx_to"),
        "explicit transaction-time slice"
    );
    // Unlike as_of_match it does NOT hard-code tx_to = OPEN.
    assert!(!q.contains("r.tx_to = $open"));
}

#[test]
fn temporal_index_is_idempotent_relationship_range_index() {
    let q = temporal_index("CALLS", "valid_from");
    assert_eq!(
        q,
        "CREATE INDEX rel_calls_valid_from IF NOT EXISTS FOR ()-[r:CALLS]-() ON (r.valid_from)"
    );
    assert!(temporal_index("EXPOSES", "tx_from").contains("IF NOT EXISTS"));
}

#[test]
fn traverse_bucket_as_of_time_filters_typed_edges_only() {
    let q = traverse_bucket_as_of(REL_ALT_CALLS_FOR_TEST, 2, true);
    // Temporal predicate over the typed segment only.
    assert!(q.contains("all(rel IN relationships(tp) WHERE"));
    assert!(q.contains("rel.valid_from <= $t AND $t < rel.valid_to AND rel.tx_to = $open"));
    // MENTIONED_IN matched in a SEPARATE clause (not time-filtered).
    assert!(q.contains("MATCH  (e)-[:MENTIONED_IN]->(c:Chunk)"));
    // Workspace filter still threads through.
    assert!(q.contains("c.workspace = $ws"));
    // typed depth is length(tp) directly (no -1), since tp excludes MENTIONED_IN.
    assert!(q.contains("length(tp) AS typed_depth"));
}

const REL_ALT_CALLS_FOR_TEST: &str = "CALLS|IMPORTS|INHERITS";

#[test]
fn correct_retires_belief_and_appends_corrected_edge() {
    let mut log = TemporalEdgeLog::new();
    log.write("s", "t", "CALLS", 10, Some(1.0)); // valid [10,OPEN) tx [10,OPEN)
                                                 // At tx=50 we realise the weight was wrong; correct to 9.0.
    assert!(log.correct("s", "t", "CALLS", 50, Some(9.0)));
    assert_eq!(log.len(), 2);

    let wrong = &log.all()[0];
    assert_eq!(wrong.tx_from, 10);
    assert_eq!(wrong.tx_to, 50, "belief in the wrong row retired at tx=50");
    assert_eq!(wrong.valid_to, OPEN, "valid-time untouched by a correction");

    let fixed = &log.all()[1];
    assert_eq!(fixed.valid_from, 10, "same real-world valid window");
    assert_eq!((fixed.tx_from, fixed.tx_to), (50, OPEN));
    assert_eq!(fixed.weight, Some(9.0));
}

#[test]
fn correct_with_no_open_edge_is_false() {
    let mut log = TemporalEdgeLog::new();
    assert!(!log.correct("s", "t", "CALLS", 50, Some(1.0)));
    assert!(log.is_empty());
}

#[test]
fn as_believed_at_reconstructs_pre_correction_belief() {
    let mut log = TemporalEdgeLog::new();
    log.write("s", "t", "CALLS", 10, Some(1.0)); // believed from tx=10
    log.correct("s", "t", "CALLS", 50, Some(9.0)); // corrected at tx=50

    // Current belief (as-of now) sees the corrected weight.
    let now = log.as_of(100);
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].weight, Some(9.0));

    // "As believed at tx=40" (before the correction) sees the original.
    let before = log.as_of_believed(100, 40);
    assert_eq!(before.len(), 1, "exactly the pre-correction row");
    assert_eq!(before[0].weight, Some(1.0));

    // "As believed at tx=60" (after) sees the corrected row.
    let after = log.as_of_believed(100, 60);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].weight, Some(9.0));
}

#[test]
fn retract_keeps_fact_current_belief_for_as_of_of_its_window() {
    // Guards the §3.2 reasoning for unrelate: closing valid_to (not
    // tx_to) means as_of INSIDE the window still sees the fact.
    let mut log = TemporalEdgeLog::new();
    log.write("a", "b", "CALLS", 100, None);
    assert!(log.retract("a", "b", "CALLS", 300));
    assert_eq!(log.all()[0].tx_to, OPEN, "retract leaves tx_to open");
    assert_eq!(log.as_of(200).len(), 1, "still visible mid-window");
    assert_eq!(
        log.as_of_believed(200, now_ms()).len(),
        1,
        "and under any current belief"
    );
}
