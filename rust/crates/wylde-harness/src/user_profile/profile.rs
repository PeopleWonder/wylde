//! The `UserProfile` data model + the LLM-proposal value types.
//!
//! Thought Bubble System Slice D. The user profile is the global,
//! user-level memory the assistant reads every turn — the harness's
//! answer to "who am I talking to, and how do they want me to behave."
//! It is the **one** context slot that is never evicted under token
//! pressure (Plan v2 §9.1 / OI-8).
//!
//! ## Shape (Plan v2 §6 Slice D / Build Order Appendix B)
//!
//! * `name` — what the user wants to be called.
//! * `preferences` — free key→value pairs (e.g. `communication_style` →
//!   `terse`). A `BTreeMap` (not the spec's `HashMap`) so the on-disk
//!   JSON and every test assertion are deterministically ordered.
//! * `recurring_topics` — subjects that keep coming up.
//! * `style` — a one-line free-text behavioural note.
//! * `free_text_rules` — the Cursor-Rules-like dump: user-authored text
//!   the assistant is told to follow verbatim. The editable centrepiece
//!   of the Settings "Profile / Rules" page.
//!
//! ## Edit policy — user-edit-wins-always (OI-18)
//!
//! [`UserProfile::apply_patch`] is the single mutation path for *user*
//! edits and applies them unconditionally. LLM-proposed changes never
//! mutate the profile directly; they enter the pending queue as a
//! [`ProfileProposal`] and only land through [`apply_proposal`] after
//! the user accepts (which is itself a user edit). There is no merge
//! conflict to resolve — the user is always authoritative.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The global, user-level profile read into every turn's system prompt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UserProfile {
    /// What the user wants to be called. `None` until set.
    #[serde(default)]
    pub name: Option<String>,
    /// Free key→value preferences. `BTreeMap` for stable JSON ordering.
    #[serde(default)]
    pub preferences: std::collections::BTreeMap<String, String>,
    /// Subjects that recur across conversations.
    #[serde(default)]
    pub recurring_topics: Vec<String>,
    /// One-line behavioural note (e.g. "prefers direct answers").
    #[serde(default)]
    pub style: Option<String>,
    /// The Cursor-Rules-like dump the assistant follows verbatim.
    #[serde(default)]
    pub free_text_rules: String,
}

impl UserProfile {
    /// Apply a user edit (OI-18 — user-edit-wins, applied
    /// unconditionally). `patch` is a JSON object carrying any subset of
    /// the profile's fields; absent keys are left untouched.
    ///
    /// Per-field semantics:
    /// * `name` / `style` — a string sets it; `null` or `""` clears it.
    /// * `free_text_rules` — a string replaces it wholesale.
    /// * `recurring_topics` — an array of strings replaces the list.
    /// * `preferences` — an object *merges* key-by-key: a string value
    ///   sets/overwrites the key, a `null` value removes it. (Merge, not
    ///   replace, so editing one preference doesn't drop the rest.)
    pub fn apply_patch(&mut self, patch: &Map<String, Value>) {
        if let Some(v) = patch.get("name") {
            self.name = string_or_clear(v);
        }
        if let Some(v) = patch.get("style") {
            self.style = string_or_clear(v);
        }
        if let Some(v) = patch.get("free_text_rules").and_then(Value::as_str) {
            self.free_text_rules = v.to_owned();
        }
        if let Some(arr) = patch.get("recurring_topics").and_then(Value::as_array) {
            self.recurring_topics = arr
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if let Some(obj) = patch.get("preferences").and_then(Value::as_object) {
            for (key, val) in obj {
                match val {
                    Value::Null => {
                        self.preferences.remove(key);
                    }
                    Value::String(s) => {
                        self.preferences.insert(key.clone(), s.clone());
                    }
                    // Non-string, non-null values are ignored — preferences
                    // are a string→string map by contract.
                    _ => {}
                }
            }
        }
    }

    /// Render the profile as the system-prompt block the turn driver
    /// injects. Empty when nothing is set, so an unconfigured profile
    /// costs zero prompt tokens. (Consumed by Slice G; provided here so
    /// the data model owns its own rendering.)
    pub fn to_prompt_block(&self) -> String {
        let mut lines = Vec::new();
        if let Some(name) = self.name.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("Name: {name}"));
        }
        if let Some(style) = self.style.as_deref().filter(|s| !s.is_empty()) {
            lines.push(format!("Style: {style}"));
        }
        for (k, v) in &self.preferences {
            lines.push(format!("Preference — {k}: {v}"));
        }
        if !self.recurring_topics.is_empty() {
            lines.push(format!(
                "Recurring topics: {}",
                self.recurring_topics.join(", ")
            ));
        }
        let rules = self.free_text_rules.trim();
        if !rules.is_empty() {
            lines.push(format!("User rules (follow verbatim):\n{rules}"));
        }
        lines.join("\n")
    }
}

/// `null`/empty → `None`; a non-empty string → `Some`.
fn string_or_clear(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned).filter(|s| !s.is_empty())
}

// ── LLM proposals ─────────────────────────────────────────────────────

/// An LLM-proposed change to one profile field, awaiting the user's
/// accept / edit / reject. Proposals never mutate the profile on their
/// own (OI-18); they sit in [`crate::user_profile::store::ProfileStore::pending`]
/// until resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileProposal {
    /// Stable id (`uuid` v4 hex) the accept/reject verbs key on.
    pub id: String,
    /// Which field this targets. One of:
    /// `"name"`, `"style"`, `"free_text_rules"`, `"recurring_topic"`, or
    /// `"preference:<key>"` (the preference key after the colon).
    pub field: String,
    /// The value the LLM proposes (for a preference, just the value —
    /// the key lives in `field`).
    pub proposed: String,
    /// Snapshot of the current value at propose time, for the accept-UI
    /// diff (OI-18 — proposals shown as a diff). `None` when the field
    /// was unset.
    #[serde(default)]
    pub current: Option<String>,
    /// Why the LLM proposed it — shown in the accept UI.
    #[serde(default)]
    pub rationale: String,
    /// Model confidence in `[0,1]`. Gated at `>= 0.7` (OI-7).
    pub confidence: f64,
    /// The conversation that produced it — drives the per-conversation
    /// proposal quota (OI-7). `None` for out-of-band proposals.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Unix seconds the proposal was minted — drives the per-field
    /// cooldown (OI-7).
    pub created_at: i64,
}

/// A record of a rejected proposal, kept so the same suggestion is
/// suppressed for a window (OI-11 — rejected proposals suppress for 30
/// days). Matched on `(field, proposed)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedRecord {
    pub field: String,
    pub proposed: String,
    /// Unix seconds the rejection happened.
    pub rejected_at: i64,
}

/// Apply an *accepted* proposal to the profile. This is the only way a
/// proposal mutates the profile, and only ever after the user accepts
/// (Plan v2 §4.6 / OI-18). Unknown `field` shapes are ignored rather
/// than erroring, so a forward-compatible proposal can't corrupt state.
pub fn apply_proposal(profile: &mut UserProfile, p: &ProfileProposal) {
    match p.field.as_str() {
        "name" => profile.name = non_empty(&p.proposed),
        "style" => profile.style = non_empty(&p.proposed),
        "free_text_rules" => profile.free_text_rules = p.proposed.clone(),
        "recurring_topic" => {
            if !p.proposed.is_empty() && !profile.recurring_topics.contains(&p.proposed) {
                profile.recurring_topics.push(p.proposed.clone());
            }
        }
        other => {
            if let Some(key) = other.strip_prefix("preference:") {
                if !key.is_empty() {
                    profile
                        .preferences
                        .insert(key.to_owned(), p.proposed.clone());
                }
            }
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn round_trips_through_json() {
        let mut p = UserProfile::default();
        p.name = Some("Aaron".into());
        p.preferences
            .insert("communication_style".into(), "terse".into());
        p.recurring_topics.push("rust".into());
        p.style = Some("direct, no hedging".into());
        p.free_text_rules = "Always show me the diff.".into();

        let s = serde_json::to_string(&p).unwrap();
        let back: UserProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn default_deserialises_from_empty_object() {
        let p: UserProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(p, UserProfile::default());
    }

    #[test]
    fn apply_patch_sets_and_clears_scalar_fields() {
        let mut p = UserProfile::default();
        p.apply_patch(&patch(json!({"name": "Aaron", "style": "terse"})));
        assert_eq!(p.name.as_deref(), Some("Aaron"));
        assert_eq!(p.style.as_deref(), Some("terse"));

        // null clears; "" clears.
        p.apply_patch(&patch(json!({"name": null, "style": ""})));
        assert_eq!(p.name, None);
        assert_eq!(p.style, None);
    }

    #[test]
    fn apply_patch_merges_preferences_key_by_key() {
        let mut p = UserProfile::default();
        p.apply_patch(&patch(json!({"preferences": {"a": "1", "b": "2"}})));
        // Editing one key leaves the rest; null removes a key.
        p.apply_patch(&patch(
            json!({"preferences": {"a": "9", "b": null, "c": "3"}}),
        ));
        assert_eq!(p.preferences.get("a").map(String::as_str), Some("9"));
        assert!(!p.preferences.contains_key("b"));
        assert_eq!(p.preferences.get("c").map(String::as_str), Some("3"));
    }

    #[test]
    fn apply_patch_replaces_recurring_topics_and_rules() {
        let mut p = UserProfile::default();
        p.apply_patch(&patch(json!({"recurring_topics": ["rust", "", "gpui"]})));
        assert_eq!(p.recurring_topics, vec!["rust", "gpui"]); // empties dropped
        p.apply_patch(&patch(json!({"recurring_topics": ["only"]})));
        assert_eq!(p.recurring_topics, vec!["only"]); // replace, not append

        p.apply_patch(&patch(json!({"free_text_rules": "Rule one."})));
        assert_eq!(p.free_text_rules, "Rule one.");
    }

    #[test]
    fn apply_patch_ignores_unknown_keys_and_wrong_types() {
        let mut p = UserProfile::default();
        p.name = Some("keep".into());
        p.apply_patch(&patch(json!({"unknown": "x", "name": 42})));
        // wrong-typed name → `as_str()` None → cleared; unknown ignored.
        assert_eq!(p.name, None);
    }

    #[test]
    fn apply_proposal_routes_each_field_kind() {
        let mut p = UserProfile::default();
        let mk = |field: &str, proposed: &str| ProfileProposal {
            id: "x".into(),
            field: field.into(),
            proposed: proposed.into(),
            current: None,
            rationale: String::new(),
            confidence: 1.0,
            conversation_id: None,
            created_at: 0,
        };
        apply_proposal(&mut p, &mk("name", "Aaron"));
        apply_proposal(&mut p, &mk("style", "terse"));
        apply_proposal(&mut p, &mk("free_text_rules", "Show diffs."));
        apply_proposal(&mut p, &mk("recurring_topic", "rust"));
        apply_proposal(&mut p, &mk("recurring_topic", "rust")); // dedup
        apply_proposal(&mut p, &mk("preference:tone", "dry"));
        apply_proposal(&mut p, &mk("bogus_field", "ignored"));

        assert_eq!(p.name.as_deref(), Some("Aaron"));
        assert_eq!(p.style.as_deref(), Some("terse"));
        assert_eq!(p.free_text_rules, "Show diffs.");
        assert_eq!(p.recurring_topics, vec!["rust"]);
        assert_eq!(p.preferences.get("tone").map(String::as_str), Some("dry"));
    }

    #[test]
    fn prompt_block_is_empty_when_unset_and_populated_otherwise() {
        assert!(UserProfile::default().to_prompt_block().is_empty());
        let mut p = UserProfile::default();
        p.name = Some("Aaron".into());
        p.free_text_rules = "Be terse.".into();
        let block = p.to_prompt_block();
        assert!(block.contains("Name: Aaron"));
        assert!(block.contains("Be terse."));
    }
}
