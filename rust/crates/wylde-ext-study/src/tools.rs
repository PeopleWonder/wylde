//! Tool handlers — `study_index_page` / `study_query` / `study_summarize` /
//! `study_explain` / `study_flashcards`, the five endpoints the Python
//! `handler.py` exposes as `index_page` / `query` / `summarize` / `explain` /
//! `flashcards`.
//!
//! Each returns a `serde_json::Value` whose shape (the `status` / `code` /
//! `error` keys on failure, the field set on success) mirrors the Python dict
//! so the MCP `structuredContent` payload is byte-compatible with the shim's.
//!
//! The harness capability behind each tool is reached over the pipe via the
//! [`HarnessClient`] seam:
//!
//! | tool               | harness verb       |
//! |--------------------|--------------------|
//! | `study_index_page` | `rag.add_episodic` |
//! | `study_query`      | `rag.search`       |
//! | `study_summarize`  | `chat.complete`    |
//! | `study_explain`    | `chat.complete`    |
//! | `study_flashcards` | `chat.complete`    |
//!
//! ## Known `chat.complete` fidelity gaps (surfaced for Slice 5b)
//!
//! Python's `_llm_call` calls `router().chat(messages, model, fmt="json",
//! temperature=0.4)` with a distinct **system** message per tool, and returns
//! a `backend` field. The landed S2a `chat.complete` is deliberately narrow —
//! it takes only `{prompt, model?, max_tokens?}`, injects **no** system role,
//! has **no** `format`/`temperature` knob, and returns **no** `backend`. So
//! the three LLM tools degrade as follows, and are NOT verbatim-faithful:
//!
//!   1. The per-tool system prompt is **folded into the single `prompt`**
//!      (system text + user text), not sent as a `system` role message.
//!   2. The hard `fmt="json"` output constraint and `temperature=0.4` are
//!      **dropped** — we rely on the prompt's own "reply in JSON" instruction
//!      plus the tolerant [`crate::jsonparse`] fallback (same parser Python
//!      uses as its safety net).
//!   3. `backend` is **unobtainable** from `chat.complete`, so it is emitted
//!      as JSON `null` rather than the backend name. This is the one output
//!      key that cannot match Python today — see the gap note in the PR body.
//!
//! `study_index_page` and `study_query` map cleanly and ARE verbatim-faithful.

use serde_json::{json, Value};

use crate::config::Config;
use crate::harness::HarnessClient;
use crate::jsonparse::try_parse_json;

// ── system prompts (verbatim from handler.py) ──────────────────────────────

const SUMMARIZE_SYS: &str = "You are a concise study assistant. Summarise the user's text in plain \
language. Keep the summary tight; produce a JSON object of the form \
{\"summary\": \"...\", \"key_points\": [\"...\", \"...\"]} and nothing else.";

const EXPLAIN_SYS: &str = "You are a study tutor. Explain the user's concept or excerpt in plain \
language for the requested audience. Be precise but accessible. Reply \
in JSON: {\"explanation\": \"...\", \"analogy\": \"...\"}.";

const FLASHCARDS_SYS: &str = "You generate study flashcards. Each card is a JSON object with \
'front' (a question) and 'back' (a concise answer). Reply in JSON: \
{\"cards\": [{\"front\": \"...\", \"back\": \"...\"}, ...]}.";

// ── study_index_page → rag.add_episodic ────────────────────────────────────

/// Index a browser page into the episodic memory tier. Port of
/// `handler.py::index_page`.
pub async fn run_index_page<C: HarnessClient>(client: &C, params: Value) -> Value {
    let url = trimmed_str(&params, "url");
    let text = trimmed_str(&params, "text");
    let title = trimmed_str(&params, "title");
    let session_id = trimmed_str(&params, "session_id");

    if url.is_empty() {
        return err("INVALID_PARAMS", "'url' is required");
    }
    if text.is_empty() {
        return err("INVALID_PARAMS", "'text' is required");
    }

    // Prepend title for retrieval surface — exactly as the Python handler:
    // `body = f"{title}\n\n{text}" if title else text`.
    let body = if title.is_empty() {
        text.clone()
    } else {
        format!("{title}\n\n{text}")
    };

    let payload = json!({
        "content": body,
        "url": url,
        "session_id": session_id,
    });

    match client.call("rag.add_episodic", payload).await {
        Ok(data) if data.get("status").and_then(Value::as_str) == Some("ok") => {
            // `chars=len(body)` — Python counts the composed body's code
            // points, same as the harness's `content.chars().count()`.
            let memory_id = data.get("memory_id").cloned().unwrap_or(Value::Null);
            json!({
                "status": "ok",
                "url": url,
                "title": title,
                "memory_id": memory_id,
                "chars": body.chars().count(),
            })
        }
        // rag.add_episodic returned an in-band error envelope.
        Ok(data) => ingest_error(&envelope_error(&data), &url),
        // Transport / server error.
        Err(e) => ingest_error(&format!("{}: {}", e.code, e.message), &url),
    }
}

// ── study_query → rag.search ───────────────────────────────────────────────

/// Answer a question against the indexed corpus (RAG hits). Port of
/// `handler.py::query`.
pub async fn run_query<C: HarnessClient>(client: &C, params: Value) -> Value {
    let q = trimmed_str(&params, "q");
    if q.is_empty() {
        return err("INVALID_PARAMS", "'q' is required and must be non-empty");
    }
    let limit = clamp_int(&params, "limit", 8, 1, 50);

    let payload = json!({ "q": q, "limit": limit });
    let data = match client.call("rag.search", payload).await {
        Ok(d) => d,
        Err(e) => return search_error(&format!("{}: {}", e.code, e.message)),
    };

    // rag.search returns `{status, q, workspace_id, results, count}` with
    // status "ok" | "insufficient_context" | "error".
    if data.get("status").and_then(Value::as_str) == Some("error") {
        return search_error(&envelope_error(&data));
    }

    let hits = data
        .get("results")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    let count = hits.as_array().map(|a| a.len()).unwrap_or(0);

    if count == 0 {
        // Python's explicit "no matches" shape — note + insufficient_context.
        json!({
            "status": "ok",
            "query": q,
            "hits": [],
            "count": 0,
            "note": "no matches in the indexed corpus; index more pages first",
            "insufficient_context": true,
        })
    } else {
        json!({ "status": "ok", "query": q, "hits": hits, "count": count })
    }
}

// ── study_summarize / study_explain / study_flashcards → chat.complete ──────

/// Summarise the supplied text. Port of `handler.py::summarize`.
pub async fn run_summarize<C: HarnessClient>(client: &C, params: Value) -> Value {
    let text = trimmed_str(&params, "text");
    if text.is_empty() {
        return err("INVALID_PARAMS", "'text' is required");
    }
    let max_words = clamp_int(&params, "max_words", 150, 20, 800);
    let user = format!(
        "Summarise the following in at most {max_words} words. Reply in \
JSON with keys 'summary' and 'key_points'.\n\n{text}"
    );

    let res = match llm_call(client, &params, SUMMARIZE_SYS, &user).await {
        Ok(res) => res,
        Err(e) => return e,
    };
    let parsed = try_parse_json(&res.text);
    json!({
        "status": "ok",
        "summary": parsed_field(&parsed, "summary"),
        "key_points": parsed_field(&parsed, "key_points"),
        "raw": res.text,
        "model": res.model,
        "backend": Value::Null, // GAP: chat.complete returns no backend.
    })
}

/// Explain a concept or excerpt. Port of `handler.py::explain`.
pub async fn run_explain<C: HarnessClient>(client: &C, params: Value) -> Value {
    let text = trimmed_str(&params, "text");
    if text.is_empty() {
        return err("INVALID_PARAMS", "'text' is required");
    }
    let audience = {
        let a = trimmed_str(&params, "audience");
        if a.is_empty() { "general".to_owned() } else { a }
    };
    let user = format!(
        "Audience: {audience}\nExplain the following:\n\n{text}\n\n\
Reply in JSON with keys 'explanation' and 'analogy'."
    );

    let res = match llm_call(client, &params, EXPLAIN_SYS, &user).await {
        Ok(res) => res,
        Err(e) => return e,
    };
    let parsed = try_parse_json(&res.text);
    json!({
        "status": "ok",
        "explanation": parsed_field(&parsed, "explanation"),
        "analogy": parsed_field(&parsed, "analogy"),
        "raw": res.text,
        "model": res.model,
        "backend": Value::Null, // GAP: chat.complete returns no backend.
    })
}

/// Generate Q/A flashcards. Port of `handler.py::flashcards`.
pub async fn run_flashcards<C: HarnessClient>(client: &C, params: Value) -> Value {
    let text = trimmed_str(&params, "text");
    if text.is_empty() {
        return err("INVALID_PARAMS", "'text' is required");
    }
    let count = clamp_int(&params, "count", 8, 1, 50);
    let user = format!(
        "Generate {count} study flashcards from the following text. \
Reply in JSON.\n\n{text}"
    );

    let res = match llm_call(client, &params, FLASHCARDS_SYS, &user).await {
        Ok(res) => res,
        Err(e) => return e,
    };
    let parsed = try_parse_json(&res.text);

    // Keep only well-formed {front, back} cards, stringifying each — exactly
    // the Python validation loop.
    let mut cards: Vec<Value> = Vec::new();
    if let Some(Value::Array(arr)) = parsed.as_ref().and_then(|p| p.get("cards").cloned()).as_ref() {
        for c in arr {
            let front = c.get("front");
            let back = c.get("back");
            if let (Some(front), Some(back)) = (front, back) {
                if is_truthy(front) && is_truthy(back) {
                    cards.push(json!({
                        "front": stringify_scalar(front),
                        "back": stringify_scalar(back),
                    }));
                }
            }
        }
    }

    json!({
        "status": "ok",
        "cards": cards,
        "count": cards.len(),
        "raw": res.text,
        "model": res.model,
        "backend": Value::Null, // GAP: chat.complete returns no backend.
    })
}

// ── chat.complete round-trip ────────────────────────────────────────────────

/// Result of one `chat.complete` round-trip, normalised to the fields the
/// three LLM tools need.
struct LlmResult {
    text: String,
    /// The model the backend actually used (`model_used` from the reply).
    model: String,
}

/// Single `chat.complete` round-trip. Folds `system` + `user` into the one
/// `prompt` the narrow verb accepts (see the module-level gap note), and
/// always sends an explicit `model` so the harness's "model is required"
/// guard never trips. On failure returns the Python `LLM_ERROR` envelope as
/// the `Err` variant for the caller to return directly.
async fn llm_call<C: HarnessClient>(
    client: &C,
    params: &Value,
    system: &str,
    user: &str,
) -> Result<LlmResult, Value> {
    // `name = (model or _default_model()).strip()`.
    let requested = {
        let m = trimmed_str(params, "model");
        if m.is_empty() {
            Config::get().default_model.clone()
        } else {
            m
        }
    };
    let prompt = format!("{system}\n\n{user}");
    let payload = json!({ "prompt": prompt, "model": requested });

    match client.call("chat.complete", payload).await {
        Ok(data) => {
            let text = data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            // Prefer the backend-echoed model; fall back to the requested one.
            let model = data
                .get("model_used")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&requested)
                .to_owned();
            Ok(LlmResult { text, model })
        }
        // chat.complete surfaces bad_request / chat_failed as Reply::err →
        // Python's `_err("LLM_ERROR", error, model=name)`.
        Err(e) => Err(json!({
            "status": "error",
            "code": "LLM_ERROR",
            "error": format!("{}: {}", e.code, e.message),
            "model": requested,
        })),
    }
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// `str(params.get(key) or "").strip()` — read a string param, blank if
/// absent or not a string.
fn trimmed_str(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// `max(lo, min(hi, int(params.get(key, default))))` with the same tolerant
/// fallback: a missing/null/unparseable value yields `default` (already in
/// range); any other value is parsed to an int then clamped.
fn clamp_int(params: &Value, key: &str, default: i64, lo: i64, hi: i64) -> i64 {
    let raw = match params.get(key) {
        None | Some(Value::Null) => return default,
        Some(v) => v,
    };
    let parsed = if let Some(i) = raw.as_i64() {
        i
    } else if let Some(f) = raw.as_f64() {
        f as i64 // int(8.9) == 8 (truncation toward zero)
    } else if let Some(i) = raw.as_str().and_then(|s| s.trim().parse::<i64>().ok()) {
        i
    } else {
        return default;
    };
    parsed.max(lo).min(hi)
}

/// Pull a field out of a tolerantly-parsed object, or `null` when the parse
/// failed or wasn't an object — exactly Python's
/// `parsed.get(k) if isinstance(parsed, dict) else None`.
fn parsed_field(parsed: &Option<Value>, key: &str) -> Value {
    match parsed {
        Some(Value::Object(map)) => map.get(key).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Python truthiness for the flashcard `front`/`back` guard: non-empty string,
/// non-zero number, true, non-empty container. Empty string / 0 / false /
/// null are falsy.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(x)` for a flashcard field — strings pass through, other scalars
/// stringify to their JSON form (without surrounding quotes for scalars).
fn stringify_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Best error message out of an in-band `{status:"error", ...}` envelope.
fn envelope_error(data: &Value) -> String {
    data.get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_owned()
}

fn err(code: &str, message: &str) -> Value {
    json!({ "status": "error", "code": code, "error": message })
}

fn ingest_error(message: &str, url: &str) -> Value {
    json!({ "status": "error", "code": "INGEST_ERROR", "error": message, "url": url })
}

fn search_error(message: &str) -> Value {
    json!({ "status": "error", "code": "SEARCH_ERROR", "error": message })
}

/// Tool-name → handler dispatch, shared by the MCP loop. Returns `None` for an
/// unknown tool so the caller can emit a JSON-RPC method-not-found.
pub async fn dispatch_tool<C: HarnessClient>(
    client: &C,
    name: &str,
    arguments: Value,
) -> Option<Value> {
    let out = match name {
        "study_index_page" => run_index_page(client, arguments).await,
        "study_query" => run_query(client, arguments).await,
        "study_summarize" => run_summarize(client, arguments).await,
        "study_explain" => run_explain(client, arguments).await,
        "study_flashcards" => run_flashcards(client, arguments).await,
        _ => return None,
    };
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use wylde_shared::ipc::IpcError;

    /// A canned mock: records the last (action, payload) it saw and returns a
    /// scripted reply. `reply` is `Ok(data)` for an in-band envelope, or
    /// `Err(IpcError)` for a transport/Reply::err failure.
    struct MockClient {
        reply: Result<Value, IpcError>,
        seen: RefCell<Option<(String, Value)>>,
    }

    impl MockClient {
        fn ok(data: Value) -> Self {
            Self { reply: Ok(data), seen: RefCell::new(None) }
        }
        fn fail(code: &str, msg: &str) -> Self {
            Self { reply: Err(IpcError::new(code, msg)), seen: RefCell::new(None) }
        }
        fn last(&self) -> (String, Value) {
            self.seen.borrow().clone().expect("a call was made")
        }
    }

    impl HarnessClient for MockClient {
        async fn call(&self, action: &str, payload: Value) -> Result<Value, IpcError> {
            *self.seen.borrow_mut() = Some((action.to_owned(), payload));
            self.reply.clone()
        }
    }

    // ── study_index_page ────────────────────────────────────────────────

    #[tokio::test]
    async fn index_page_missing_url_is_invalid_params() {
        let c = MockClient::ok(json!({}));
        let r = run_index_page(&c, json!({ "text": "body" })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
        assert_eq!(r["error"], "'url' is required");
    }

    #[tokio::test]
    async fn index_page_missing_text_is_invalid_params() {
        let c = MockClient::ok(json!({}));
        let r = run_index_page(&c, json!({ "url": "http://x" })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
        assert_eq!(r["error"], "'text' is required");
    }

    #[tokio::test]
    async fn index_page_ok_matches_python_schema() {
        let c = MockClient::ok(json!({
            "status": "ok", "memory_id": "abc123", "id": "abc123",
            "chars": 999, "memory_type": "episodic",
        }));
        let r = run_index_page(
            &c,
            json!({ "url": "http://x", "title": "T", "text": "hello", "session_id": "s1" }),
        )
        .await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["url"], "http://x");
        assert_eq!(r["title"], "T");
        assert_eq!(r["memory_id"], "abc123");
        // chars is computed locally from the composed body "T\n\nhello" (8),
        // NOT taken from the verb reply — matches Python's `len(body)`.
        assert_eq!(r["chars"], 8);
        // Exactly the Python key set, no extras (serde_json::Map is sorted).
        let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["chars", "memory_id", "status", "title", "url"]);
        // The verb got the composed body + url + session_id.
        let (action, payload) = c.last();
        assert_eq!(action, "rag.add_episodic");
        assert_eq!(payload["content"], "T\n\nhello");
        assert_eq!(payload["url"], "http://x");
        assert_eq!(payload["session_id"], "s1");
    }

    #[tokio::test]
    async fn index_page_body_omits_title_when_blank() {
        let c = MockClient::ok(json!({ "status": "ok", "memory_id": "z" }));
        let r = run_index_page(&c, json!({ "url": "http://x", "text": "just body" })).await;
        assert_eq!(r["chars"], 9); // "just body"
        assert_eq!(c.last().1["content"], "just body");
    }

    #[tokio::test]
    async fn index_page_inband_error_is_ingest_error() {
        let c = MockClient::ok(json!({ "status": "error", "error": "store blew up" }));
        let r = run_index_page(&c, json!({ "url": "http://x", "text": "b" })).await;
        assert_eq!(r["code"], "INGEST_ERROR");
        assert_eq!(r["error"], "store blew up");
        assert_eq!(r["url"], "http://x");
    }

    #[tokio::test]
    async fn index_page_transport_error_is_ingest_error() {
        let c = MockClient::fail("pipe_connect", "no pipe");
        let r = run_index_page(&c, json!({ "url": "http://x", "text": "b" })).await;
        assert_eq!(r["code"], "INGEST_ERROR");
        assert_eq!(r["error"], "pipe_connect: no pipe");
        assert_eq!(r["url"], "http://x");
    }

    // ── study_query ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn query_missing_q_is_invalid_params() {
        let c = MockClient::ok(json!({}));
        let r = run_query(&c, json!({})).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn query_with_hits_maps_results_to_hits() {
        let row = json!({ "id": "r1", "content": "c", "memory_type": "episodic" });
        let c = MockClient::ok(json!({
            "status": "ok", "q": "what", "workspace_id": "default",
            "results": [row.clone()], "count": 1,
        }));
        let r = run_query(&c, json!({ "q": "what", "limit": 5 })).await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["query"], "what");
        assert_eq!(r["count"], 1);
        assert_eq!(r["hits"][0], row);
        // limit forwarded as-is.
        assert_eq!(c.last().1["limit"], 5);
    }

    #[tokio::test]
    async fn query_no_hits_emits_python_insufficient_context_shape() {
        let c = MockClient::ok(json!({
            "status": "insufficient_context", "q": "x",
            "workspace_id": "default", "results": [], "count": 0,
        }));
        let r = run_query(&c, json!({ "q": "x" })).await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["query"], "x");
        assert_eq!(r["count"], 0);
        assert_eq!(r["hits"], json!([]));
        assert_eq!(r["insufficient_context"], true);
        assert_eq!(r["note"], "no matches in the indexed corpus; index more pages first");
    }

    #[tokio::test]
    async fn query_clamps_limit() {
        let c = MockClient::ok(json!({ "status": "ok", "results": [], "count": 0 }));
        let _ = run_query(&c, json!({ "q": "x", "limit": 9999 })).await;
        assert_eq!(c.last().1["limit"], 50);
    }

    #[tokio::test]
    async fn query_inband_error_is_search_error() {
        let c = MockClient::ok(json!({ "status": "error", "error": "unknown tier 'bogus'" }));
        let r = run_query(&c, json!({ "q": "x" })).await;
        assert_eq!(r["code"], "SEARCH_ERROR");
        assert_eq!(r["error"], "unknown tier 'bogus'");
    }

    #[tokio::test]
    async fn query_transport_error_is_search_error() {
        let c = MockClient::fail("pipe_timeout", "slow");
        let r = run_query(&c, json!({ "q": "x" })).await;
        assert_eq!(r["code"], "SEARCH_ERROR");
        assert_eq!(r["error"], "pipe_timeout: slow");
    }

    // ── study_summarize ─────────────────────────────────────────────────

    #[tokio::test]
    async fn summarize_missing_text_is_invalid_params() {
        let c = MockClient::ok(json!({}));
        let r = run_summarize(&c, json!({})).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn summarize_ok_parses_json_and_folds_system_prompt() {
        let c = MockClient::ok(json!({
            "text": "{\"summary\": \"s\", \"key_points\": [\"a\", \"b\"]}",
            "model_used": "llama3", "tokens_used": 5,
        }));
        let r = run_summarize(&c, json!({ "text": "long text", "model": "llama3" })).await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["summary"], "s");
        assert_eq!(r["key_points"], json!(["a", "b"]));
        assert_eq!(r["model"], "llama3");
        assert!(r["raw"].is_string());
        assert!(r["backend"].is_null());
        // The system prompt is folded into the single `prompt` arg.
        let (action, payload) = c.last();
        assert_eq!(action, "chat.complete");
        let prompt = payload["prompt"].as_str().unwrap();
        assert!(prompt.starts_with(SUMMARIZE_SYS));
        assert!(prompt.contains("long text"));
        assert_eq!(payload["model"], "llama3");
    }

    #[tokio::test]
    async fn summarize_unparseable_text_yields_null_fields() {
        let c = MockClient::ok(json!({ "text": "I couldn't do that", "model_used": "m" }));
        let r = run_summarize(&c, json!({ "text": "x" })).await;
        assert_eq!(r["status"], "ok");
        assert!(r["summary"].is_null());
        assert!(r["key_points"].is_null());
        assert_eq!(r["raw"], "I couldn't do that");
    }

    #[tokio::test]
    async fn summarize_llm_failure_is_llm_error() {
        let c = MockClient::fail("chat_failed", "ollama down");
        let r = run_summarize(&c, json!({ "text": "x", "model": "m" })).await;
        assert_eq!(r["code"], "LLM_ERROR");
        assert_eq!(r["error"], "chat_failed: ollama down");
        assert_eq!(r["model"], "m"); // requested model echoed back.
    }

    // ── study_explain ────────────────────────────────────────────────────

    #[tokio::test]
    async fn explain_ok_parses_fields_and_defaults_audience() {
        let c = MockClient::ok(json!({
            "text": "{\"explanation\": \"e\", \"analogy\": \"like a pump\"}",
            "model_used": "m",
        }));
        let r = run_explain(&c, json!({ "text": "the heart" })).await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["explanation"], "e");
        assert_eq!(r["analogy"], "like a pump");
        assert!(r["backend"].is_null());
        // Default audience "general" appears in the folded prompt.
        assert!(c.last().1["prompt"].as_str().unwrap().contains("Audience: general"));
    }

    #[tokio::test]
    async fn explain_honours_audience() {
        let c = MockClient::ok(json!({ "text": "{}", "model_used": "m" }));
        let _ = run_explain(&c, json!({ "text": "x", "audience": "expert" })).await;
        assert!(c.last().1["prompt"].as_str().unwrap().contains("Audience: expert"));
    }

    // ── study_flashcards ──────────────────────────────────────────────────

    #[tokio::test]
    async fn flashcards_keeps_only_well_formed_cards() {
        let c = MockClient::ok(json!({
            "text": "{\"cards\": [\
                {\"front\": \"Q1\", \"back\": \"A1\"}, \
                {\"front\": \"\", \"back\": \"A2\"}, \
                {\"front\": \"Q3\"}, \
                {\"front\": \"Q4\", \"back\": \"A4\"}]}",
            "model_used": "m",
        }));
        let r = run_flashcards(&c, json!({ "text": "src", "count": 4 })).await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["count"], 2);
        assert_eq!(r["cards"][0], json!({ "front": "Q1", "back": "A1" }));
        assert_eq!(r["cards"][1], json!({ "front": "Q4", "back": "A4" }));
        assert!(r["backend"].is_null());
    }

    #[tokio::test]
    async fn flashcards_stringifies_non_string_card_values() {
        let c = MockClient::ok(json!({
            "text": "{\"cards\": [{\"front\": 1, \"back\": true}]}",
            "model_used": "m",
        }));
        let r = run_flashcards(&c, json!({ "text": "src" })).await;
        assert_eq!(r["count"], 1);
        assert_eq!(r["cards"][0]["front"], "1");
        assert_eq!(r["cards"][0]["back"], "true");
    }

    #[tokio::test]
    async fn flashcards_no_cards_when_parse_fails() {
        let c = MockClient::ok(json!({ "text": "nope", "model_used": "m" }));
        let r = run_flashcards(&c, json!({ "text": "x" })).await;
        assert_eq!(r["count"], 0);
        assert_eq!(r["cards"], json!([]));
    }

    // ── dispatch ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_unknown_tool_is_none() {
        let c = MockClient::ok(json!({}));
        assert!(dispatch_tool(&c, "nope", json!({})).await.is_none());
    }

    #[tokio::test]
    async fn dispatch_routes_known_tool() {
        let c = MockClient::ok(json!({ "status": "ok", "memory_id": "m" }));
        let out = dispatch_tool(&c, "study_index_page", json!({ "url": "u", "text": "t" })).await;
        assert_eq!(out.unwrap()["status"], "ok");
    }

    #[tokio::test]
    async fn clamp_int_tolerates_garbage() {
        assert_eq!(clamp_int(&json!({}), "limit", 8, 1, 50), 8);
        assert_eq!(clamp_int(&json!({ "limit": Value::Null }), "limit", 8, 1, 50), 8);
        assert_eq!(clamp_int(&json!({ "limit": "abc" }), "limit", 8, 1, 50), 8);
        assert_eq!(clamp_int(&json!({ "limit": "12" }), "limit", 8, 1, 50), 12);
        assert_eq!(clamp_int(&json!({ "limit": 0 }), "limit", 8, 1, 50), 1);
        assert_eq!(clamp_int(&json!({ "limit": 8.9 }), "limit", 8, 1, 50), 8);
    }
}
