//! `rag_prune` — conditional delete with filters.
//!
//! Rust port of the prune half of `Core/harness/memory/vector_store.py`,
//! restricted to the surface the `rag_prune` tool exposes. Three
//! optional filters; at least one must be provided.
//!
//! * `before_ts` — delete records older than this epoch-seconds value.
//! * `memory_type` — delete only records in this tier.
//! * `score_lt` — delete only records whose score is strictly less than
//!   this float.
//!
//! Returns the count + ids deleted; bubbles a guard error when no
//! filter is supplied so the model can't accidentally wipe the store.

use serde_json::{json, Value};

use crate::memory::rag::store::TieredStore;

/// Filters accepted by [`prune_rows`]. All three are optional, but at
/// least one must be `Some` — see [`PruneError::NoFilter`].
#[derive(Debug, Clone, Default)]
pub struct PruneFilters {
    pub before_ts: Option<f64>,
    pub memory_type: Option<String>,
    pub score_lt: Option<f32>,
}

impl PruneFilters {
    pub fn is_empty(&self) -> bool {
        self.before_ts.is_none() && self.memory_type.is_none() && self.score_lt.is_none()
    }
}

/// Errors raised by the prune surface. Mirrors Python's "at least one
/// filter required" guard.
#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    #[error("at least one filter required: before_ts, memory_type, or score_lt")]
    NoFilter,
}

/// Dry-run preview — returns the records that would be deleted under the
/// given filters, capped at `max_delete`. No mutation.
pub fn preview(
    store: &TieredStore,
    filters: &PruneFilters,
    max_delete: usize,
) -> Result<Vec<String>, PruneError> {
    if filters.is_empty() {
        return Err(PruneError::NoFilter);
    }
    let candidates = store.list_rows(filters.memory_type.as_deref(), filters.score_lt, max_delete);
    Ok(candidates
        .into_iter()
        .filter(|r| match filters.before_ts {
            Some(thr) => r.created_at < thr,
            None => true,
        })
        .map(|r| r.id)
        .collect())
}

/// Execute the prune. Returns `(deleted_count, deleted_ids)`. Mutates
/// the store but does NOT save — the caller controls when to persist
/// so a transactional flow can compose multiple prunes.
pub fn prune_rows(
    store: &mut TieredStore,
    filters: &PruneFilters,
    max_delete: usize,
) -> Result<(usize, Vec<String>), PruneError> {
    let ids = preview(store, filters, max_delete)?;
    let removed = store.delete_rows(&ids);
    Ok((removed, ids))
}

/// Format a successful prune as the `{deleted, ids, filters}` envelope
/// the `rag_prune` tool returns.
pub fn ok_envelope(deleted: usize, ids: &[String], filters: &PruneFilters) -> Value {
    json!({
        "status": "ok",
        "deleted": deleted,
        "ids": ids,
        "filters": {
            "before_ts": filters.before_ts,
            "memory_type": filters.memory_type,
            "score_lt": filters.score_lt,
        },
    })
}

/// Format a dry-run as the `{would_delete, filters}` envelope the
/// `rag_prune` tool returns when `confirm=false`.
pub fn dry_run_envelope(would_delete: usize, filters: &PruneFilters, max_delete: usize) -> Value {
    json!({
        "status": "dry_run",
        "would_delete": would_delete,
        "filters": {
            "before_ts": filters.before_ts,
            "memory_type": filters.memory_type,
            "score_lt": filters.score_lt,
        },
        "max_delete": max_delete,
        "note": "Set confirm=true to actually delete.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::rag::store::TierRecord;
    use crate::memory::rag::test_support::TestEnv;

    fn record_with_ts(id: &str, tier: &str, score: f32, ts: f64) -> TierRecord {
        let mut r = TierRecord::new(id, format!("body-{id}"), tier, score, "", "");
        r.created_at = ts;
        r
    }

    fn seeded() -> (TestEnv, TieredStore) {
        let env = TestEnv::new();
        let mut s = TieredStore::open_at(
            &std::env::var_os("WYLDE_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap(),
            4,
        );
        s.insert(record_with_ts("a", "episodic", 0.3, 100.0), None)
            .unwrap();
        s.insert(record_with_ts("b", "episodic", 0.6, 200.0), None)
            .unwrap();
        s.insert(record_with_ts("c", "core", 0.9, 300.0), None)
            .unwrap();
        (env, s)
    }

    #[test]
    fn preview_with_no_filter_errors() {
        let (_env, s) = seeded();
        let err = preview(&s, &PruneFilters::default(), 100).unwrap_err();
        matches!(err, PruneError::NoFilter);
    }

    #[test]
    fn preview_by_tier_returns_matching_ids() {
        let (_env, s) = seeded();
        let ids = preview(
            &s,
            &PruneFilters {
                memory_type: Some("episodic".into()),
                ..PruneFilters::default()
            },
            100,
        )
        .unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"a".to_owned()));
        assert!(ids.contains(&"b".to_owned()));
    }

    #[test]
    fn preview_by_before_ts_drops_older_rows() {
        let (_env, s) = seeded();
        let ids = preview(
            &s,
            &PruneFilters {
                before_ts: Some(250.0),
                memory_type: Some("episodic".into()),
                ..PruneFilters::default()
            },
            100,
        )
        .unwrap();
        // Only "a" (ts=100) and "b" (ts=200) match episodic; before_ts
        // 250 then drops "c" (which isn't episodic anyway) and keeps
        // both episodic rows.
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn preview_by_score_lt_filters_strict_inequality() {
        let (_env, s) = seeded();
        let ids = preview(
            &s,
            &PruneFilters {
                score_lt: Some(0.6),
                ..PruneFilters::default()
            },
            100,
        )
        .unwrap();
        // Only "a" (0.3) satisfies score < 0.6. "b" (0.6) is excluded by strict-LT.
        assert_eq!(ids, vec!["a".to_owned()]);
    }

    #[test]
    fn preview_respects_max_delete_cap() {
        let (_env, s) = seeded();
        let ids = preview(
            &s,
            &PruneFilters {
                memory_type: Some("episodic".into()),
                ..PruneFilters::default()
            },
            1,
        )
        .unwrap();
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn prune_rows_actually_deletes_from_store() {
        let (_env, mut s) = seeded();
        let (n, ids) = prune_rows(
            &mut s,
            &PruneFilters {
                memory_type: Some("episodic".into()),
                ..PruneFilters::default()
            },
            100,
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(ids.len(), 2);
        assert_eq!(s.count_rows(), 1);
    }

    #[test]
    fn ok_envelope_shape_matches_python() {
        let env = ok_envelope(
            3,
            &["a".into(), "b".into(), "c".into()],
            &PruneFilters {
                before_ts: Some(100.0),
                memory_type: Some("core".into()),
                score_lt: None,
            },
        );
        assert_eq!(env["status"], "ok");
        assert_eq!(env["deleted"], 3);
        assert_eq!(env["ids"][0], "a");
        assert_eq!(env["filters"]["memory_type"], "core");
        assert!(env["filters"]["score_lt"].is_null());
    }

    #[test]
    fn dry_run_envelope_carries_max_delete_cap() {
        let env = dry_run_envelope(
            5,
            &PruneFilters {
                memory_type: Some("episodic".into()),
                ..PruneFilters::default()
            },
            10,
        );
        assert_eq!(env["status"], "dry_run");
        assert_eq!(env["would_delete"], 5);
        assert_eq!(env["max_delete"], 10);
    }
}
