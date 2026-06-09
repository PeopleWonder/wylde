//! `chat/search/` — scoped chat-history search tools (Thought Bubble
//! System **Slice E**, Phase 2).
//!
//! **Conceptual path:** `Core/Harness/chat/search/`.
//!
//! Three verbs let the assistant (and the GUI) recall past conversations,
//! **strictly bounded to the current scope** (Plan v2 §3.2):
//!
//! | Verb | Tier (Build Order App. A) | One-line |
//! |---|---|---|
//! | `chat.search_history`  | Medium · 2s  · idempotent read | semantic + lexical recall, scope-bounded |
//! | `chat.list_recent`     | Fast · 500ms · idempotent read | recent conversations, scope-bounded |
//! | `chat.get_conversation`| Medium · 2s  · idempotent read | one conversation by id, scope-checked |
//!
//! ## The strict scope boundary (non-negotiable — Plan v2 §3.2)
//!
//! | Caller context | What search can access |
//! |---|---|
//! | Standalone chat (no active workspace) | Standalone conversations only |
//! | Workspace chat (workspace X active)   | Workspace X's conversations **+** standalone |
//! | Anyone                                | **NEVER** another workspace's conversations |
//!
//! All three verbs resolve their reachable backends through
//! [`scope::resolve_scope`]; they never read a store the resolver didn't
//! authorise. A caller that passes `WorkspaceScope::WorkspaceOnly(other)`
//! while operating in a different workspace gets a `bad_request` — you
//! cannot escape scope by passing an id (the [boundary test][scope]).
//!
//! ## Two backends, dispatched by scope (Build Order §3)
//!
//! * **Standalone** conversations (`workspace_id == None`) live in the
//!   harness flat store ([`crate::memory::conversations`]) — read directly.
//! * **Workspace** conversations live in the `wylde-workspaces` service —
//!   read over the pipe via [`wylde_workspaces_client`]. A
//!   slow/unreachable service degrades that backend away (the other
//!   backends still answer); it never fails the whole call.
//!
//! ## Auto-summary + embedding pipeline ([`summary`])
//!
//! Each standalone conversation grows an `auto_summary`, `topic_tags`, and
//! an `embedding` (Plan v2 §3.4). [`search_history`] ranks by cosine over
//! those embeddings when present, falling back to lexical overlap so search
//! still works before the embedder has run (or when Ollama is down).
//!
//! ## Tiers, not cache (spec wins over the brief)
//!
//! Build Order Appendix A lists these as **harness-dispatched** verbs with
//! an empty cache column — the `wylde-workspaces-client` TTL cache (Plan v2
//! §7.6) applies to `workspaces.*` client verbs, not to these in-harness
//! handlers. So there is no cache layer here; the tiers above are
//! descriptive. (Same brief-vs-spec reconciliation as Slices B/F/G/N-data.)
//!
//! [scope]: scope::resolve_scope

pub mod api;
pub mod scope;
pub mod summary;

use serde_json::{json, Value};

/// Default cap on `search_history` results.
pub const DEFAULT_TOP_K: usize = 20;

/// Default minimum relevance for a hit to be returned by `search_history`.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// A conversation that matched a search / list, with its relevance and the
/// metadata the GUI sidebar renders.
///
/// Timestamps are epoch **seconds** (`i64`), matching the on-disk
/// conversation document and the `conversations.list` metadata shape
/// (`created_at` / `updated_at`). The Build Order's `DateTime`-typed sketch
/// is represented this way to stay byte-compatible with the proven store —
/// the same "proven layout > canonical churn" stance Slices 0b/0c/N-data
/// took.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationHit {
    pub id: String,
    /// `created_at` — the Build Order's `date`.
    pub created_at: i64,
    /// `updated_at` / `last_active_at` — most recent activity.
    pub last_active_at: i64,
    pub summary: String,
    pub topic_tags: Vec<String>,
    /// Semantic-search relevance in `[0, 1]`; `1.0` for non-search calls
    /// (`list_recent`).
    pub score: f32,
    /// `None` for a standalone conversation; `Some(id)` for a workspace one.
    pub workspace_id: Option<String>,
}

impl ConversationHit {
    /// Render to the JSON shape the GUI consumes — field names match
    /// `conversations.list` (`workspace_id` is `""` for standalone, never
    /// `null`, so existing readers stay happy).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "created_at": self.created_at,
            "last_active_at": self.last_active_at,
            "summary": self.summary,
            "topic_tags": self.topic_tags,
            "score": self.score,
            "workspace_id": self.workspace_id.clone().unwrap_or_default(),
        })
    }
}

/// An inclusive epoch-seconds window to filter conversations by. Either
/// bound may be open (`None`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DateRange {
    pub from: Option<i64>,
    pub to: Option<i64>,
}

impl DateRange {
    /// True when a conversation whose active window is `[created, active]`
    /// intersects this range. An open bound never excludes.
    pub fn includes(&self, created_at: i64, last_active_at: i64) -> bool {
        // The conversation spans [created_at, last_active_at]; keep it when
        // that span overlaps [from, to].
        if let Some(from) = self.from {
            if last_active_at < from {
                return false;
            }
        }
        if let Some(to) = self.to {
            if created_at > to {
                return false;
            }
        }
        true
    }

    /// Parse `{from?, to?}` (epoch seconds) from a payload value. A missing
    /// / null / non-object value yields the fully-open range.
    pub fn from_payload(value: Option<&Value>) -> Self {
        let Some(obj) = value.and_then(Value::as_object) else {
            return Self::default();
        };
        Self {
            from: obj.get("from").and_then(Value::as_i64),
            to: obj.get("to").and_then(Value::as_i64),
        }
    }
}

/// Errors surfaced by the scoped-search verbs. Each maps to a stable IPC
/// error code in the action layer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChatSearchError {
    /// Caller passed a malformed request, an invalid id, or tried to escape
    /// scope (e.g. `WorkspaceOnly(other)` from a different workspace).
    /// → `bad_request`.
    #[error("{0}")]
    BadRequest(String),

    /// `get_conversation`: the id wasn't found in any in-scope backend.
    /// → `not_found`.
    #[error("{0}")]
    NotFound(String),
}

impl ChatSearchError {
    /// The stable IPC error code for this variant.
    pub fn code(&self) -> &'static str {
        match self {
            ChatSearchError::BadRequest(_) => "bad_request",
            ChatSearchError::NotFound(_) => "not_found",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_range_open_includes_everything() {
        let r = DateRange::default();
        assert!(r.includes(0, 0));
        assert!(r.includes(i64::MIN, i64::MAX));
    }

    #[test]
    fn date_range_from_bound_excludes_older() {
        let r = DateRange {
            from: Some(100),
            to: None,
        };
        // last_active 50 < from 100 → excluded.
        assert!(!r.includes(10, 50));
        // last_active 150 >= from → included even if created earlier.
        assert!(r.includes(10, 150));
    }

    #[test]
    fn date_range_to_bound_excludes_newer() {
        let r = DateRange {
            from: None,
            to: Some(100),
        };
        // created 150 > to 100 → excluded.
        assert!(!r.includes(150, 200));
        // created 50 <= to → included.
        assert!(r.includes(50, 200));
    }

    #[test]
    fn date_range_from_payload_reads_bounds() {
        let r = DateRange::from_payload(Some(&json!({"from": 5, "to": 9})));
        assert_eq!(r.from, Some(5));
        assert_eq!(r.to, Some(9));
        // Missing / wrong type → open.
        assert_eq!(DateRange::from_payload(None), DateRange::default());
        assert_eq!(
            DateRange::from_payload(Some(&json!("nope"))),
            DateRange::default()
        );
    }

    #[test]
    fn hit_json_emits_empty_string_for_standalone_workspace() {
        let hit = ConversationHit {
            id: "c1".into(),
            created_at: 1,
            last_active_at: 2,
            summary: "hi".into(),
            topic_tags: vec!["a".into()],
            score: 0.9,
            workspace_id: None,
        };
        let v = hit.to_json();
        assert_eq!(v["workspace_id"], "");
        assert_eq!(v["id"], "c1");
        assert_eq!(v["topic_tags"][0], "a");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ChatSearchError::BadRequest("x".into()).code(), "bad_request");
        assert_eq!(ChatSearchError::NotFound("x".into()).code(), "not_found");
    }
}
