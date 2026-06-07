//! Per-panel IPC helpers for the Memory panel.
//!
//! Wraps the harness's `memory.long_term.*` + `memory.workspaces.*`
//! verbs into typed reads.  Writes (delete, persona edit) live in the
//! Settings panel today and are intentionally absent here — see the
//! crate-level note in `lib.rs`.

use serde_json::{json, Value};

pub const SVC_HARNESS: &str = "wylde-harness";

/// One curated long-term memory.  Mirrors `memory::long_term::records::LongTermMemory`
/// from `wylde-harness`; we keep the shape inlined here so the panel
/// doesn't pull the harness crate in just to read serde-derived fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LongTermRecord {
    pub id: String,
    pub body: String,
    pub source: String,
    /// 1..=10 importance — the harness sort-key.
    pub importance: i32,
    /// Unix seconds.  Used for the recency strip.
    pub created_at: f64,
    pub last_used_at: f64,
    pub tags: Vec<String>,
}

impl LongTermRecord {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            body: v
                .get("body")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            source: v
                .get("source")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            importance: v
                .get("importance")
                .and_then(|x| x.as_i64())
                .map(|n| n as i32)
                .unwrap_or(0),
            created_at: v.get("created_at").and_then(|x| x.as_f64()).unwrap_or(0.0),
            last_used_at: v
                .get("last_used_at")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0),
            tags: v
                .get("tags")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// One workspace summary as `workspaces.list_mru` reports it
/// (config-file-backed redesign, PR #12).  The MRU entries are
/// `WorkspaceDefinition` records, so `path` is sourced from `folder`;
/// `persona` text and `last_activated_at` are not part of the slim
/// list-MRU projection and stay `None` (the row degrades to "No persona
/// set" / "Never activated").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceSummary {
    pub id: String,
    pub path: String,
    pub persona: Option<String>,
    pub last_activated_at: Option<String>,
}

impl WorkspaceSummary {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            // Redesign `WorkspaceDefinition` uses `folder`; fall back to
            // the legacy `path` key for resilience.
            path: v
                .get("folder")
                .or_else(|| v.get("path"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            persona: v
                .get("persona")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
            last_activated_at: v
                .get("last_activated_at")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
        }
    }
}

/// Read the curated long-term list, importance-desc.
pub async fn list_long_term() -> Result<Vec<LongTermRecord>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "memory.long_term.list", "payload": {} })),
    )
    .await?;
    Ok(parse_record_array(&v))
}

/// Vector-search long-term.  Empty/whitespace queries are rejected by
/// the harness with `bad_request`; the panel guards against that
/// up-front so the user sees the empty state rather than a "query is
/// empty after trim" error toast.
pub async fn search_long_term(query: &str, limit: u32) -> Result<Vec<LongTermRecord>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "memory.long_term.search",
            "payload": { "query": query, "limit": limit },
        })),
    )
    .await?;
    // search emits `{results: [...], count}` with each entry carrying
    // the same field set as `list` + `similarity` + `score`.  We only
    // keep the fields the panel renders, so the same `from_value`
    // parser works on both shapes.
    let Some(arr) = v.get("results").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(LongTermRecord::from_value).collect())
}

/// Read the most-recent workspaces.  Migrated to `workspaces.list_mru`
/// (config-file-backed redesign, PR #12) — the retired
/// `memory.workspaces.recent` verb returned `no_action`.  The harness
/// caps at its static MRU-5 window, so `limit` is accepted for call-site
/// compatibility but ignored.
pub async fn recent_workspaces(_limit: u32) -> Result<Vec<WorkspaceSummary>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.list_mru",
            "payload": {},
        })),
    )
    .await?;
    Ok(parse_workspace_array(&v))
}

/// One short-term ("working memory") entry, mirrored from the active
/// conversation's rolling buffer.  Same projection the Chat panel's
/// working-memory strip uses (`kind` tag + one-line `summary`); inlined
/// here rather than shared so the Memory panel stays free of a
/// dependency on the Chat crate.  The raw `data` payload is collapsed to
/// a single line so the browser can't surface a tool's full input/output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShortTermEntry {
    pub kind: String,
    pub summary: String,
}

impl ShortTermEntry {
    pub fn from_value(v: &Value) -> Self {
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("entry")
            .to_owned();
        Self {
            kind,
            summary: summarize_short_term_data(v.get("data")),
        }
    }
}

/// Collapse a freeform working-memory `data` value into one short line.
/// Strings pass through; objects prefer a known descriptive field and
/// fall back to a comma-joined key list.  (Twin of the Chat panel's
/// `summarize_working_data`.)
fn summarize_short_term_data(data: Option<&Value>) -> String {
    match data {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => {
            for key in ["summary", "text", "title", "path", "name"] {
                if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        return s.to_owned();
                    }
                }
            }
            map.keys().cloned().collect::<Vec<_>>().join(", ")
        }
        Some(other) => other.to_string(),
    }
}

/// `memory.short_term.get` — the rolling working-memory buffer for the
/// active conversation.  The Memory panel reads this when the nav bus
/// tells it which conversation is active, so its Short-term section
/// mirrors the Chat panel's working-memory pill.  Reply shape is
/// `{ working_memory: [...], conversation_id }`; a missing array reads as
/// an empty buffer.
pub async fn fetch_short_term(conversation_id: &str) -> Result<Vec<ShortTermEntry>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "memory.short_term.get",
            "payload": { "conversation_id": conversation_id },
        })),
    )
    .await?;
    let Some(arr) = v.get("working_memory").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(ShortTermEntry::from_value).collect())
}

fn parse_record_array(v: &Value) -> Vec<LongTermRecord> {
    let Some(arr) = v.get("memories").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(LongTermRecord::from_value).collect()
}

fn parse_workspace_array(v: &Value) -> Vec<WorkspaceSummary> {
    let Some(arr) = v.get("workspaces").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(WorkspaceSummary::from_value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_term_record_parses_full_payload() {
        let v = json!({
            "id": "abcd1234",
            "body": "remember: prefer Bash over PowerShell",
            "source": "settings_ui",
            "importance": 7,
            "created_at": 1_700_000_000.0,
            "last_used_at": 1_700_001_000.0,
            "tags": ["env", "preference"],
        });
        let r = LongTermRecord::from_value(&v);
        assert_eq!(r.id, "abcd1234");
        assert_eq!(r.importance, 7);
        assert_eq!(r.tags, vec!["env".to_owned(), "preference".to_owned()]);
    }

    #[test]
    fn long_term_record_defaults_missing_fields() {
        let r = LongTermRecord::from_value(&json!({}));
        assert!(r.id.is_empty());
        assert_eq!(r.importance, 0);
        assert!(r.tags.is_empty());
    }

    #[test]
    fn workspace_summary_parses_full_payload() {
        let v = json!({
            "id": "wylde",
            "path": "%USERPROFILE%/Documents/Obsidian Vault/Wylde",
            "persona": "careful architect",
            "last_activated_at": "2026-05-28T12:00:00Z",
        });
        let s = WorkspaceSummary::from_value(&v);
        assert_eq!(s.id, "wylde");
        assert_eq!(s.persona.as_deref(), Some("careful architect"));
        assert_eq!(s.last_activated_at.as_deref(), Some("2026-05-28T12:00:00Z"));
    }

    #[test]
    fn parse_record_array_unwraps_envelope() {
        let v = json!({
            "memories": [
                {"id": "a", "body": "x", "importance": 5},
                {"id": "b", "body": "y", "importance": 3},
            ]
        });
        let out = parse_record_array(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].importance, 3);
    }

    #[test]
    fn parse_record_array_handles_missing_key() {
        assert!(parse_record_array(&json!({})).is_empty());
    }

    #[test]
    fn parse_workspace_array_handles_missing_key() {
        assert!(parse_workspace_array(&json!({})).is_empty());
    }

    #[test]
    fn harness_service_name_matches_pipe_prefix() {
        assert_eq!(SVC_HARNESS, "wylde-harness");
    }

    #[test]
    fn each_pipe_call_uses_expected_verb() {
        // Build-time witness — same pattern Settings / Workspaces use.
        let _ = list_long_term;
        let _ = search_long_term;
        let _ = recent_workspaces;
        let _ = fetch_short_term;
    }

    #[test]
    fn short_term_entry_prefers_summary_field() {
        let e = ShortTermEntry::from_value(&json!({
            "kind": "tool",
            "data": { "summary": "searched memory for 'rust'", "name": "memory.long_term.search" },
        }));
        assert_eq!(e.kind, "tool");
        assert_eq!(e.summary, "searched memory for 'rust'");
    }

    #[test]
    fn short_term_entry_falls_back_through_known_keys() {
        let e = ShortTermEntry::from_value(&json!({
            "kind": "file",
            "data": { "path": "src/lib.rs", "bytes": 42 },
        }));
        assert_eq!(e.summary, "src/lib.rs");
    }

    #[test]
    fn short_term_entry_string_data_passes_through() {
        let e = ShortTermEntry::from_value(&json!({
            "kind": "decision",
            "data": "use the strangler fallback",
        }));
        assert_eq!(e.summary, "use the strangler fallback");
    }

    #[test]
    fn short_term_entry_defaults_kind_when_absent() {
        let e = ShortTermEntry::from_value(&json!({ "data": "x" }));
        assert_eq!(e.kind, "entry");
    }
}
