//! `LongTermMemory` — one authoritative record + JSON serialisation.
//!
//! Wire shape matches Python's `long_term.LongTermMemory.to_dict()`
//! exactly so the Settings UI reads either side without conversion:
//!
//! ```json
//! {
//!   "id": "8af3...",
//!   "body": "user said X",
//!   "source": "settings_ui",
//!   "importance": 7,
//!   "created_at": 1764234567.123,
//!   "last_used_at": 1764234567.123,
//!   "superseded_by": "",
//!   "tags": ["alpha"]
//! }
//! ```

use serde::{Deserialize, Serialize};

/// One long-term memory record. Field order in the JSON output matches
/// Python's `to_dict()` purely to keep diff-against-Python output
/// readable — Python dicts are insertion-ordered too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongTermMemory {
    pub id: String,
    pub body: String,
    #[serde(default)]
    pub source: String,
    #[serde(default = "default_importance")]
    pub importance: i32,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub last_used_at: f64,
    #[serde(default)]
    pub superseded_by: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_importance() -> i32 {
    5
}

impl LongTermMemory {
    /// Build a fresh record. Caller fills in `id` + timestamps.
    pub fn new(id: String, body: String) -> Self {
        Self {
            id,
            body,
            source: String::new(),
            importance: 5,
            created_at: 0.0,
            last_used_at: 0.0,
            superseded_by: String::new(),
            tags: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_serde_json() {
        let r = LongTermMemory {
            id: "abc".into(),
            body: "hello".into(),
            source: "ui".into(),
            importance: 7,
            created_at: 1.0,
            last_used_at: 2.0,
            superseded_by: String::new(),
            tags: vec!["x".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: LongTermMemory = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn deserialises_python_to_dict_output() {
        // Exact shape from Python `LongTermMemory.to_dict()` —
        // ensure the Rust struct round-trips against a Python-shaped
        // input without losing fields.
        let raw = json!({
            "id": "deadbeef",
            "body": "hello",
            "source": "settings_ui",
            "importance": 7,
            "created_at": 1.5,
            "last_used_at": 2.5,
            "superseded_by": "",
            "tags": ["a", "b"],
        });
        let r: LongTermMemory = serde_json::from_value(raw).unwrap();
        assert_eq!(r.id, "deadbeef");
        assert_eq!(r.importance, 7);
        assert_eq!(r.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let raw = json!({"id": "x", "body": "y"});
        let r: LongTermMemory = serde_json::from_value(raw).unwrap();
        assert_eq!(r.importance, 5);
        assert_eq!(r.created_at, 0.0);
        assert_eq!(r.last_used_at, 0.0);
        assert!(r.superseded_by.is_empty());
        assert!(r.tags.is_empty());
    }
}
