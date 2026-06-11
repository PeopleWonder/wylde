//! Reflection / consolidation cycle for long-term memory.
//!
//! Rust port of the `long_term` scope of
//! `Core/harness/memory/reflection.py`. Periodically the LLM scans
//! recent low-level memories within a time window and synthesises ONE
//! higher-level insight. The synthesised reflection becomes a new
//! long-term record with importance >= the inputs'; each input is
//! re-linked so its `superseded_by` field points at the reflection,
//! fading the originals from default retrieval but keeping them
//! visible via the Settings history walker.
//!
//! Scope coverage: this module owns the `"long_term"` cycle only. The
//! top-level scope dispatcher — `"workspace:<id>"`,
//! `"conversation:<id>"`, empty / unknown scopes, plus the
//! `memory.reflect` pipe verb itself — lives in
//! [`crate::memory::reflection`] (full-Rust cutover slice R2b), which
//! routes the long_term scope here via [`reflect_long_term`].
//!
//! ## Why the chat function is injected
//!
//! Reflection runs an LLM turn. The harness's chat machinery
//! (`turn::actions`) handles tool-loop bookkeeping the reflection cycle
//! doesn't need; rather than thread the whole turn pipeline through
//! here, callers pass a trait object that takes a messages vec and
//! returns the assistant text. This keeps the module testable (mock
//! implementor in tests, real wylde-ollama client in production).

use std::cmp::max;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::entries::json_path;
use super::{
    delete as long_term_delete, list_records as long_term_list, save as long_term_save,
    update as long_term_update, LongTermMemory,
};

const SECONDS_PER_DAY: f64 = 86_400.0;

/// Default window: synthesise from the last 24 hours of non-superseded,
/// non-reflection memories. Mirrors Python `DEFAULT_WINDOW_DAYS`.
pub const DEFAULT_WINDOW_DAYS: f64 = 1.0;
/// Tag attached to every reflection record so it's filterable later.
/// Mirrors Python `REFLECTION_TAG`.
pub const REFLECTION_TAG: &str = "reflection";
/// Minimum-importance floor for reflections — a synthesis is at least
/// "reflection-worthy" even if the inputs were all importance 1.
pub const REFLECTION_IMPORTANCE_FLOOR: i32 = 7;

/// System message the reflection cycle sends with every consolidation
/// request. B9: resolves through the prompts catalog (`memory.consolidate`,
/// default byte-identical to the old hardcoded const, which was itself
/// verbatim from `reflection.py::_REFLECTION_SYSTEM`) so the Settings
/// prompt editor can tune it without a rebuild.
pub fn reflection_system_prompt() -> String {
    crate::prompts::store::effective_prompt("memory.consolidate")
}

/// Result of one reflection cycle. Same shape as Python
/// `ReflectionResult.to_dict()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionResult {
    pub scope: String,
    pub inputs_considered: usize,
    /// `None` when the cycle was skipped or the model declined.
    pub reflection_id: Option<String>,
    pub reflection_body: String,
    pub superseded_ids: Vec<String>,
    pub skipped: bool,
    pub skip_reason: String,
}

impl ReflectionResult {
    /// A cycle that did nothing, with the reason recorded. Public so
    /// the scope dispatcher ([`crate::memory::reflection`]) and the
    /// scheduler tests can build parity shapes.
    pub fn skipped(scope: impl Into<String>, considered: usize, reason: impl Into<String>) -> Self {
        ReflectionResult {
            scope: scope.into(),
            inputs_considered: considered,
            reflection_id: None,
            reflection_body: String::new(),
            superseded_ids: Vec::new(),
            skipped: true,
            skip_reason: reason.into(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "scope": self.scope,
            "inputs_considered": self.inputs_considered,
            "reflection_id": self.reflection_id,
            "reflection_body": self.reflection_body,
            "superseded_ids": self.superseded_ids,
            "skipped": self.skipped,
            "skip_reason": self.skip_reason,
        })
    }
}

/// Pluggable chat surface for the reflection cycle. Implementors run
/// one LLM turn given a messages list and return the assistant text.
/// On error / refusal, return an empty string — mirrors Python's
/// "log + return empty" semantics in `_ask_model`.
#[async_trait]
pub trait ReflectionChat: Send + Sync {
    async fn ask(&self, messages: Vec<Value>, model: Option<String>) -> String;
}

/// Knobs for one reflection cycle. Mirrors the keyword args on
/// `reflection.py::reflect`. Defaults match the Python module
/// (`window_days = 1.0`, `min_inputs = 3`).
#[derive(Debug, Clone)]
pub struct ReflectOptions {
    pub model: Option<String>,
    pub window_days: f64,
    pub min_inputs: usize,
}

impl Default for ReflectOptions {
    fn default() -> Self {
        ReflectOptions {
            model: None,
            window_days: DEFAULT_WINDOW_DAYS,
            min_inputs: 3,
        }
    }
}

/// Run one long-term consolidation cycle. Routed to by the top-level
/// scope dispatcher, [`crate::memory::reflection::reflect`].
pub async fn reflect_long_term(
    chat: &dyn ReflectionChat,
    opts: ReflectOptions,
) -> ReflectionResult {
    let inputs = select_inputs_long_term(opts.window_days);
    if inputs.len() < opts.min_inputs {
        return ReflectionResult::skipped(
            "long_term",
            inputs.len(),
            format!("need {} inputs, have {}", opts.min_inputs, inputs.len()),
        );
    }

    let inputs_block = format_inputs(&inputs);
    let messages = vec![
        json!({"role": "system", "content": reflection_system_prompt()}),
        json!({"role": "user", "content": inputs_block}),
    ];
    let text = chat.ask(messages, opts.model.clone()).await;
    let text = text.trim().to_owned();
    if text.is_empty() || text.eq_ignore_ascii_case("NOTHING") {
        let reason = if text.is_empty() {
            "model declined: (empty)".to_owned()
        } else {
            format!("model declined: {text}")
        };
        return ReflectionResult::skipped("long_term", inputs.len(), reason);
    }

    let importance = max(
        REFLECTION_IMPORTANCE_FLOOR,
        inputs
            .iter()
            .map(|r| r.importance)
            .max()
            .unwrap_or(REFLECTION_IMPORTANCE_FLOOR),
    );

    let new_record = match long_term_save(
        &text,
        "reflection:long_term",
        Some(importance as f64),
        vec![REFLECTION_TAG.to_owned()],
        None,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("reflection: save failed: {e}");
            return ReflectionResult::skipped(
                "long_term",
                inputs.len(),
                format!("save failed: {e}"),
            );
        }
    };

    let mut superseded_ids: Vec<String> = Vec::new();
    for r in &inputs {
        // Mirror Python: write a no-op update so the API contract holds,
        // delete the redundant copy, then surgically re-link the original
        // to the reflection record.
        if let Some(redundant) = long_term_update(&r.id, Some(&r.body), None, None, None) {
            if let Err(e) = link_supersession(&r.id, &new_record.id) {
                tracing::warn!("reflection: link_supersession failed for {}: {e}", r.id);
                // Continue — original still points at the redundant copy;
                // a future reflection pass will sweep it up.
                continue;
            }
            long_term_delete(&redundant.id);
            superseded_ids.push(r.id.clone());
        }
    }

    ReflectionResult {
        scope: "long_term".to_owned(),
        inputs_considered: inputs.len(),
        reflection_id: Some(new_record.id),
        reflection_body: text,
        superseded_ids,
        skipped: false,
        skip_reason: String::new(),
    }
}

/// Pull recent, non-reflection long-term memories within `window_days`.
/// Public for tests; mirrors `reflection.py::_select_inputs_long_term`.
pub fn select_inputs_long_term(window_days: f64) -> Vec<LongTermMemory> {
    let now = now_secs();
    let cutoff = now - window_days * SECONDS_PER_DAY;
    long_term_list(false)
        .into_iter()
        .filter(|r| r.last_used_at >= cutoff)
        .filter(|r| !r.tags.iter().any(|t| t == REFLECTION_TAG))
        .collect()
}

/// Render an inputs block the model can read. Mirrors Python
/// `_format_inputs` 1:1 (1-indexed numbering, importance shown).
pub fn format_inputs(inputs: &[LongTermMemory]) -> String {
    inputs
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. (importance {}) {}", i + 1, r.importance, r.body))
        .collect::<Vec<_>>()
        .join("\n")
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Surgical re-link: rewrite the long_term.json so `old_id`'s
/// `superseded_by` field points at `new_id`. Mirrors Python
/// `_link_supersession`. Does not touch the vector mirror (the original
/// record's vector is already in place; the JSON edit is the only
/// thing that changes which records surface in `search`).
fn link_supersession(old_id: &str, new_id: &str) -> std::io::Result<()> {
    let path = json_path();
    let raw = std::fs::read_to_string(&path)?;
    let mut value: Value = serde_json::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let memories = value
        .get_mut("memories")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "long_term.json missing 'memories' array",
            )
        })?;
    let mut found = false;
    for item in memories.iter_mut() {
        let id_matches = item
            .get("id")
            .and_then(Value::as_str)
            .map(|s| s == old_id)
            .unwrap_or(false);
        if id_matches {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("superseded_by".to_owned(), Value::String(new_id.to_owned()));
            }
            found = true;
            break;
        }
    }
    if !found {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("record {old_id} not found in long_term.json"),
        ));
    }
    let serialised = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialised)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;
    use std::sync::Mutex;

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    /// Mock that returns a fixed string. Used to drive the long_term
    /// path without an LLM. The interior mutability records call count
    /// for tests that need to assert it.
    struct FixedChat {
        reply: String,
        calls: Mutex<Vec<(Vec<Value>, Option<String>)>>,
    }

    impl FixedChat {
        fn new(reply: impl Into<String>) -> Self {
            FixedChat {
                reply: reply.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ReflectionChat for FixedChat {
        async fn ask(&self, messages: Vec<Value>, model: Option<String>) -> String {
            self.calls.lock().unwrap().push((messages, model));
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn reflect_long_term_skips_when_not_enough_inputs() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // Seed only 1 record — need 3 by default.
        long_term_save("only one", "test", Some(5.0), vec![], None).unwrap();
        let chat = FixedChat::new("anything");
        let r = reflect_long_term(&chat, ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.inputs_considered, 1);
        assert!(r.skip_reason.contains("need 3 inputs"));
    }

    #[tokio::test]
    async fn reflect_long_term_skips_when_model_returns_nothing() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        for i in 0..3 {
            long_term_save(&format!("rec {i}"), "test", Some(5.0), vec![], None).unwrap();
        }
        let chat = FixedChat::new("NOTHING");
        let r = reflect_long_term(&chat, ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.inputs_considered, 3);
        assert!(r.skip_reason.contains("model declined"));
    }

    #[tokio::test]
    async fn reflect_long_term_skips_when_model_returns_empty() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        for i in 0..3 {
            long_term_save(&format!("rec {i}"), "test", Some(5.0), vec![], None).unwrap();
        }
        let chat = FixedChat::new("   ");
        let r = reflect_long_term(&chat, ReflectOptions::default()).await;
        assert!(r.skipped);
        assert!(r.skip_reason.contains("(empty)"));
    }

    #[tokio::test]
    async fn reflect_long_term_synthesises_and_supersedes_inputs() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        let r1 = long_term_save("aaa", "test", Some(4.0), vec![], None).unwrap();
        let r2 = long_term_save("bbb", "test", Some(5.0), vec![], None).unwrap();
        let r3 = long_term_save("ccc", "test", Some(6.0), vec![], None).unwrap();

        let chat = FixedChat::new("a consolidated insight");
        let r = reflect_long_term(&chat, ReflectOptions::default()).await;
        assert!(!r.skipped);
        assert_eq!(r.inputs_considered, 3);
        assert_eq!(r.reflection_body, "a consolidated insight");
        let new_id = r.reflection_id.expect("new record id present");
        assert_eq!(r.superseded_ids.len(), 3);

        // Each original now points at the new record via `superseded_by`.
        for id in [&r1.id, &r2.id, &r3.id] {
            let rec = crate::memory::long_term::get(id).expect("original record still present");
            assert_eq!(rec.superseded_by, new_id);
        }

        // The new record is importance floor (max input was 6, floor is 7).
        let new_rec = crate::memory::long_term::get(&new_id).expect("reflection record present");
        assert_eq!(new_rec.importance, REFLECTION_IMPORTANCE_FLOOR);
        assert!(new_rec.tags.iter().any(|t| t == REFLECTION_TAG));
        assert_eq!(new_rec.source, "reflection:long_term");
    }

    #[tokio::test]
    async fn reflect_long_term_carries_importance_above_floor_when_inputs_demand() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        long_term_save("a", "test", Some(8.0), vec![], None).unwrap();
        long_term_save("b", "test", Some(9.0), vec![], None).unwrap();
        long_term_save("c", "test", Some(10.0), vec![], None).unwrap();

        let chat = FixedChat::new("synthesis");
        let r = reflect_long_term(&chat, ReflectOptions::default()).await;
        assert!(!r.skipped);
        let new_rec = crate::memory::long_term::get(&r.reflection_id.unwrap()).unwrap();
        assert_eq!(new_rec.importance, 10);
    }

    #[tokio::test]
    async fn reflect_long_term_filters_existing_reflections_from_inputs() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // 3 plain records — enough to satisfy min_inputs.
        for i in 0..3 {
            long_term_save(&format!("r{i}"), "test", Some(5.0), vec![], None).unwrap();
        }
        // A prior reflection — must NOT be re-consumed.
        long_term_save(
            "prior synthesis",
            "reflection:long_term",
            Some(8.0),
            vec![REFLECTION_TAG.to_owned()],
            None,
        )
        .unwrap();

        let selected = select_inputs_long_term(DEFAULT_WINDOW_DAYS);
        assert_eq!(selected.len(), 3);
        assert!(selected
            .iter()
            .all(|r| !r.tags.iter().any(|t| t == REFLECTION_TAG)));
    }

    #[tokio::test]
    async fn reflect_long_term_filters_inputs_outside_window() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // One within window.
        long_term_save("recent", "test", Some(5.0), vec![], None).unwrap();

        // Two outside the window — backdate them by editing JSON.
        let r2 = long_term_save("stale_a", "test", Some(5.0), vec![], None).unwrap();
        let r3 = long_term_save("stale_b", "test", Some(5.0), vec![], None).unwrap();
        backdate(&r2.id, now_secs() - 10.0 * SECONDS_PER_DAY);
        backdate(&r3.id, now_secs() - 10.0 * SECONDS_PER_DAY);

        let selected = select_inputs_long_term(DEFAULT_WINDOW_DAYS);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].body, "recent");
    }

    #[tokio::test]
    async fn reflect_long_term_passes_system_prompt_and_user_block_to_model() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        for i in 0..3 {
            long_term_save(&format!("rec{i}"), "test", Some(5.0), vec![], None).unwrap();
        }
        let chat = FixedChat::new("ok");
        let opts = ReflectOptions {
            model: Some("test-model".to_owned()),
            ..Default::default()
        };
        let _ = reflect_long_term(&chat, opts).await;

        let calls = chat.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (messages, model) = &calls[0];
        assert_eq!(model.as_deref(), Some("test-model"));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        // Pinned against the catalog default's key phrases so the B9
        // migration can never silently change the consolidator's contract.
        let sys = messages[0]["content"].as_str().unwrap();
        assert_eq!(sys, reflection_system_prompt());
        assert!(sys.starts_with("You are a memory consolidator.")); // wylde-check: prompt-literal-ok
        assert!(sys.contains("output the literal string NOTHING"));
        assert_eq!(messages[1]["role"], "user");
        let user = messages[1]["content"].as_str().unwrap();
        // Every input appears with a numbered prefix and importance tag.
        // Ordering matches `list_records`: importance-desc, then
        // recency-desc — all three have importance 5 here, so it's
        // newest (rec2) first.
        assert!(
            user.contains("1. (importance 5) rec2"),
            "user block missing line 1: {user}"
        );
        assert!(user.contains("2. (importance 5) rec1"));
        assert!(user.contains("3. (importance 5) rec0"));
    }

    #[tokio::test]
    async fn format_inputs_matches_python_one_indexed_shape() {
        let inputs = vec![
            LongTermMemory {
                id: "id1".to_owned(),
                body: "alpha".to_owned(),
                source: "".to_owned(),
                importance: 5,
                created_at: 0.0,
                last_used_at: 0.0,
                superseded_by: "".to_owned(),
                tags: vec![],
            },
            LongTermMemory {
                id: "id2".to_owned(),
                body: "beta".to_owned(),
                source: "".to_owned(),
                importance: 8,
                created_at: 0.0,
                last_used_at: 0.0,
                superseded_by: "".to_owned(),
                tags: vec![],
            },
        ];
        let block = format_inputs(&inputs);
        assert_eq!(block, "1. (importance 5) alpha\n2. (importance 8) beta",);
    }

    #[tokio::test]
    async fn reflection_result_to_value_includes_every_field() {
        let r = ReflectionResult {
            scope: "long_term".to_owned(),
            inputs_considered: 3,
            reflection_id: Some("rid".to_owned()),
            reflection_body: "body".to_owned(),
            superseded_ids: vec!["a".to_owned(), "b".to_owned()],
            skipped: false,
            skip_reason: String::new(),
        };
        let v = r.to_value();
        assert_eq!(v["scope"], "long_term");
        assert_eq!(v["inputs_considered"], 3);
        assert_eq!(v["reflection_id"], "rid");
        assert_eq!(v["reflection_body"], "body");
        assert_eq!(v["superseded_ids"], json!(["a", "b"]));
        assert_eq!(v["skipped"], false);
    }

    /// Edit `last_used_at` on a record directly in the JSON store —
    /// the public `touch` only bumps to now. Mirrors the test helper
    /// Python uses to backdate records for window tests.
    fn backdate(record_id: &str, new_ts: f64) {
        let path = json_path();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut v: Value = serde_json::from_str(&raw).unwrap();
        for r in v["memories"].as_array_mut().unwrap() {
            if r["id"].as_str() == Some(record_id) {
                r["last_used_at"] = json!(new_ts);
            }
        }
        let pretty = serde_json::to_string_pretty(&v).unwrap();
        std::fs::write(&path, pretty).unwrap();
    }
}
