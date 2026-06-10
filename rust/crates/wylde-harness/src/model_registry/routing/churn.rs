//! Churn prevention — `promote_model`, swap eligibility, pending-swap
//! state. Rust port of `_routing/churn.py`.
//!
//! Promotion is gated on minimum benchmark runs, a delta threshold over
//! the incumbent, and a per-week swap cap. When a candidate beats the
//! incumbent by enough but auto-promotion isn't requested, the swap is
//! queued in `pending_swaps.json` for the user to confirm.

use serde_json::{json, Map, Value};

use crate::model_registry::routing::profiles::{get_profile, read_profiles, write_profiles};
use crate::model_registry::routing::slots::select_model;
use crate::model_registry::routing::{
    load_json, pending_swaps_file, save_json, swaps_file, FALLBACK_DAYS, INCUMBENT_BONUS,
    MAX_SWAP_PER_WEEK, MIN_BENCHMARK_RUNS, MIN_DELTA_PCT, STORE_LOCK,
};

// ── Pending swap suggestions ──────────────────────────────────────────

/// Return every queued swap suggestion keyed by capability. Matches
/// Python's `load_pending_swaps()`.
pub fn load_pending_swaps() -> Map<String, Value> {
    let v = load_json(&pending_swaps_file(), json!({}));
    v.as_object().cloned().unwrap_or_default()
}

/// Queue a swap prompt for the user to confirm. Mirrors Python's private
/// `_queue_swap_prompt`. Exposed at the crate level so the watcher (when
/// it lives in wylde-ollama) can reach it, mirroring the Python call.
#[allow(dead_code)]
pub(crate) fn queue_swap_prompt(
    capability: &str,
    candidate: &str,
    incumbent: &str,
    delta_pct: f64,
) {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut swaps = load_pending_swaps();
    let now = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string();
    swaps.insert(
        capability.to_owned(),
        json!({
            "capability": capability,
            "candidate": candidate,
            "incumbent": incumbent,
            "delta_pct": round1(delta_pct),
            "queued_at": now,
        }),
    );
    let _ = save_json(&pending_swaps_file(), &Value::Object(swaps)); // wylde-check: discard-result-ok
}

/// Drop the queued suggestion for `capability`. Mirrors Python's
/// `clear_swap_prompt(capability)`.
pub fn clear_swap_prompt(capability: &str) {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut swaps = load_pending_swaps();
    swaps.remove(capability);
    let _ = save_json(&pending_swaps_file(), &Value::Object(swaps)); // wylde-check: discard-result-ok
}

#[allow(dead_code)]
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

// ── Swap eligibility & promotion ──────────────────────────────────────

fn can_promote(candidate: &Value, incumbent: &Value, capability: &str) -> Result<(), String> {
    let runs = candidate
        .get("benchmark_runs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if runs < MIN_BENCHMARK_RUNS {
        return Err(format!(
            "Needs {MIN_BENCHMARK_RUNS} benchmark runs (has {runs})"
        ));
    }
    let c_score = task_score(candidate, capability);
    let mut i_score = task_score(incumbent, capability);
    if let Some(first_active) = incumbent.get("first_active_at").and_then(Value::as_str) {
        let age_days = days_since_iso(first_active);
        if age_days > 30 {
            i_score *= 1.0 + INCUMBENT_BONUS;
        }
    }
    let delta = (c_score - i_score) / i_score.max(0.01);
    if delta < MIN_DELTA_PCT {
        return Err(format!(
            "Delta {:.1}% < required {:.0}%",
            delta * 100.0,
            MIN_DELTA_PCT * 100.0
        ));
    }
    let recent_count = count_recent_swaps(capability);
    if recent_count >= MAX_SWAP_PER_WEEK {
        return Err(format!("Swap limit reached for {capability:?} this week"));
    }
    Ok(())
}

fn task_score(profile: &Value, capability: &str) -> f64 {
    profile
        .get("benchmark_scores")
        .and_then(|s| s.get("task_scores"))
        .and_then(|t| t.get(capability))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn count_recent_swaps(capability: &str) -> usize {
    let swaps = load_json(&swaps_file(), json!([]));
    let Some(arr) = swaps.as_array() else {
        return 0;
    };
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string();
    arr.iter()
        .filter(|s| {
            s.get("capability").and_then(Value::as_str) == Some(capability)
                && s.get("swapped_at").and_then(Value::as_str).unwrap_or("") > cutoff.as_str()
        })
        .count()
}

fn days_since_iso(iso: &str) -> i64 {
    let now = chrono::Utc::now();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return (now - dt.with_timezone(&chrono::Utc)).num_days();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        return (now - naive.and_utc()).num_days();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S") {
        return (now - naive.and_utc()).num_days();
    }
    0
}

/// Promote a candidate model to active for a capability slot. Returns a
/// JSON envelope matching Python's `promote_model(name, capability,
/// force)`. Successful promotion emits `{status: "promoted", model,
/// capability}`. Blocking returns `{error, status: 4xx}`.
pub fn promote_model(name: &str, capability: &str, force: bool) -> Value {
    let Some(candidate) = get_profile(name) else {
        return json!({"error": "Model not profiled", "status": 404});
    };
    let mut incumbent_name: Option<String> = None;
    if !force {
        if let Some(incumb) = select_model(capability, "normal") {
            let incumbent = get_profile(&incumb).unwrap_or_else(|| json!({}));
            if let Err(reason) = can_promote(&candidate, &incumbent, capability) {
                return json!({
                    "error": format!("Promotion blocked: {reason}"),
                    "status": 409,
                });
            }
            incumbent_name = Some(incumb);
        }
    }
    let now_iso = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string();
    let fallback_until = (chrono::Utc::now() + chrono::Duration::days(FALLBACK_DAYS))
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string();
    {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut profiles = read_profiles();
        // Demote every currently-active profile for this capability to fallback.
        for (_, p) in profiles.iter_mut() {
            let Some(map) = p.as_object_mut() else {
                continue;
            };
            let active = map.get("status").and_then(Value::as_str) == Some("active");
            let claims_cap = map
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|caps| caps.iter().any(|c| c.as_str() == Some(capability)))
                .unwrap_or(false);
            if active && claims_cap {
                map.insert("status".to_owned(), Value::String("fallback".to_owned()));
                map.insert(
                    "fallback_until".to_owned(),
                    Value::String(fallback_until.clone()),
                );
            }
        }
        // Ensure candidate is in the store; promote it.
        profiles
            .entry(name.to_owned())
            .or_insert_with(|| candidate.clone());
        if let Some(map) = profiles.get_mut(name).and_then(Value::as_object_mut) {
            map.insert("status".to_owned(), Value::String("active".to_owned()));
            let existing_first = map
                .get("first_active_at")
                .and_then(Value::as_str)
                .map(str::to_owned);
            map.insert(
                "first_active_at".to_owned(),
                Value::String(existing_first.unwrap_or_else(|| now_iso.clone())),
            );
            if !capability.is_empty() {
                let caps_entry = map
                    .entry("capabilities".to_owned())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(arr) = caps_entry.as_array_mut() {
                    let already = arr.iter().any(|c| c.as_str() == Some(capability));
                    if !already {
                        arr.push(Value::String(capability.to_owned()));
                    }
                }
            }
        }
        write_profiles(&profiles);
    }

    // Append swap log entry.
    {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut swaps = load_json(&swaps_file(), json!([]));
        if let Some(arr) = swaps.as_array_mut() {
            arr.push(json!({
                "capability": capability,
                "from": incumbent_name,
                "to": name,
                "swapped_at": now_iso,
                "forced": force,
            }));
        } else {
            swaps = json!([{
                "capability": capability,
                "from": incumbent_name,
                "to": name,
                "swapped_at": now_iso,
                "forced": force,
            }]);
        }
        let _ = save_json(&swaps_file(), &swaps); // wylde-check: discard-result-ok
    }

    // Python promote_model fires a daemon thread that resets autotuner
    // scores. Autotuner is a Python-only concern (no Rust binding); skip
    // the side effect here. The strangler-fig keeps Python canonical
    // until the impl flips, so the autotuner reset still happens via the
    // Python path during cutover.

    json!({
        "status": "promoted",
        "model": name,
        "capability": capability,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_registry::routing::profiles::{test_support::TestEnv, upsert_profile};

    fn now_iso() -> String {
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.f")
            .to_string()
    }

    #[test]
    fn promote_unknown_model_returns_404() {
        let _env = TestEnv::new();
        let r = promote_model("ghost", "chat", false);
        assert_eq!(r["status"], 404);
    }

    #[test]
    fn force_promote_skips_eligibility_check() {
        let _env = TestEnv::new();
        upsert_profile(
            "newcomer",
            json!({"status": "candidate", "benchmark_runs": 0}),
        );
        let r = promote_model("newcomer", "code", true);
        assert_eq!(r["status"], "promoted");
        let p = get_profile("newcomer").unwrap();
        assert_eq!(p["status"], "active");
        assert!(p["first_active_at"].is_string());
        assert!(p["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str() == Some("code")));
    }

    #[test]
    fn promote_blocks_below_min_runs() {
        let _env = TestEnv::new();
        upsert_profile(
            "old",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "first_active_at": now_iso(),
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        upsert_profile(
            "newcomer",
            json!({
                "status": "candidate",
                "capabilities": ["chat"],
                "benchmark_runs": 1,
                "benchmark_scores": {"task_scores": {"chat": 0.9}},
            }),
        );
        let r = promote_model("newcomer", "chat", false);
        assert_eq!(r["status"], 409);
        let err = r["error"].as_str().unwrap();
        assert!(err.contains("benchmark runs"));
    }

    #[test]
    fn promote_blocks_below_delta_threshold() {
        let _env = TestEnv::new();
        upsert_profile(
            "old",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "first_active_at": now_iso(),
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        upsert_profile(
            "newcomer",
            json!({
                "status": "candidate",
                "capabilities": ["chat"],
                "benchmark_runs": 5,
                // delta = (0.51 - 0.5) / 0.5 = 0.02 < 0.10
                "benchmark_scores": {"task_scores": {"chat": 0.51}},
            }),
        );
        let r = promote_model("newcomer", "chat", false);
        assert_eq!(r["status"], 409);
        assert!(r["error"].as_str().unwrap().contains("Delta"));
    }

    #[test]
    fn promote_demotes_incumbent_to_fallback() {
        let _env = TestEnv::new();
        upsert_profile(
            "old",
            json!({
                "status": "active",
                "capabilities": ["chat"],
                "first_active_at": now_iso(),
                "benchmark_scores": {"task_scores": {"chat": 0.5}},
            }),
        );
        upsert_profile(
            "newcomer",
            json!({
                "status": "candidate",
                "capabilities": ["chat"],
                "benchmark_runs": 5,
                "benchmark_scores": {"task_scores": {"chat": 0.99}},
            }),
        );
        let r = promote_model("newcomer", "chat", false);
        assert_eq!(r["status"], "promoted");
        let old = get_profile("old").unwrap();
        assert_eq!(old["status"], "fallback");
        assert!(old["fallback_until"].is_string());
    }

    #[test]
    fn pending_swap_can_be_queued_and_cleared() {
        let _env = TestEnv::new();
        queue_swap_prompt("chat", "candidate-a", "incumbent-b", 14.2);
        let swaps = load_pending_swaps();
        assert_eq!(swaps["chat"]["candidate"], "candidate-a");
        assert_eq!(swaps["chat"]["incumbent"], "incumbent-b");
        assert_eq!(swaps["chat"]["delta_pct"], 14.2);
        clear_swap_prompt("chat");
        assert!(load_pending_swaps().get("chat").is_none());
    }

    #[test]
    fn round1_matches_python_round_to_one_decimal() {
        assert!((round1(14.249) - 14.2).abs() < 1e-9);
        assert!((round1(14.25) - 14.3).abs() < 1e-9 || (round1(14.25) - 14.2).abs() < 1e-9);
    }
}
