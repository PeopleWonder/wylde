//! Auto-summary + embedding pipeline and the pure ranking helpers (Plan v2
//! §3.4 / §3.7).
//!
//! Every standalone conversation grows three derived fields that power
//! [`super::api::search_history`]:
//!
//! * `auto_summary`  — a 1–2 sentence LLM précis of the conversation.
//! * `topic_tags`    — a short list of topic keywords from the same call.
//! * `embedding`     — the summary embedded via `ollama.embed`, for cosine
//!   ranking.
//! * `summary_msg_count` — the message count at last summarisation, so we
//!   can tell when a regen is due.
//!
//! ## When a summary is (re)generated
//!
//! * **On demand** — [`refresh_standalone`] regenerates one conversation.
//! * **Cadence** — [`needs_regen`] is true when there is no summary yet, or
//!   the conversation has crossed another [`SUMMARY_EVERY_N_MESSAGES`]
//!   boundary since the last one. A caller (the turn driver, a future
//!   `chat.refresh_summary`) drives the cadence; this module supplies the
//!   decision + the work.
//!
//! ## Separation for testability
//!
//! The LLM / embedder calls ([`generate_summary`], [`refresh_standalone`])
//! are split from the **pure** logic ([`cosine_similarity`],
//! [`lexical_score`], [`needs_regen`], [`parse_summary_response`],
//! [`summary_input_text`]) so the ranking + cadence are fully unit-tested
//! without a live Ollama. The live path is exercised by an ignore-marked
//! integration test.

use serde_json::{json, Map, Value};

use crate::memory::conversations::store as conv_store;
use crate::memory::embeddings;

/// Re-summarise after every N messages (Plan v2 §3.4 cadence). Chosen to
/// match the brief's "every 5 messages".
pub const SUMMARY_EVERY_N_MESSAGES: usize = 5;

/// Hard cap on how much conversation text we feed the summariser, so a long
/// chat can't blow the prompt budget. Generous — a few thousand chars.
const MAX_SUMMARY_INPUT_CHARS: usize = 6_000;

/// Hard cap on the stored summary length (defensive against a chatty model).
const MAX_SUMMARY_CHARS: usize = 600;

/// Cap on the number of topic tags kept.
const MAX_TOPIC_TAGS: usize = 8;

/// Errors from the (impure) generation path. All are treated as fail-soft
/// by callers — a conversation simply keeps its previous summary (or none).
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("conversation not found: {0}")]
    NotFound(String),
    #[error("summary LLM call failed: {0}")]
    Llm(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("no default model configured (set WYLDE_DEFAULT_MODEL)")]
    NoModel,
}

// ── pure ranking helpers ─────────────────────────────────────────────────

/// Cosine similarity of two equal-length vectors, in `[-1, 1]` (≈`[0, 1]`
/// for the non-negative-ish embeddings we use). Returns `0.0` for a
/// length mismatch or a zero-norm vector, so a malformed embedding can
/// never rank above a real match.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Tokenise to lowercase alphanumeric words of length ≥ 2.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// A lexical fallback score in `[0, 1]`: the fraction of the query's
/// distinct terms that appear in `text`. Used when an embedding isn't
/// available (the embedder hasn't run, or Ollama is down) so search still
/// returns something sensible.
pub fn lexical_score(query: &str, text: &str) -> f32 {
    let q: std::collections::BTreeSet<String> = tokenize(query).into_iter().collect();
    if q.is_empty() {
        return 0.0;
    }
    let haystack: std::collections::BTreeSet<String> = tokenize(text).into_iter().collect();
    let hits = q.iter().filter(|t| haystack.contains(*t)).count();
    hits as f32 / q.len() as f32
}

/// The message count stored in a conversation document.
fn message_count(doc: &Value) -> usize {
    doc.get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0)
}

/// True when `doc` should be (re)summarised: no summary yet, or it has
/// crossed another [`SUMMARY_EVERY_N_MESSAGES`] boundary since the count at
/// which it was last summarised.
pub fn needs_regen(doc: &Value) -> bool {
    let count = message_count(doc);
    if count == 0 {
        return false; // nothing to summarise
    }
    let has_summary = doc
        .get("auto_summary")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_summary {
        return true;
    }
    let last = doc
        .get("summary_msg_count")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    // Crossed a new N-boundary since last time?
    (count / SUMMARY_EVERY_N_MESSAGES) > (last / SUMMARY_EVERY_N_MESSAGES)
}

/// The text fed to the summariser: each message as `role: content`, joined
/// newest-last, capped at [`MAX_SUMMARY_INPUT_CHARS`].
pub fn summary_input_text(doc: &Value) -> String {
    let mut out = String::new();
    if let Some(msgs) = doc.get("messages").and_then(Value::as_array) {
        for m in msgs {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            if content.trim().is_empty() {
                continue;
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(content.trim());
            out.push('\n');
            if out.len() >= MAX_SUMMARY_INPUT_CHARS {
                out.truncate(MAX_SUMMARY_INPUT_CHARS);
                break;
            }
        }
    }
    out
}

/// The stored `auto_summary` for a conversation id, when present and
/// non-empty. The improvement-plan **B2** read helper: the turn gather
/// joins this onto `ChatContext.conversation_summary` (the tier-2 slot
/// that existed since Slice G but had no source). Fail-soft: an unknown
/// id, an unreadable doc, or a blank summary all yield `None`.
pub fn auto_summary_for(conversation_id: &str) -> Option<String> {
    if conversation_id.trim().is_empty() {
        return None;
    }
    let doc = crate::memory::conversations::store::read_conversation(conversation_id).ok()?;
    doc.get("auto_summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The display summary for a conversation: its `auto_summary` when present,
/// else a best-effort fallback (title, then the first user message snippet,
/// then "Untitled conversation"). Search/list always have *something* to
/// show even before the summariser has run.
pub fn display_summary(doc: &Value) -> String {
    if let Some(s) = doc.get("auto_summary").and_then(Value::as_str) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_owned();
        }
    }
    if let Some(t) = doc.get("title").and_then(Value::as_str) {
        let t = t.trim();
        if !t.is_empty() && t != "Untitled" {
            return t.to_owned();
        }
    }
    if let Some(msgs) = doc.get("messages").and_then(Value::as_array) {
        for m in msgs {
            if m.get("role").and_then(Value::as_str) == Some("user") {
                if let Some(c) = m.get("content").and_then(Value::as_str) {
                    let c = c.trim();
                    if !c.is_empty() {
                        return snippet(c, 160);
                    }
                }
            }
        }
    }
    "Untitled conversation".to_owned()
}

/// The stored topic tags, if any.
pub fn topic_tags(doc: &Value) -> Vec<String> {
    doc.get("topic_tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The stored embedding vector, if present and well-formed.
pub fn stored_embedding(doc: &Value) -> Option<Vec<f32>> {
    let arr = doc.get("embedding").and_then(Value::as_array)?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        out.push(v.as_f64()? as f32);
    }
    Some(out)
}

/// First `max` chars of `s`, ellipsised on a char boundary.
fn snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Parse a summariser reply into `(summary, tags)`. The prompt asks for the
/// summary on its own, then an optional `Tags: a, b, c` line. Tolerant: a
/// reply with no tags line yields no tags; the summary is everything that
/// isn't the tags line, trimmed + capped.
pub fn parse_summary_response(text: &str) -> (String, Vec<String>) {
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("tags:")
            .or_else(|| lower.strip_prefix("topics:"))
            .or_else(|| lower.strip_prefix("topic_tags:"))
        {
            // Re-extract from the original (preserve case) at the same offset.
            let cut = trimmed.len() - rest.len();
            tags = trimmed[cut..]
                .split([',', ';'])
                .map(|t| t.trim().trim_start_matches('#').trim())
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .take(MAX_TOPIC_TAGS)
                .collect();
            continue;
        }
        if !trimmed.is_empty() {
            summary_lines.push(trimmed);
        }
    }
    let summary = snippet(&summary_lines.join(" "), MAX_SUMMARY_CHARS);
    (summary, tags)
}

/// Build the `fields` map written back to the conversation document for a
/// freshly computed summary. Pure — the persistence is one `merge_fields`
/// call away, which keeps the write path unit-testable.
pub fn summary_fields(
    summary: &str,
    tags: &[String],
    embedding: &[f32],
    msg_count: usize,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("auto_summary".to_owned(), json!(summary));
    m.insert("topic_tags".to_owned(), json!(tags));
    m.insert(
        "embedding".to_owned(),
        Value::Array(embedding.iter().map(|x| json!(*x as f64)).collect()),
    );
    m.insert("summary_msg_count".to_owned(), json!(msg_count as u64));
    m
}

// ── impure generation path ───────────────────────────────────────────────

/// The default chat model used for summarisation. Reads `WYLDE_DEFAULT_MODEL`
/// (same knob `chat.complete` falls back to); empty → [`SummaryError::NoModel`].
fn default_model() -> Result<String, SummaryError> {
    std::env::var("WYLDE_DEFAULT_MODEL")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or(SummaryError::NoModel)
}

/// Ask the LLM for a 1–2 sentence summary + topic tags of `convo_text` via
/// `ollama.chat` on the wylde-ollama service. Returns `(summary, tags)`.
pub async fn generate_summary(convo_text: &str) -> Result<(String, Vec<String>), SummaryError> {
    use wylde_shared::ipc;

    let cfg = crate::config::Config::get();
    let model = default_model()?;

    // B9: the instruction resolves through the prompts catalog so the
    // Settings prompt editor can tune summary style without a rebuild.
    let instruction = crate::prompts::store::effective_prompt("conversation.summarise");
    let prompt = format!("{instruction}\n\n---\n{convo_text}\n---");
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "priority": cfg.default_chat_priority,
        "stream": false,
        "keep_alive": "24h",
    });

    match ipc::call_action(&cfg.ollama_service, "ollama.chat", body).await {
        Ok(upstream) => {
            let text = upstream
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if text.trim().is_empty() {
                return Err(SummaryError::Llm("model returned empty content".to_owned()));
            }
            Ok(parse_summary_response(&text))
        }
        Err(e) => Err(SummaryError::Llm(format!("{}: {}", e.code, e.message))),
    }
}

/// Embed a search query for cosine ranking, fail-soft: any embedder error
/// (Ollama down, model missing) yields `None`, and the caller falls back to
/// lexical scoring rather than failing the search.
pub async fn embed_query(query: &str) -> Option<Vec<f32>> {
    match embeddings::embed_one(query.to_owned()).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("chat.search: query embed failed, using lexical: {e}");
            None
        }
    }
}

/// Regenerate the summary + embedding for one **standalone** conversation
/// and persist them. Fail-soft: any error leaves the stored fields
/// untouched and is returned for the caller to log/ignore.
pub async fn refresh_standalone(conv_id: &str) -> Result<Value, SummaryError> {
    let doc = conv_store::read_conversation(conv_id).map_err(|_| {
        SummaryError::NotFound(format!("standalone conversation {conv_id:?} not found"))
    })?;
    let count = message_count(&doc);
    let input = summary_input_text(&doc);
    if input.trim().is_empty() {
        return Err(SummaryError::Llm(
            "conversation has no text to summarise".to_owned(),
        ));
    }

    let (summary, tags) = generate_summary(&input).await?;
    let embedding = embeddings::embed_one(summary.clone())
        .await
        .map_err(|e| SummaryError::Embed(e.to_string()))?;

    let fields = summary_fields(&summary, &tags, &embedding, count);
    conv_store::merge_fields(conv_id, fields)
        .map_err(|_| SummaryError::NotFound(format!("conversation {conv_id:?} vanished mid-write")))
}

/// Regenerate the summary + embedding for one **workspace** conversation —
/// the parity twin of [`refresh_standalone`] (Phase 2 polish, Item 1).
///
/// Workspace conversations live in the `wylde-workspaces` service, so the
/// harness fetches the doc over the pipe, runs the **same** LLM + embedder
/// pipeline locally (the service has no Ollama client), then pushes the
/// derived fields back via `workspaces.conversations.refresh_summary`. After
/// that, [`super::api::search_history`] ranks workspace conversations by the
/// same cosine path it already uses for standalone ones (their fetched docs
/// now carry `auto_summary` / `topic_tags` / `embedding`).
///
/// Fail-soft like its twin: a slow/unreachable service or embedder error is
/// returned for the caller to log; the stored doc keeps its prior fields.
pub async fn refresh_workspace(workspace_id: &str, conv_id: &str) -> Result<Value, SummaryError> {
    use wylde_workspaces_client::WorkspacesClient;

    let client = WorkspacesClient::for_service(super::api::workspaces_service());
    let doc = client
        .conversations_get(workspace_id, conv_id)
        .await
        .map_err(|e| {
            SummaryError::NotFound(format!(
                "workspace conversation {conv_id:?} unreadable: {e}"
            ))
        })?;
    let count = message_count(&doc);
    let input = summary_input_text(&doc);
    if input.trim().is_empty() {
        return Err(SummaryError::Llm(
            "conversation has no text to summarise".to_owned(),
        ));
    }

    let (summary, tags) = generate_summary(&input).await?;
    let embedding = embeddings::embed_one(summary.clone())
        .await
        .map_err(|e| SummaryError::Embed(e.to_string()))?;

    client
        .conversations_refresh_summary(
            workspace_id,
            conv_id,
            &summary,
            &tags,
            &embedding,
            count as u64,
        )
        .await
        .map_err(|e| SummaryError::Llm(format!("persist workspace summary: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cosine_of_identical_unit_vectors_is_one() {
        let a = vec![0.6_f32, 0.8];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_is_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_ranks_closer_vector_higher() {
        let q = vec![1.0_f32, 0.0, 0.0];
        let near = vec![0.9_f32, 0.1, 0.0];
        let far = vec![0.0_f32, 0.0, 1.0];
        assert!(cosine_similarity(&q, &near) > cosine_similarity(&q, &far));
    }

    #[test]
    fn cosine_guards_mismatch_and_zero_norm() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn lexical_score_is_fraction_of_query_terms_present() {
        // 2 of 2 query terms present → 1.0.
        assert!((lexical_score("apply overrides", "the apply_overrides bug") - 1.0).abs() < 1e-6);
        // 1 of 2 → 0.5.
        assert!((lexical_score("apply missingword", "we apply things") - 0.5).abs() < 1e-6);
        // none → 0.0.
        assert_eq!(lexical_score("zzz qqq", "nothing here"), 0.0);
        // empty query → 0.0.
        assert_eq!(lexical_score("", "anything"), 0.0);
    }

    #[test]
    fn needs_regen_true_when_no_summary_and_messages_exist() {
        let doc = json!({"id": "c", "messages": [{"role": "user", "content": "hi"}]});
        assert!(needs_regen(&doc));
    }

    #[test]
    fn needs_regen_false_when_no_messages() {
        let doc = json!({"id": "c", "messages": []});
        assert!(!needs_regen(&doc));
    }

    #[test]
    fn needs_regen_crosses_n_boundary() {
        // Summarised at 5 messages; now at 9 → same bucket (5..9), no regen.
        let doc = json!({
            "id": "c", "auto_summary": "s", "summary_msg_count": 5,
            "messages": vec![json!({"role":"user","content":"x"}); 9],
        });
        assert!(!needs_regen(&doc));
        // Now at 10 → crossed into the next bucket → regen.
        let doc = json!({
            "id": "c", "auto_summary": "s", "summary_msg_count": 5,
            "messages": vec![json!({"role":"user","content":"x"}); 10],
        });
        assert!(needs_regen(&doc));
    }

    #[test]
    fn parse_summary_splits_summary_and_tags() {
        let (s, tags) = parse_summary_response(
            "We debugged the apply_overrides race in the settings store.\nTags: settings, race, overrides",
        );
        assert!(s.contains("apply_overrides"));
        assert_eq!(tags, vec!["settings", "race", "overrides"]);
    }

    #[test]
    fn parse_summary_tolerates_no_tags_line() {
        let (s, tags) = parse_summary_response("Just a summary, nothing else.");
        assert_eq!(s, "Just a summary, nothing else.");
        assert!(tags.is_empty());
    }

    #[test]
    fn parse_summary_strips_hash_and_caps_tags() {
        let (_s, tags) = parse_summary_response("x\nTopics: #a, #b, c, d, e, f, g, h, i, j");
        assert_eq!(tags.len(), MAX_TOPIC_TAGS);
        assert_eq!(tags[0], "a");
    }

    #[test]
    fn summary_input_joins_messages_with_roles() {
        let doc = json!({
            "messages": [
                {"role": "user", "content": "how do anchors work?"},
                {"role": "assistant", "content": "they are units of attention"},
                {"role": "assistant", "content": "   "},
            ]
        });
        let text = summary_input_text(&doc);
        assert!(text.contains("user: how do anchors work?"));
        assert!(text.contains("assistant: they are units of attention"));
        // Blank content skipped.
        assert_eq!(text.matches("assistant:").count(), 1);
    }

    #[test]
    fn display_summary_prefers_auto_then_title_then_first_user_msg() {
        assert_eq!(
            display_summary(&json!({"auto_summary": "real summary"})),
            "real summary"
        );
        assert_eq!(
            display_summary(&json!({"auto_summary": "  ", "title": "My Chat"})),
            "My Chat"
        );
        assert_eq!(
            display_summary(&json!({
                "title": "Untitled",
                "messages": [{"role": "user", "content": "first thing I said"}]
            })),
            "first thing I said"
        );
        assert_eq!(
            display_summary(&json!({"messages": []})),
            "Untitled conversation"
        );
    }

    #[test]
    fn stored_embedding_reads_or_none() {
        assert_eq!(
            stored_embedding(&json!({"embedding": [0.1, 0.2]})),
            Some(vec![0.1_f32, 0.2])
        );
        assert_eq!(stored_embedding(&json!({"embedding": []})), None);
        assert_eq!(stored_embedding(&json!({})), None);
    }

    #[test]
    fn summary_fields_round_trip_shape() {
        let m = summary_fields("sum", &["a".into(), "b".into()], &[0.5, 0.5], 7);
        assert_eq!(m["auto_summary"], "sum");
        assert_eq!(m["topic_tags"], json!(["a", "b"]));
        assert_eq!(m["summary_msg_count"], 7);
        assert_eq!(m["embedding"].as_array().unwrap().len(), 2);
    }

    /// The persist half of the pipeline (minus the LLM/embedder): writing
    /// `summary_fields` through the store attaches the derived fields,
    /// preserves siblings, and does NOT bump `updated_at` (a re-summary must
    /// not reorder the conversation list).
    #[test]
    fn merge_fields_persists_summary_without_reordering() {
        use crate::memory::conversations::test_support::TestEnv;
        let _env = TestEnv::new();
        let dir = crate::memory::common::conversations_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("c1.json"),
            serde_json::to_string(&json!({
                "id": "c1", "title": "Keep", "created_at": 10, "updated_at": 10,
                "messages": [{"role": "user", "content": "x"}],
            }))
            .unwrap(),
        )
        .unwrap();

        let fields = summary_fields("a précis", &["t".into()], &[0.6, 0.8], 5);
        conv_store::merge_fields("c1", fields).unwrap();

        let doc = conv_store::read_conversation("c1").unwrap();
        assert_eq!(doc["auto_summary"], "a précis");
        assert_eq!(doc["summary_msg_count"], 5);
        assert_eq!(stored_embedding(&doc), Some(vec![0.6_f32, 0.8]));
        // Siblings preserved; activity timestamp untouched.
        assert_eq!(doc["title"], "Keep");
        assert_eq!(doc["updated_at"], 10);
        // The freshly written fields satisfy the display + needs_regen logic.
        assert_eq!(display_summary(&doc), "a précis");
        assert!(
            !needs_regen(&doc),
            "5 msgs summarised at count 5 → no regen"
        );
    }
}
