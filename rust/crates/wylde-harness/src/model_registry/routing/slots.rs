//! Capability slots & smart routing — `CAPABILITY_SLOTS`,
//! `select_model`. Rust port of `_routing/slots.py`.
//!
//! A *slot* is one capability label (`code`, `reasoning`, …); each slot
//! tracks one "current best" active model. [`select_model`] is the
//! read-side API: given a capability and budget mode, pick the
//! highest-scoring active model.

use serde_json::Value;

use crate::model_registry::routing::profiles::read_profiles;
use crate::model_registry::routing::INCUMBENT_BONUS;

/// Capability slots — each slot tracks one "current best" model.
/// Matches Python's `CAPABILITY_SLOTS` ordering.
pub const CAPABILITY_SLOTS: &[&str] = &["code", "reasoning", "extraction", "creative", "chat"];

/// Return the best active model name for the requested capability.
/// Mirrors Python's `select_model(capability, budget_mode)`.
///
/// * `capability` — defaults to `"chat"` if empty.
/// * `budget_mode` — `"compact"` triggers a size penalty (divides score
///   by `max(size_gb / 7, 1)`); any other value is treated as `"normal"`.
pub fn select_model(capability: &str, budget_mode: &str) -> Option<String> {
    let capability = if capability.is_empty() { "chat" } else { capability };
    let profiles = read_profiles();
    // Active models with the requested capability declared.
    let mut candidates: Vec<&Value> = profiles
        .values()
        .filter(|p| {
            p.get("status").and_then(Value::as_str) == Some("active")
                && profile_has_capability(p, capability)
        })
        .collect();
    if candidates.is_empty() {
        // Fall back to every active model.
        candidates = profiles
            .values()
            .filter(|p| p.get("status").and_then(Value::as_str) == Some("active"))
            .collect();
    }
    if candidates.is_empty() {
        return None;
    }
    let best = candidates
        .iter()
        .copied()
        .max_by(|a, b| {
            score_for(a, capability, budget_mode)
                .partial_cmp(&score_for(b, capability, budget_mode))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    best.get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn profile_has_capability(p: &Value, capability: &str) -> bool {
    let Some(caps) = p.get("capabilities").and_then(Value::as_array) else {
        return false;
    };
    caps.iter().any(|c| c.as_str() == Some(capability))
}

fn score_for(p: &Value, capability: &str, budget_mode: &str) -> f64 {
    let mut base = p
        .get("benchmark_scores")
        .and_then(|s| s.get("task_scores"))
        .and_then(|t| t.get(capability))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if let Some(first_active) = p.get("first_active_at").and_then(Value::as_str) {
        let age_days = days_since_iso(first_active);
        if age_days > 30 {
            base *= 1.0 + INCUMBENT_BONUS;
        }
    }
    if budget_mode == "compact" {
        let size_gb = p.get("size_gb").and_then(Value::as_f64).unwrap_or(7.0);
        let divisor = (size_gb / 7.0).max(1.0);
        base /= divisor;
    }
    base
}

fn days_since_iso(iso: &str) -> i64 {
    // Match Python's `datetime.fromisoformat(iso)`: handles
    // `2025-01-01T00:00:00` (no timezone) and `…+00:00` (with). chrono's
    // `parse_from_rfc3339` accepts the latter; for the no-tz form we
    // fall through to NaiveDateTime → UTC.
    let now = chrono::Utc::now();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return (now - dt.with_timezone(&chrono::Utc)).num_days();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        let dt = naive.and_utc();
        return (now - dt).num_days();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S") {
        let dt = naive.and_utc();
        return (now - dt).num_days();
    }
    // Python's `datetime.fromisoformat` raises on bad input; the
    // surrounding `try/except Exception` swallows it and falls back to
    // age_days=0 (which means no incumbent bonus).
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_registry::routing::profiles::{test_support::TestEnv, upsert_profile};
    use serde_json::json;

    #[test]
    fn select_returns_none_with_no_profiles() {
        let _env = TestEnv::new();
        assert!(select_model("chat", "normal").is_none());
    }

    #[test]
    fn picks_highest_scoring_active_for_capability() {
        let _env = TestEnv::new();
        upsert_profile(
            "a",
            json!({
                "status": "active",
                "capabilities": ["code"],
                "benchmark_scores": {"task_scores": {"code": 0.5}},
            }),
        );
        upsert_profile(
            "b",
            json!({
                "status": "active",
                "capabilities": ["code"],
                "benchmark_scores": {"task_scores": {"code": 0.9}},
            }),
        );
        assert_eq!(select_model("code", "normal"), Some("b".to_owned()));
    }

    #[test]
    fn falls_back_to_any_active_when_no_capability_match() {
        let _env = TestEnv::new();
        upsert_profile(
            "only",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "benchmark_scores": {"task_scores": {"chat": 0.4}},
            }),
        );
        let r = select_model("code", "normal");
        assert_eq!(r, Some("only".to_owned()));
    }

    #[test]
    fn ignores_candidate_status_when_active_exists() {
        let _env = TestEnv::new();
        upsert_profile(
            "active",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        upsert_profile(
            "candidate",
            json!({
                "status": "candidate",
                "capabilities": ["chat"],
                "benchmark_scores": {"task_scores": {"chat": 0.99}},
            }),
        );
        assert_eq!(select_model("chat", "normal"), Some("active".to_owned()));
    }

    #[test]
    fn compact_mode_penalises_large_models() {
        let _env = TestEnv::new();
        upsert_profile(
            "big",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "size_gb": 70.0,
                "benchmark_scores": {"task_scores": {"chat": 0.9}},
            }),
        );
        upsert_profile(
            "small",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "size_gb": 7.0,
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        // Big: 0.9 / max(70/7, 1) = 0.09
        // Small: 0.5 / max(7/7, 1) = 0.5
        assert_eq!(
            select_model("chat", "compact"),
            Some("small".to_owned())
        );
        // Normal mode: big wins.
        assert_eq!(
            select_model("chat", "normal"),
            Some("big".to_owned())
        );
    }

    #[test]
    fn incumbent_bonus_kicks_in_after_30_days() {
        let _env = TestEnv::new();
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(60)).format("%Y-%m-%dT%H:%M:%S").to_string();
        upsert_profile(
            "incumbent",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "first_active_at": old_ts,
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        upsert_profile(
            "newer",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "benchmark_scores": {"task_scores": {"chat": 0.52}},
            }),
        );
        // Incumbent's effective score: 0.5 * 1.05 = 0.525 — beats 0.52.
        assert_eq!(
            select_model("chat", "normal"),
            Some("incumbent".to_owned())
        );
    }

    #[test]
    fn empty_capability_defaults_to_chat() {
        let _env = TestEnv::new();
        upsert_profile(
            "x",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        assert_eq!(select_model("", "normal"), Some("x".to_owned()));
    }
}
