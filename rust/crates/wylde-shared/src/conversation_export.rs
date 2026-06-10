//! The portable conversation-export envelope (TBS Slice J — "the escape
//! hatch for moving context across workspaces").
//!
//! One envelope shape for BOTH conversation stores — the harness's
//! standalone flat store and `wylde-workspaces`' per-workspace bundles —
//! lifted here so the two processes agree by construction (the Slice
//! N-data `Anchor` precedent):
//!
//! ```json
//! {
//!   "format": "wylde-conversation-export",
//!   "version": 1,
//!   "scope": "workspace" | "standalone",
//!   "conversation": { …the stored document, verbatim… }
//! }
//! ```
//!
//! The `conversation` field is the store's raw `Value` document — never a
//! typed re-encode — so export → import **round-trips byte-exact** (the
//! Phase 5 acceptance): no field a struct didn't know about is dropped, no
//! key order churn beyond serde_json's stable map behaviour. Deliberately
//! NO `exported_at` stamp: exporting the same conversation twice yields
//! identical bytes, which makes the round-trip property testable and diffs
//! meaningful.
//!
//! Export always emits **plaintext** JSON regardless of the at-rest
//! encryption setting (OI-14): DPAPI ciphertext is machine+user-bound, and
//! the entire point of the escape hatch is leaving the machine. Import
//! writes back through the store, which re-encrypts when enabled.

use serde_json::{json, Map, Value};

/// The envelope's `format` discriminator.
pub const FORMAT: &str = "wylde-conversation-export";
/// Current envelope version. Bump on any breaking shape change.
pub const VERSION: u64 = 1;

/// Why an envelope failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwrapError {
    /// Not a JSON object at all.
    NotAnObject,
    /// `format` missing or not [`FORMAT`].
    WrongFormat(String),
    /// `version` missing or newer than this build understands.
    UnsupportedVersion(u64),
    /// No `conversation` object inside.
    MissingConversation,
    /// The conversation document has no usable string `id`.
    MissingId,
}

impl UnwrapError {
    pub fn message(&self) -> String {
        match self {
            UnwrapError::NotAnObject => "export envelope must be a JSON object".to_owned(),
            UnwrapError::WrongFormat(got) => {
                format!("not a {FORMAT} document (format: {got:?})")
            }
            UnwrapError::UnsupportedVersion(v) => {
                format!("export version {v} is newer than this build understands (max {VERSION})")
            }
            UnwrapError::MissingConversation => {
                "export envelope has no `conversation` object".to_owned()
            }
            UnwrapError::MissingId => "exported conversation has no usable `id`".to_owned(),
        }
    }
}

/// Wrap a stored conversation document in the portable envelope.
/// `scope` labels where it came from (`"workspace"` / `"standalone"`) —
/// provenance for humans; import ignores it (any envelope imports into any
/// store).
pub fn wrap(scope: &str, conversation: Value) -> Value {
    json!({
        "format": FORMAT,
        "version": VERSION,
        "scope": scope,
        "conversation": conversation,
    })
}

/// Validate an envelope and hand back the conversation document (with its
/// id). The returned map is the embedded document verbatim.
pub fn unwrap(envelope: &Value) -> Result<(String, Map<String, Value>), UnwrapError> {
    let Some(obj) = envelope.as_object() else {
        return Err(UnwrapError::NotAnObject);
    };
    let format = obj.get("format").and_then(Value::as_str).unwrap_or("");
    if format != FORMAT {
        return Err(UnwrapError::WrongFormat(format.to_owned()));
    }
    let version = obj.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version == 0 || version > VERSION {
        return Err(UnwrapError::UnsupportedVersion(version));
    }
    let Some(Value::Object(doc)) = obj.get("conversation") else {
        return Err(UnwrapError::MissingConversation);
    };
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(UnwrapError::MissingId)?
        .to_owned();
    Ok((id, doc.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        json!({
            "id": "conv-1",
            "title": "T",
            "messages": [{"role": "user", "content": "hi"}],
            "working_memory": [],
            "some_future_field": {"kept": true},
        })
    }

    #[test]
    fn wrap_unwrap_round_trips_the_document_verbatim() {
        let envelope = wrap("workspace", doc());
        assert_eq!(envelope["format"], FORMAT);
        assert_eq!(envelope["version"], VERSION);
        assert_eq!(envelope["scope"], "workspace");

        let (id, got) = unwrap(&envelope).expect("valid");
        assert_eq!(id, "conv-1");
        // Byte-exact: the embedded document re-serialises identically.
        assert_eq!(
            serde_json::to_string(&Value::Object(got)).unwrap(),
            serde_json::to_string(&doc()).unwrap()
        );
    }

    #[test]
    fn wrapping_twice_is_deterministic() {
        // No timestamp → same input, same bytes (the testable round-trip).
        let a = serde_json::to_string(&wrap("standalone", doc())).unwrap();
        let b = serde_json::to_string(&wrap("standalone", doc())).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unwrap_rejects_each_failure_mode_distinctly() {
        assert_eq!(unwrap(&json!("nope")), Err(UnwrapError::NotAnObject));
        assert_eq!(
            unwrap(&json!({"format": "something-else", "version": 1})),
            Err(UnwrapError::WrongFormat("something-else".to_owned()))
        );
        assert_eq!(
            unwrap(&json!({"format": FORMAT, "version": 99, "conversation": {"id": "x"}})),
            Err(UnwrapError::UnsupportedVersion(99))
        );
        assert_eq!(
            unwrap(&json!({"format": FORMAT, "version": 1})),
            Err(UnwrapError::MissingConversation)
        );
        assert_eq!(
            unwrap(&json!({"format": FORMAT, "version": 1, "conversation": {"title": "no id"}})),
            Err(UnwrapError::MissingId)
        );
        // Every variant renders a non-empty human message.
        for e in [
            UnwrapError::NotAnObject,
            UnwrapError::WrongFormat("x".into()),
            UnwrapError::UnsupportedVersion(2),
            UnwrapError::MissingConversation,
            UnwrapError::MissingId,
        ] {
            assert!(!e.message().is_empty());
        }
    }
}
