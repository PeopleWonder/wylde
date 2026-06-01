//! Opt-in HuggingFace discovery — `discovery_status`, scheduled-search
//! state. Rust port of `_routing/hf_search.py`.
//!
//! Network calls (the actual `https://huggingface.co/api/models` GET)
//! are NOT performed from this crate. The harness owns no outbound HTTP
//! client today; the network half of model discovery routes through
//! `wylde-gateway` egress in a follow-up slice. This module exposes the
//! state-side surface — last-search timestamp, schedule, enabled flag —
//! so the GUI can render the status panel without depending on the
//! Python implementation.

use serde_json::{json, Value};

use crate::model_registry::routing::{
    discovery_enabled, discovery_file, discovery_schedule, load_json,
};

/// Snapshot of the discovery loop's status for the GUI status panel.
/// Mirrors Python's `discovery_status()` dict shape.
pub fn discovery_status() -> Value {
    let info = load_json(&discovery_file(), json!({}));
    json!({
        "enabled": discovery_enabled(),
        "schedule": discovery_schedule(),
        "last_search_at": info.get("last_search_at").cloned().unwrap_or(Value::Null),
        "last_results_count": info
            .get("last_results_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "note": "Search is opt-in. Set MODEL_DISCOVERY_ENABLED=true to enable scheduled search.",
    })
}

/// Placeholder for the network-bound `hf_search(vram_gb, capability)`
/// call from Python. The Rust harness has no outbound HTTP client today;
/// returning an explanatory envelope lets the strangler-fig stay safe
/// (callers see "off" instead of crashing) and signals that the real
/// network implementation lives behind a follow-up slice routed through
/// `wylde-gateway`. The Python implementation remains canonical until
/// then.
///
/// Returns a JSON-shaped error envelope rather than panicking so the
/// `model_registry.hf_search(...)` action surface can be wired without
/// regression.
pub fn hf_search(_vram_gb: f64, _capability: &str) -> Value {
    json!({
        "results": [],
        "note": "hf_search not yet wired in Rust; route via Python until \
                 the wylde-gateway egress path lands.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use tempfile::tempdir;

    #[test]
    fn discovery_status_reports_disabled_by_default() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior_enabled = std::env::var_os("MODEL_DISCOVERY_ENABLED");
        let prior_schedule = std::env::var_os("MODEL_DISCOVERY_SCHEDULE");
        let prior_dir = std::env::var_os("MODEL_DATA_DIR");
        std::env::remove_var("MODEL_DISCOVERY_ENABLED");
        std::env::remove_var("MODEL_DISCOVERY_SCHEDULE");
        let td = tempdir().unwrap();
        std::env::set_var("MODEL_DATA_DIR", td.path());

        let s = discovery_status();
        assert_eq!(s["enabled"], false);
        assert_eq!(s["schedule"], "weekly");
        assert_eq!(s["last_search_at"], Value::Null);
        assert_eq!(s["last_results_count"], 0);

        match prior_enabled {
            Some(v) => std::env::set_var("MODEL_DISCOVERY_ENABLED", v),
            None => std::env::remove_var("MODEL_DISCOVERY_ENABLED"),
        }
        match prior_schedule {
            Some(v) => std::env::set_var("MODEL_DISCOVERY_SCHEDULE", v),
            None => std::env::remove_var("MODEL_DISCOVERY_SCHEDULE"),
        }
        match prior_dir {
            Some(v) => std::env::set_var("MODEL_DATA_DIR", v),
            None => std::env::remove_var("MODEL_DATA_DIR"),
        }
    }

    #[test]
    fn discovery_status_picks_up_persisted_last_search() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let td = tempdir().unwrap();
        let prior_dir = std::env::var_os("MODEL_DATA_DIR");
        let prior_enabled = std::env::var_os("MODEL_DISCOVERY_ENABLED");
        std::env::set_var("MODEL_DATA_DIR", td.path());
        std::env::set_var("MODEL_DISCOVERY_ENABLED", "true");

        let p = td.path().join("discovery.json");
        std::fs::write(
            &p,
            r#"{"last_search_at": "2026-01-01T00:00:00", "last_results_count": 17}"#,
        )
        .unwrap();

        let s = discovery_status();
        assert_eq!(s["enabled"], true);
        assert_eq!(s["last_search_at"], "2026-01-01T00:00:00");
        assert_eq!(s["last_results_count"], 17);

        match prior_dir {
            Some(v) => std::env::set_var("MODEL_DATA_DIR", v),
            None => std::env::remove_var("MODEL_DATA_DIR"),
        }
        match prior_enabled {
            Some(v) => std::env::set_var("MODEL_DISCOVERY_ENABLED", v),
            None => std::env::remove_var("MODEL_DISCOVERY_ENABLED"),
        }
    }

    #[test]
    fn hf_search_returns_explanatory_envelope_for_now() {
        let r = hf_search(16.0, "code");
        assert!(r["results"].as_array().unwrap().is_empty());
        assert!(r["note"].as_str().unwrap().contains("wylde-gateway"));
    }
}
