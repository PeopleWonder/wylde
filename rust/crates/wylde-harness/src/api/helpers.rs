//! Shared JSON payload helpers for the `HarnessApi` trait methods,
//! split from `api.rs` per architecture-review R1.
//!
//! Pre-Phase-12.1 these lived in pipe/mod.rs as `pub(crate)`. The trait
//! methods (plus the action modules that import
//! `crate::api::require_string`) are the callers now, so they live
//! adjacent to those methods.

use serde_json::Value;

use crate::memory::long_term::LongTermMemory;

pub(crate) fn require_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

pub(crate) fn optional_string(payload: &Value, key: &str) -> Option<String> {
    require_string(payload, key)
}

pub(super) fn record_to_value(record: LongTermMemory) -> Value {
    serde_json::to_value(record).expect("LongTermMemory serializes to JSON")
}

pub(super) fn string_array(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
