//! `WorkspaceMemory` — one workspace-scoped record + lenient decode.
//!
//! Wire shape matches Python's `_store.WorkspaceMemory.to_dict()`
//! exactly so the gateway / Settings UI read either side without
//! conversion:
//!
//! ```json
//! {
//!   "id": "8af3c2d4e5f60718",
//!   "workspace_id": "my-project",
//!   "body": "the build watcher polls outputs/build-requests",
//!   "source": "chat",
//!   "importance": 7,
//!   "created_at": 1764234567.123,
//!   "last_used_at": 1764234567.123,
//!   "superseded_by": "",
//!   "entities": ["build watcher"]
//! }
//! ```
//!
//! Decoding goes through [`WorkspaceMemory::from_value_lenient`]
//! rather than a serde `Deserialize` derive because the Python
//! `from_dict` coerced types (`str(...)`, `int(... or 5)`,
//! `float(... or 0.0)`) instead of rejecting them — a hand-edited or
//! half-written record must degrade to defaults, never poison the
//! whole file.

use serde::Serialize;
use serde_json::Value;

/// Default importance when the stored value is missing, zero, or
/// unparseable. Matches Python's `int(d.get("importance", 5) or 5)` —
/// note the `or 5`: a stored `0` also reads back as 5.
const DEFAULT_IMPORTANCE: i32 = 5;

/// One workspace memory record. Field order matches Python's
/// `to_dict()` so diff-against-Python output stays readable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceMemory {
    pub id: String,
    pub workspace_id: String,
    pub body: String,
    pub source: String,
    pub importance: i32,
    pub created_at: f64,
    pub last_used_at: f64,
    pub superseded_by: String,
    pub entities: Vec<String>,
}

impl WorkspaceMemory {
    /// Lenient decode of one stored item — the Rust mirror of Python's
    /// `WorkspaceMemory.from_dict`. Non-objects are skipped (`None`);
    /// inside an object every field falls back to its default when
    /// missing or wrong-typed:
    ///
    /// * string fields    → `""` (numbers are stringified, like `str()`)
    /// * `importance`     → 5 (and `0` reads as 5 — the Python `or 5` quirk)
    /// * timestamps       → `0.0` (numeric strings parse, like `float()`)
    /// * `entities`       → the string elements only (non-strings dropped;
    ///   Python kept them as-is, but they were never written by the
    ///   harness — `Vec<String>` keeps the rest of the crate honest)
    pub fn from_value_lenient(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        Some(Self {
            id: coerce_string(obj.get("id")),
            workspace_id: coerce_string(obj.get("workspace_id")),
            body: coerce_string(obj.get("body")),
            source: coerce_string(obj.get("source")),
            importance: coerce_importance(obj.get("importance")),
            created_at: coerce_f64(obj.get("created_at")),
            last_used_at: coerce_f64(obj.get("last_used_at")),
            superseded_by: coerce_string(obj.get("superseded_by")),
            entities: coerce_string_list(obj.get("entities")),
        })
    }

    /// JSON wire shape — matches Python `to_dict()` key-for-key.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("WorkspaceMemory serializes to JSON")
    }
}

fn coerce_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn coerce_f64(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn coerce_importance(v: Option<&Value>) -> i32 {
    let n = coerce_f64(v);
    // Python: `int(d.get("importance", 5) or 5)` — 0 / missing /
    // unparseable all land on the default.
    if n == 0.0 {
        DEFAULT_IMPORTANCE
    } else {
        n as i32
    }
}

fn coerce_string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_python_to_dict_output_field_for_field() {
        let raw = json!({
            "id": "deadbeefdeadbeef",
            "workspace_id": "proj",
            "body": "hello",
            "source": "chat",
            "importance": 7,
            "created_at": 1.5,
            "last_used_at": 2.5,
            "superseded_by": "",
            "entities": ["a", "b"],
        });
        let r = WorkspaceMemory::from_value_lenient(&raw).unwrap();
        assert_eq!(r.id, "deadbeefdeadbeef");
        assert_eq!(r.workspace_id, "proj");
        assert_eq!(r.body, "hello");
        assert_eq!(r.source, "chat");
        assert_eq!(r.importance, 7);
        assert_eq!(r.created_at, 1.5);
        assert_eq!(r.last_used_at, 2.5);
        assert!(r.superseded_by.is_empty());
        assert_eq!(r.entities, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn missing_fields_use_python_defaults() {
        let r = WorkspaceMemory::from_value_lenient(&json!({"id": "x"})).unwrap();
        assert_eq!(r.id, "x");
        assert!(r.workspace_id.is_empty());
        assert!(r.body.is_empty());
        assert!(r.source.is_empty());
        assert_eq!(r.importance, 5);
        assert_eq!(r.created_at, 0.0);
        assert_eq!(r.last_used_at, 0.0);
        assert!(r.superseded_by.is_empty());
        assert!(r.entities.is_empty());
    }

    #[test]
    fn importance_zero_reads_as_default_like_python_or_five() {
        let r = WorkspaceMemory::from_value_lenient(&json!({"importance": 0})).unwrap();
        assert_eq!(r.importance, 5);
    }

    #[test]
    fn importance_numeric_string_parses_like_python_int() {
        let r = WorkspaceMemory::from_value_lenient(&json!({"importance": "7"})).unwrap();
        assert_eq!(r.importance, 7);
    }

    #[test]
    fn wrong_typed_fields_degrade_to_defaults_not_errors() {
        let raw = json!({
            "id": 42,
            "body": null,
            "importance": "not a number",
            "created_at": {"nested": true},
            "entities": "not a list",
        });
        let r = WorkspaceMemory::from_value_lenient(&raw).unwrap();
        assert_eq!(r.id, "42"); // str()-style coercion
        assert!(r.body.is_empty());
        assert_eq!(r.importance, 5);
        assert_eq!(r.created_at, 0.0);
        assert!(r.entities.is_empty());
    }

    #[test]
    fn non_object_items_are_skipped() {
        assert!(WorkspaceMemory::from_value_lenient(&json!("a string")).is_none());
        assert!(WorkspaceMemory::from_value_lenient(&json!(7)).is_none());
        assert!(WorkspaceMemory::from_value_lenient(&Value::Null).is_none());
    }

    #[test]
    fn to_value_carries_exactly_the_nine_wire_keys() {
        let r = WorkspaceMemory {
            id: "abc".into(),
            workspace_id: "ws".into(),
            body: "b".into(),
            source: "s".into(),
            importance: 6,
            created_at: 1.0,
            last_used_at: 2.0,
            superseded_by: String::new(),
            entities: vec!["e".into()],
        };
        let v = r.to_value();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "body",
                "created_at",
                "entities",
                "id",
                "importance",
                "last_used_at",
                "source",
                "superseded_by",
                "workspace_id",
            ]
        );
        assert_eq!(v["importance"], 6);
        assert_eq!(v["entities"], json!(["e"]));
    }

    #[test]
    fn round_trips_through_to_value_and_lenient_decode() {
        let r = WorkspaceMemory {
            id: "abc".into(),
            workspace_id: "ws".into(),
            body: "hello world".into(),
            source: "chat".into(),
            importance: 9,
            created_at: 10.5,
            last_used_at: 20.5,
            superseded_by: "def".into(),
            entities: vec!["x".into(), "y".into()],
        };
        let back = WorkspaceMemory::from_value_lenient(&r.to_value()).unwrap();
        assert_eq!(r, back);
    }
}
