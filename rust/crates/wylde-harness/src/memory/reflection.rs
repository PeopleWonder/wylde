//! `memory.reflect` — scope dispatcher for the reflection /
//! consolidation cycles. Rust port of the dispatch + conversation
//! halves of `Core/harness/memory/reflection.py` (full-Rust cutover
//! slice R2b).
//!
//! ## Scope map
//!
//! * `"long_term"`          → [`crate::memory::long_term::reflection::reflect_long_term`]
//!   (ported in an earlier slice; this module only routes to it).
//! * `"conversation:<id>"`  → [`reflect_conversation`] — NEW in this
//!   slice. Distils the conversation's working-memory breadcrumbs into
//!   one durable insight, lands it in workspace memory (when the
//!   conversation is bound to a workspace) or long-term memory
//!   (otherwise), then stamps the consumed entries `superseded_by` so
//!   the next turn's prompt sees the synthesis instead of the raw
//!   breadcrumbs. Faithful port of `_reflect_conversation`.
//! * `"workspace:<id>"`     → [`crate::memory::workspace::curate_with_chat`].
//!   **Deliberate divergence from Python**: the Python `_reflect_workspace`
//!   synthesis pass duplicated what `_curate.py` already did better
//!   (keep / supersede / merge verdicts instead of one blanket
//!   synthesis), and its input selection leaned on the LanceDB mirror
//!   the Rust store doesn't keep. Per the R2b slice decision the
//!   workspace scope runs the curation pass (now fully ported — see
//!   [`crate::memory::workspace`]) and maps the [`CurationResult`]
//!   into the `ReflectionResult` wire shape (`superseded_ids` carries
//!   the superseded + merged-away record ids; `reflection_id` /
//!   `reflection_body` stay empty because curation may write several
//!   merge records, not one synthesis).
//!
//! Empty / unknown scopes and the missing-chat-fn case return the same
//! skipped [`ReflectionResult`] shapes Python returned, field-for-field.
//!
//! ## The pipe verb actually reflects (improvement over Python)
//!
//! Python's `_memory_reflect_action` always answered the skipped
//! `"no chat_fn supplied"` result because a chat function can't cross
//! the wire. The Rust crate, however, already has a cheap in-process
//! chat path — the same unary `ollama.chat` IPC hop
//! `chat::search::summary::generate_summary` uses — so
//! [`handle_reflect`] wires [`OllamaReflectionChat`] in and the verb
//! runs a real cycle whenever a chat model is resolvable
//! (active-model pick → starred default → `WYLDE_DEFAULT_MODEL`).
//! When no model is resolvable the verb answers Python's exact
//! `skipped: "no chat_fn supplied"` parity shape, so the no-LLM wire
//! behaviour is byte-identical.
//!
//! ## Known prompt-only divergences (documented, accepted)
//!
//! `format_conversation_inputs` renders dict-shaped working-memory
//! entries as a `k=v` strip. Python iterated keys in JSON insertion
//! order and rendered values with `str()`; serde_json's map is
//! key-sorted and non-string scalars render as JSON (`true` not
//! `True`). Both only affect the synthesis prompt text, never a wire
//! shape or a stored field.

use async_trait::async_trait;
use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::memory::conversations::store as conversations_store;
use crate::memory::long_term;
use crate::memory::long_term::reflection::{
    reflect_long_term, ReflectOptions, ReflectionChat, ReflectionResult, REFLECTION_SYSTEM_PROMPT,
    REFLECTION_TAG,
};
use crate::memory::short_term::store as short_term_store;
use crate::memory::workspace;
use crate::model_registry::model_state;

/// Importance assigned to a conversation synthesis. Working-memory
/// entries carry no importance of their own, so Python pinned the
/// "reflection-worthy floor" — 7 — and we mirror it.
pub const CONVERSATION_REFLECTION_IMPORTANCE: f64 = 7.0;

/// Run one consolidation cycle for `scope`. Mirrors
/// `reflection.py::reflect` exactly:
///
/// 1. blank scope            → skipped `"empty scope"` (scope echoed as `""`).
/// 2. no chat fn             → skipped `"no chat_fn supplied"`.
/// 3. `long_term`            → the ported long-term cycle.
/// 4. `workspace:<id>`       → curation pass (see module docs).
/// 5. `conversation:<id>`    → conversation working-memory synthesis.
/// 6. anything else          → skipped `unknown scope "<scope>"`.
pub async fn reflect(
    scope: &str,
    chat: Option<&dyn ReflectionChat>,
    opts: ReflectOptions,
) -> ReflectionResult {
    let scope = scope.trim();
    if scope.is_empty() {
        return ReflectionResult::skipped("", 0, "empty scope");
    }
    let Some(chat) = chat else {
        return ReflectionResult::skipped(scope, 0, "no chat_fn supplied");
    };
    if scope == "long_term" {
        return reflect_long_term(chat, opts).await;
    }
    if let Some(ws_id) = scope.strip_prefix("workspace:") {
        return reflect_workspace_via_curation(ws_id, chat, opts.model.clone()).await;
    }
    if let Some(conv_id) = scope.strip_prefix("conversation:") {
        return reflect_conversation(conv_id, chat, &opts).await;
    }
    ReflectionResult::skipped(scope, 0, format!("unknown scope {scope:?}"))
}

// ── Workspace scope → curation (see module docs for the divergence) ───

/// Run the workspace curation pass and fold its [`workspace::CurationResult`]
/// into the `ReflectionResult` wire shape. `superseded_ids` carries every
/// record id curation retired (supersede verdicts + merged-away
/// originals, deduplicated in first-seen order).
async fn reflect_workspace_via_curation(
    workspace_id: &str,
    chat: &dyn ReflectionChat,
    model: Option<String>,
) -> ReflectionResult {
    let scope = format!("workspace:{workspace_id}");
    let cur = workspace::curate_with_options(
        workspace_id,
        Some(chat),
        model,
        workspace::CURATION_BATCH_SIZE,
    )
    .await;

    let mut superseded_ids: Vec<String> = Vec::new();
    let mut push_unique = |id: &str| {
        if !id.is_empty() && !superseded_ids.iter().any(|s| s == id) {
            superseded_ids.push(id.to_owned());
        }
    };
    for s in &cur.superseded {
        if let Some(id) = s.get("old_id").and_then(Value::as_str) {
            push_unique(id);
        }
    }
    for m in &cur.merged {
        if let Some(old_ids) = m.get("old_ids").and_then(Value::as_array) {
            for id in old_ids.iter().filter_map(Value::as_str) {
                push_unique(id);
            }
        }
    }

    ReflectionResult {
        scope,
        inputs_considered: cur.inputs_considered,
        reflection_id: None,
        reflection_body: String::new(),
        superseded_ids,
        skipped: cur.skipped,
        skip_reason: cur.skip_reason,
    }
}

// ── Conversation scope (port of `_reflect_conversation`) ──────────────

/// Distil one conversation's working memory into a durable insight.
async fn reflect_conversation(
    conversation_id: &str,
    chat: &dyn ReflectionChat,
    opts: &ReflectOptions,
) -> ReflectionResult {
    let scope = format!("conversation:{conversation_id}");
    if conversation_id.is_empty() {
        return ReflectionResult::skipped(scope, 0, "empty conversation_id");
    }

    let inputs = select_inputs_conversation(conversation_id);
    if inputs.len() < opts.min_inputs {
        return ReflectionResult::skipped(
            scope,
            inputs.len(),
            format!("need {} inputs, have {}", opts.min_inputs, inputs.len()),
        );
    }

    let inputs_block = format_conversation_inputs(&inputs);
    let messages = vec![
        json!({"role": "system", "content": REFLECTION_SYSTEM_PROMPT}),
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
        return ReflectionResult::skipped(scope, inputs.len(), reason);
    }

    // Resolve where the synthesis lands: the bound workspace's memory
    // tier when the conversation carries a workspace_id, the global
    // long-term tier otherwise. The binding is trusted as-is (no
    // registry existence check) — same as Python post the
    // config-file-backed workspaces redesign.
    let target_workspace = conversation_workspace(conversation_id);
    let source = format!("reflection:conversation:{conversation_id}");

    let new_id = if let Some(ws_id) = &target_workspace {
        match workspace::save_new(
            ws_id,
            &text,
            &source,
            Some(CONVERSATION_REFLECTION_IMPORTANCE),
            Vec::new(),
        ) {
            Ok(r) => r.id,
            Err(e) => {
                tracing::warn!("reflection: workspace save failed: {e}");
                return ReflectionResult::skipped(
                    scope,
                    inputs.len(),
                    format!("save failed: {e}"),
                );
            }
        }
    } else {
        match long_term::save(
            &text,
            &source,
            Some(CONVERSATION_REFLECTION_IMPORTANCE),
            vec![REFLECTION_TAG.to_owned()],
            None,
        ) {
            Ok(r) => r.id,
            Err(e) => {
                tracing::warn!("reflection: long_term save failed: {e}");
                return ReflectionResult::skipped(
                    scope,
                    inputs.len(),
                    format!("save failed: {e}"),
                );
            }
        }
    };

    // Stamp the consumed working-memory entries as superseded so the
    // chat-turn driver's short-term slot stops surfacing them. The
    // entries remain on disk for audit / history; just hidden from
    // default reads.
    supersede_working_memory(conversation_id, &inputs, &new_id);

    // Working-memory entries don't have stable ids — surface a
    // signature instead so callers can correlate which inputs we
    // consumed without inventing fake ids. Mirrors Python's
    // `wm:<conversation_id>:<index>` shape.
    let superseded_ids = (0..inputs.len())
        .map(|i| format!("wm:{conversation_id}:{i}"))
        .collect();

    ReflectionResult {
        scope,
        inputs_considered: inputs.len(),
        reflection_id: Some(new_id),
        reflection_body: text,
        superseded_ids,
        skipped: false,
        skip_reason: String::new(),
    }
}

/// Pull non-superseded working-memory entries from the conversation.
/// Mirrors `_select_inputs_conversation` (read errors fold to empty);
/// non-object entries are skipped — Python assumed dict entries and
/// would have crashed on anything else, so dropping them is the safe
/// translation.
pub fn select_inputs_conversation(conversation_id: &str) -> Vec<Value> {
    let entries = match short_term_store::get_working_memory(conversation_id) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    entries
        .into_iter()
        .filter(|e| e.is_object())
        .filter(|e| !is_truthy(e.get("superseded_by")))
        .collect()
}

/// Render working-memory entries the way `_format_conversation_inputs`
/// did — one numbered line per entry, tagged with the entry kind.
/// (See module docs for the two prompt-only divergences.)
pub fn format_conversation_inputs(entries: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(entries.len());
    for (idx, e) in entries.iter().enumerate() {
        let i = idx + 1;
        let kind = e
            .get("kind")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("raw");
        let data = e.get("data").unwrap_or(&Value::Null);
        if let Some(map) = data.as_object() {
            // Render a tool entry as "ran tool X" and other dicts as a
            // compact key=value strip — the synthesis prompt cares
            // about the gist, not the JSON shape.
            if kind == "tool" {
                let name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?");
                lines.push(format!("{i}. ({kind}) ran tool {name}"));
            } else {
                let bits: Vec<String> = map
                    .iter()
                    .take(4)
                    .map(|(k, v)| format!("{k}={}", truncate_chars(&scalar_text(v), 80)))
                    .collect();
                lines.push(format!("{i}. ({kind}) {}", bits.join(", ")));
            }
        } else {
            let text = if data.is_null() {
                String::new()
            } else {
                scalar_text(data)
            };
            lines.push(format!("{i}. ({kind}) {}", truncate_chars(&text, 200)));
        }
    }
    lines.join("\n")
}

/// The conversation's bound workspace id, or `None` when unbound /
/// unreadable. Mirrors `_conversation_target` (errors fold to the
/// long-term target).
fn conversation_workspace(conversation_id: &str) -> Option<String> {
    let doc = conversations_store::read_conversation(conversation_id).ok()?;
    doc.get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Mark each entry in `consumed` as superseded by `reflection_id`.
/// Working-memory entries are dicts without stable ids, so matching is
/// by a coarse signature (kind + at + JSON-stable data) — exactly the
/// approach Python's `_supersede_working_memory` took. Best-effort:
/// a failed rewrite logs and leaves the breadcrumbs visible.
fn supersede_working_memory(conversation_id: &str, consumed: &[Value], reflection_id: &str) {
    let existing = match short_term_store::get_working_memory(conversation_id) {
        Ok(v) => v,
        Err(_) => return,
    };
    if existing.is_empty() {
        return;
    }
    let consumed_signatures: Vec<String> = consumed.iter().map(entry_signature).collect();
    let updated: Vec<Value> = existing
        .into_iter()
        .map(|entry| {
            if !entry.is_object() {
                return entry;
            }
            if is_truthy(entry.get("superseded_by")) {
                return entry;
            }
            if consumed_signatures.contains(&entry_signature(&entry)) {
                let mut e = entry;
                if let Some(obj) = e.as_object_mut() {
                    obj.insert(
                        "superseded_by".to_owned(),
                        Value::String(reflection_id.to_owned()),
                    );
                }
                e
            } else {
                entry
            }
        })
        .collect();
    if let Err(e) = short_term_store::replace_working_memory(conversation_id, updated) {
        tracing::warn!(
            "reflection: working-memory rewrite failed for {conversation_id}: {e:?}"
        );
    }
}

/// Coarse fingerprint for matching working-memory entries before /
/// after the supersession write: kind + `at` + a JSON-stable string of
/// `data` (serde_json maps are key-sorted, matching Python's
/// `sort_keys=True`). Signatures are only ever compared against each
/// other within this module, so the exact rendering needn't match
/// Python's — only be stable.
fn entry_signature(entry: &Value) -> String {
    let kind = entry.get("kind").cloned().unwrap_or(Value::Null);
    let at = entry.get("at").cloned().unwrap_or(Value::Null);
    let data_repr = entry
        .get("data")
        .map(|d| serde_json::to_string(d).unwrap_or_else(|_| d.to_string()))
        .unwrap_or_else(|| "null".to_owned());
    serde_json::to_string(&json!([kind, at, data_repr]))
        .unwrap_or_else(|_| format!("{kind:?}|{at:?}|{data_repr}"))
}

/// Python truthiness over a JSON value — `entry.get("superseded_by")`
/// guards used `if not e.get(...)` semantics.
fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Render one JSON value the way the prompt wants it: strings pass
/// through unquoted, everything else as compact JSON.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Char-boundary-safe prefix (Python sliced by characters).
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ── Production chat implementor ────────────────────────────────────────

/// Production [`ReflectionChat`]: one unary `ollama.chat` IPC hop to
/// the wylde-ollama service — the same path
/// `chat::search::summary::generate_summary` uses. On any failure
/// (no model resolvable, service down, empty reply) it returns an
/// empty string, which the reflection / curation cycles treat as
/// "model declined" — fail-soft, mirroring Python's `_ask_model`
/// log-and-return-empty semantics.
pub struct OllamaReflectionChat;

#[async_trait]
impl ReflectionChat for OllamaReflectionChat {
    async fn ask(&self, messages: Vec<Value>, model: Option<String>) -> String {
        let Some(model) = model.or_else(resolve_chat_model) else {
            tracing::warn!("reflection: no chat model resolvable; skipping LLM call");
            return String::new();
        };
        let cfg = crate::config::Config::get();
        let body = json!({
            "model": model,
            "messages": messages,
            "priority": cfg.default_chat_priority,
            "stream": false,
            "keep_alive": "24h",
        });
        match wylde_shared::ipc::call_action(&cfg.ollama_service, "ollama.chat", body).await {
            Ok(upstream) => upstream
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Err(e) => {
                tracing::warn!("reflection: ollama.chat failed: {}: {}", e.code, e.message);
                String::new()
            }
        }
    }
}

/// The model a reflection turn should use when the caller doesn't pass
/// one: active inference-bar pick → starred default →
/// `WYLDE_DEFAULT_MODEL` env → `None`. Same chain as
/// `models.get_effective`.
pub fn resolve_chat_model() -> Option<String> {
    model_state::get_active_model().or_else(model_state::get_default_model)
}

/// `Some(OllamaReflectionChat)` when a chat model is resolvable right
/// now, `None` otherwise. The pipe handler and the scheduler both use
/// this to decide between a real cycle and Python's skipped-parity
/// reply.
pub fn production_chat() -> Option<OllamaReflectionChat> {
    resolve_chat_model().map(|_| OllamaReflectionChat)
}

// ── Pipe action ────────────────────────────────────────────────────────

/// `memory.reflect` — run one consolidation cycle. Payload `{scope}`
/// where scope is `"long_term"` | `"workspace:<id>"` |
/// `"conversation:<id>"`. Returns the `ReflectionResult` dict,
/// field-for-field as Python's `_memory_reflect_action` did — except
/// that when a chat model is resolvable the cycle actually runs (see
/// module docs); without one the reply is Python's exact
/// `skipped: "no chat_fn supplied"`.
pub async fn handle_reflect(payload: Value) -> Reply {
    let Some(scope) = crate::api::require_string(&payload, "scope") else {
        return Reply::err_msg("bad_request", "scope is required");
    };
    let result = match production_chat() {
        Some(chat) => reflect(&scope, Some(&chat), ReflectOptions::default()).await,
        None => reflect(&scope, None, ReflectOptions::default()).await,
    };
    Reply::ok(result.to_value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::long_term::test_support::TestEnv;
    use std::sync::Mutex;

    fn set_embed_dim_3() {
        std::env::set_var("WYLDE_EMBED_DIM", "3");
    }

    /// Mock chat returning a fixed string, recording calls.
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

    fn seed_working_memory(cid: &str, n: usize) {
        for i in 0..n {
            short_term_store::append_working_memory(
                cid,
                json!({"kind": "decision", "at": 100 + i as i64, "data": format!("decided thing {i}")}),
            )
            .unwrap();
        }
    }

    // ── scope dispatch ───────────────────────────────────────────────

    #[tokio::test]
    async fn empty_scope_is_skipped_even_with_chat() {
        let _env = TestEnv::new();
        let chat = FixedChat::new("x");
        let r = reflect("   ", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "empty scope");
        assert_eq!(r.scope, "");
    }

    #[tokio::test]
    async fn missing_chat_fn_is_skipped_with_python_reason() {
        let _env = TestEnv::new();
        for scope in ["long_term", "workspace:w1", "conversation:c1", "bogus"] {
            let r = reflect(scope, None, ReflectOptions::default()).await;
            assert!(r.skipped, "{scope} should skip without a chat fn");
            assert_eq!(r.skip_reason, "no chat_fn supplied");
            assert_eq!(r.scope, scope);
            assert_eq!(r.inputs_considered, 0);
        }
    }

    #[tokio::test]
    async fn unknown_scope_is_skipped() {
        let _env = TestEnv::new();
        let chat = FixedChat::new("x");
        let r = reflect("nonsense", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert!(r.skip_reason.contains("unknown scope"));
        assert_eq!(r.scope, "nonsense");
    }

    #[tokio::test]
    async fn long_term_scope_routes_to_ported_cycle() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        // No records seeded — the long_term cycle skips with its
        // min-inputs message, proving the route landed there.
        let chat = FixedChat::new("x");
        let r = reflect("long_term", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.scope, "long_term");
        assert!(r.skip_reason.contains("need 3 inputs"));
    }

    // ── workspace scope → curation mapping ──────────────────────────

    #[tokio::test]
    async fn workspace_scope_with_empty_id_maps_curation_skip() {
        let _env = TestEnv::new();
        let chat = FixedChat::new("x");
        let r = reflect("workspace:", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "empty workspace_id");
        assert_eq!(r.scope, "workspace:");
    }

    #[tokio::test]
    async fn workspace_scope_runs_curation_and_maps_superseded_ids() {
        let _env = TestEnv::new();
        let a = workspace::save_new("wsr", "alpha", "t", Some(9.0), vec![]).unwrap();
        let _b = workspace::save_new("wsr", "beta", "t", Some(8.0), vec![]).unwrap();
        // index 1 = alpha (importance 9), index 2 = beta.
        let chat = FixedChat::new(
            "{\"index\": 1, \"verdict\": \"supersede\", \"reason\": \"stale\"}\n\
             {\"index\": 2, \"verdict\": \"keep\"}",
        );
        let r = reflect("workspace:wsr", Some(&chat), ReflectOptions::default()).await;
        assert!(!r.skipped, "curation ran: {}", r.skip_reason);
        assert_eq!(r.scope, "workspace:wsr");
        assert_eq!(r.inputs_considered, 2);
        assert_eq!(r.superseded_ids, vec![a.id.clone()]);
        assert_eq!(r.reflection_id, None);
        assert_eq!(r.reflection_body, "");
    }

    // ── conversation scope ───────────────────────────────────────────

    #[tokio::test]
    async fn conversation_scope_with_empty_id_is_skipped() {
        let _env = TestEnv::new();
        let chat = FixedChat::new("x");
        let r = reflect("conversation:", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.skip_reason, "empty conversation_id");
        assert_eq!(r.scope, "conversation:");
    }

    #[tokio::test]
    async fn conversation_skips_below_min_inputs() {
        let _env = TestEnv::new();
        seed_working_memory("conv1", 2);
        let chat = FixedChat::new("x");
        let r = reflect("conversation:conv1", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert_eq!(r.inputs_considered, 2);
        assert!(r.skip_reason.contains("need 3 inputs, have 2"));
        assert!(chat.calls.lock().unwrap().is_empty(), "no LLM call below the gate");
    }

    #[tokio::test]
    async fn conversation_skips_when_model_declines() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        seed_working_memory("conv2", 3);
        let chat = FixedChat::new("NOTHING");
        let r = reflect("conversation:conv2", Some(&chat), ReflectOptions::default()).await;
        assert!(r.skipped);
        assert!(r.skip_reason.contains("model declined"));
        // Nothing written, nothing superseded.
        assert!(long_term::list_records(true).is_empty());
        let wm = short_term_store::get_working_memory("conv2").unwrap();
        assert!(wm.iter().all(|e| e.get("superseded_by").is_none()));
    }

    #[tokio::test]
    async fn conversation_unbound_lands_in_long_term_and_supersedes_wm() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        seed_working_memory("conv3", 3);
        let chat = FixedChat::new("a distilled insight");
        let r = reflect("conversation:conv3", Some(&chat), ReflectOptions::default()).await;
        assert!(!r.skipped, "unexpected skip: {}", r.skip_reason);
        assert_eq!(r.scope, "conversation:conv3");
        assert_eq!(r.inputs_considered, 3);
        assert_eq!(r.reflection_body, "a distilled insight");
        assert_eq!(
            r.superseded_ids,
            vec!["wm:conv3:0", "wm:conv3:1", "wm:conv3:2"]
        );

        // Long-term record landed with the conversation source + tag.
        let new_id = r.reflection_id.expect("reflection id");
        let rec = long_term::get(&new_id).expect("record present");
        assert_eq!(rec.body, "a distilled insight");
        assert_eq!(rec.source, "reflection:conversation:conv3");
        assert_eq!(rec.importance, 7);
        assert!(rec.tags.iter().any(|t| t == REFLECTION_TAG));

        // Every working-memory entry now points at the reflection.
        let wm = short_term_store::get_working_memory("conv3").unwrap();
        assert_eq!(wm.len(), 3);
        for e in &wm {
            assert_eq!(e.get("superseded_by").and_then(Value::as_str), Some(new_id.as_str()));
        }

        // A second pass sees zero live inputs and skips.
        let r2 = reflect("conversation:conv3", Some(&chat), ReflectOptions::default()).await;
        assert!(r2.skipped);
        assert_eq!(r2.inputs_considered, 0);
    }

    #[tokio::test]
    async fn conversation_bound_to_workspace_lands_in_workspace_memory() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        conversations_store::set_workspace("conv4", Some("ws-tgt")).unwrap();
        seed_working_memory("conv4", 3);
        let chat = FixedChat::new("workspace-bound insight");
        let r = reflect("conversation:conv4", Some(&chat), ReflectOptions::default()).await;
        assert!(!r.skipped, "unexpected skip: {}", r.skip_reason);

        let new_id = r.reflection_id.expect("reflection id");
        let rec = workspace::get("ws-tgt", &new_id).expect("workspace record present");
        assert_eq!(rec.body, "workspace-bound insight");
        assert_eq!(rec.source, "reflection:conversation:conv4");
        assert_eq!(rec.importance, 7);
        // Nothing leaked into the global tier.
        assert!(long_term::list_records(true).is_empty());
        // Working memory still superseded.
        let wm = short_term_store::get_working_memory("conv4").unwrap();
        assert!(wm.iter().all(|e| {
            e.get("superseded_by").and_then(Value::as_str) == Some(new_id.as_str())
        }));
    }

    #[tokio::test]
    async fn conversation_prompt_carries_system_and_numbered_inputs() {
        let _env = TestEnv::new();
        set_embed_dim_3();
        seed_working_memory("conv5", 3);
        let chat = FixedChat::new("ok");
        let opts = ReflectOptions {
            model: Some("test-model".to_owned()),
            ..Default::default()
        };
        let _ = reflect("conversation:conv5", Some(&chat), opts).await;
        let calls = chat.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (messages, model) = &calls[0];
        assert_eq!(model.as_deref(), Some("test-model"));
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], REFLECTION_SYSTEM_PROMPT);
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("1. (decision) decided thing 0"), "got: {user}");
        assert!(user.contains("3. (decision) decided thing 2"));
    }

    // ── formatting helpers ───────────────────────────────────────────

    #[test]
    fn format_inputs_covers_tool_dict_scalar_and_null_shapes() {
        let entries = vec![
            json!({"kind": "tool", "data": {"name": "git_status", "args": {}}}),
            json!({"kind": "note", "data": {"alpha": 1, "beta": "two"}}),
            json!({"kind": "summary", "data": "read the file"}),
            json!({"data": "kindless entry"}),
            json!({"kind": "blank"}),
        ];
        let block = format_conversation_inputs(&entries);
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], "1. (tool) ran tool git_status");
        assert_eq!(lines[1], "2. (note) alpha=1, beta=two");
        assert_eq!(lines[2], "3. (summary) read the file");
        assert_eq!(lines[3], "4. (raw) kindless entry");
        assert_eq!(lines[4], "5. (blank) ");
    }

    #[test]
    fn format_inputs_truncates_long_values() {
        let long = "x".repeat(500);
        let entries = vec![
            json!({"kind": "note", "data": {"k": long.clone()}}),
            json!({"kind": "raw", "data": long}),
        ];
        let block = format_conversation_inputs(&entries);
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0].len(), "1. (note) k=".len() + 80);
        assert_eq!(lines[1].len(), "2. (raw) ".len() + 200);
    }

    #[test]
    fn select_inputs_filters_superseded_and_non_objects() {
        let _env = TestEnv::new();
        short_term_store::append_working_memory("conv6", json!({"kind": "a", "data": "live"}))
            .unwrap();
        short_term_store::append_working_memory(
            "conv6",
            json!({"kind": "b", "data": "done", "superseded_by": "ref1"}),
        )
        .unwrap();
        let inputs = select_inputs_conversation("conv6");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["kind"], "a");
        // Unknown conversation → empty, never an error.
        assert!(select_inputs_conversation("ghost-conv").is_empty());
    }

    // ── pipe handler ─────────────────────────────────────────────────

    #[tokio::test]
    async fn handle_reflect_requires_scope() {
        let _env = TestEnv::new();
        for payload in [json!({}), json!({"scope": ""}), json!({"scope": 7})] {
            let reply = handle_reflect(payload).await;
            assert!(!reply.ok);
            let err = reply.error.expect("error envelope");
            assert_eq!(err.code, "bad_request");
            assert_eq!(err.message, "scope is required");
        }
    }

    #[tokio::test]
    async fn handle_reflect_returns_full_reflection_result_shape() {
        let _env = TestEnv::new();
        // Whatever the env's model situation, an inputless conversation
        // scope always answers a skipped ReflectionResult — either
        // "no chat_fn supplied" (no model resolvable, Python parity) or
        // "need 3 inputs, have 0" (real cycle ran). Both carry the full
        // field set.
        let reply = handle_reflect(json!({"scope": "conversation:never-seen"})).await;
        assert!(reply.ok);
        let v = reply.data;
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "inputs_considered",
                "reflection_body",
                "reflection_id",
                "scope",
                "skip_reason",
                "skipped",
                "superseded_ids",
            ]
        );
        assert_eq!(v["scope"], "conversation:never-seen");
        assert_eq!(v["skipped"], true);
        assert_eq!(v["reflection_id"], Value::Null);
    }
}
