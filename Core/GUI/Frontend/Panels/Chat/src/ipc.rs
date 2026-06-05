//! Per-panel IPC helpers for the Chat panel.
//!
//! Wraps the harness's chat-turn driver + workspace MRU + consent
//! gate into typed reads / writes the View body consumes.
//!
//! Slice 5.1 additions:
//!   * `chat.stream_tools` subscriber + `ToolChunk` projection so the
//!     activity strip can render a subtle "consulting tools…" indicator
//!     without breaking the InferenceBar invariant ("tool calls don't
//!     appear as bubbles").
//!   * `chat.cancel` unary verb so the in-flight stream can be aborted
//!     server-side AS WELL AS by the client dropping its `PipeStream`.
//!   * `ollama.list_models` reader for the model picker pill.
//!   * `start_turn_with_model` — `start_turn` variant that includes a
//!     `model` field in the payload (harness already accepts it; the
//!     existing `start_turn` left it out).

use serde_json::{json, Value};

pub const SVC_HARNESS: &str = "wylde-harness";

/// `chat.start_turn` reply — the turn handle the caller follows up on
/// via `chat.stream_turn`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartTurnReply {
    pub turn_id: String,
    pub conversation_id: String,
}

impl StartTurnReply {
    pub fn from_value(v: &Value) -> Self {
        Self {
            turn_id: v
                .get("turn_id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            conversation_id: v
                .get("conversation_id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// One workspace, MRU-clipped for the InferenceBar dropdown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceSummary {
    pub id: String,
    pub path: String,
}

impl WorkspaceSummary {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            path: v
                .get("path")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// `consent.stream_pending` event — one of the four shapes the chunk
/// can take.  We project them into a single enum so the panel match
/// stays exhaustive.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsentEvent {
    /// A tool is awaiting Allow / Deny / Once.
    Pending(PendingConsent),
    /// A prompt resolved (locally or remotely); drop matching cards.
    Resolved {
        id: String,
        tool: String,
        decision: String,
    },
    /// The broadcast lagged — caller should `consent.list` to recover.
    Lagged,
    /// Heartbeat — silently dropped.
    Heartbeat,
}

impl ConsentEvent {
    pub fn from_value(v: &Value) -> Option<Self> {
        let t = v.get("type").and_then(|x| x.as_str())?;
        match t {
            "pending" => Some(Self::Pending(PendingConsent::from_value(v))),
            "resolved" => Some(Self::Resolved {
                id: v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                tool: v
                    .get("tool")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                decision: v
                    .get("decision")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            }),
            "lagged" => Some(Self::Lagged),
            "heartbeat" => Some(Self::Heartbeat),
            _ => None,
        }
    }
}

/// One pending-consent card.  Mirrors `consent::PendingEntry` on the
/// harness side; inlined to keep the panel free of a harness-crate
/// dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PendingConsent {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub default_action: String,
}

impl PendingConsent {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            tool: v
                .get("tool")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            summary: v
                .get("summary")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            default_action: v
                .get("default_action")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// One streaming chunk on the user-facing `chat.stream_turn` channel.
/// Mirror of `wylde_harness::events::TurnEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnChunk {
    Token { turn_id: String, text: String },
    Thinking { turn_id: String, text: String },
    TurnComplete { turn_id: String, final_message: String },
    TurnAborted { turn_id: String, reason: String, error: Option<String> },
    /// Unknown event type — preserved as a noop so a new harness
    /// event variant doesn't surface as a parse failure.
    Unknown,
}

impl TurnChunk {
    pub fn from_value(v: &Value) -> Self {
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let turn_id = v
            .get("turn_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        match t {
            "token" => Self::Token {
                turn_id,
                text: v
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            "thinking" => Self::Thinking {
                turn_id,
                text: v
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            "turn_complete" => Self::TurnComplete {
                turn_id,
                final_message: v
                    .get("final_message")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            "turn_aborted" => Self::TurnAborted {
                turn_id,
                reason: v
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("aborted")
                    .to_owned(),
                error: v
                    .get("error")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_owned()),
            },
            _ => Self::Unknown,
        }
    }
}

// ── Unary verbs ──────────────────────────────────────────────────────

/// `chat.start_turn` — registers a turn handle the streaming subscriber
/// then follows up on via `chat.stream_turn`.
pub async fn start_turn(
    user_message: &str,
    conversation_id: &str,
    workspace_id: Option<&str>,
) -> Result<StartTurnReply, String> {
    start_turn_with_model(user_message, conversation_id, workspace_id, None).await
}

/// `chat.start_turn` with an optional `model` override.  Empty / None
/// falls back to the harness's `default_model` config.
pub async fn start_turn_with_model(
    user_message: &str,
    conversation_id: &str,
    workspace_id: Option<&str>,
    model: Option<&str>,
) -> Result<StartTurnReply, String> {
    let mut payload = serde_json::Map::new();
    payload.insert("user_message".into(), Value::String(user_message.to_owned()));
    payload.insert(
        "conversation_id".into(),
        Value::String(conversation_id.to_owned()),
    );
    if let Some(ws) = workspace_id {
        payload.insert("workspace_id".into(), Value::String(ws.to_owned()));
    }
    if let Some(m) = model.filter(|s| !s.is_empty()) {
        payload.insert("model".into(), Value::String(m.to_owned()));
    }
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "chat.start_turn",
            "payload": Value::Object(payload),
        })),
    )
    .await?;
    Ok(StartTurnReply::from_value(&v))
}

/// `chat.cancel` — server-side cancel.  Complements the client-side
/// `drop(active_stream)` path used by the Stop button.
pub async fn cancel_turn(turn_id: &str) -> Result<(), String> {
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "chat.cancel",
            "payload": { "turn_id": turn_id },
        })),
    )
    .await
    .map(|_| ())
}

/// `ollama.list_models` — names from `/api/tags`.  Used by the model
/// picker pill.  Empty list when Ollama is down — matches the harness's
/// soft-fail shape.
pub async fn list_models() -> Result<Vec<String>, String> {
    let v = wylde_gui_pipe::call(
        "wylde-ollama",
        "POST",
        "/__action__",
        Some(json!({ "action": "ollama.list_models", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("models").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("name").and_then(|x| x.as_str()).map(str::to_owned))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// `ollama.eject` — release a model from VRAM immediately (empty-prompt
/// `/api/generate` with `keep_alive=0` on the harness side).  Used by the
/// InferenceBar's eject button.  Requires a concrete model name; the
/// "(auto)" selection has none, so the button is disabled in that state.
pub async fn eject_model(model: &str) -> Result<(), String> {
    wylde_gui_pipe::call(
        "wylde-ollama",
        "POST",
        "/__action__",
        Some(json!({
            "action": "ollama.eject",
            "payload": { "model": model },
        })),
    )
    .await
    .map(|_| ())
}

/// `models.set_active` — persist the inference-bar pick so it's
/// observable cross-process (Settings reads it via `models.get_effective`
/// to preview the model whose defaults apply to the next turn). `None`
/// clears it ("(auto)"). Fire-and-confirm: the caller publishes the
/// model bus optimistically and only logs a transport failure.
pub async fn set_active_model(model: Option<&str>) -> Result<(), String> {
    let model = match model {
        Some(m) => json!(m),
        None => Value::Null,
    };
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "models.set_active", "payload": { "model": model } })),
    )
    .await
    .map(|_| ())
}

/// `memory.workspaces.recent` — MRU-clipped workspace list for the
/// InferenceBar dropdown.
pub async fn recent_workspaces(limit: u32) -> Result<Vec<WorkspaceSummary>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "memory.workspaces.recent",
            "payload": { "limit": limit },
        })),
    )
    .await?;
    let Some(arr) = v.get("workspaces").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(WorkspaceSummary::from_value).collect())
}

/// `rag.workspaces.activate` — wire the picked folder up as a
/// workspace.  Mirrors the Workspaces panel's flow so a workspace
/// added here shows up there too.
pub async fn activate_workspace(path: &str) -> Result<WorkspaceSummary, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "rag.workspaces.activate",
            "payload": {
                "path": path,
                "conversation_id": null,
                "full_reindex": false,
            },
        })),
    )
    .await?;
    // `rag.workspaces.activate` returns the full workspace record; we
    // only need the (id, path) projection for the dropdown.
    Ok(WorkspaceSummary::from_value(&v))
}

/// `consent.respond` — Allow / Deny / Once response to a pending
/// gate prompt.  `remember = false` makes the decision authorise the
/// current call without writing the persisted consent store
/// (Phase 12.6 "Once" semantics).
pub async fn respond_consent(
    tool_id: &str,
    decision: &str,
    remember: bool,
) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "consent.respond",
            "payload": {
                "tool_id": tool_id,
                "decision": decision,
                "remember": remember,
            },
        })),
    )
    .await
}

// ── Streaming verbs ──────────────────────────────────────────────────

/// `chat.stream_turn` — subscribe to the user-facing token stream for
/// `turn_id`.  Cancellable by dropping the returned `PipeStream`.
pub fn stream_turn(turn_id: &str) -> Result<wylde_gui_pipe::PipeStream, String> {
    wylde_gui_pipe::stream_call(
        SVC_HARNESS,
        "chat.stream_turn",
        json!({ "turn_id": turn_id }),
    )
}

/// `chat.stream_tools` — subscribe to the tool-activity event stream for
/// `turn_id`.  Disjoint from `chat.stream_turn`; consumed by the
/// activity strip near the InferenceBar so the user sees "Wylde is
/// consulting memory…" without the tool calls polluting the bubble log.
pub fn stream_tools(turn_id: &str) -> Result<wylde_gui_pipe::PipeStream, String> {
    wylde_gui_pipe::stream_call(
        SVC_HARNESS,
        "chat.stream_tools",
        json!({ "turn_id": turn_id }),
    )
}

/// `consent.stream_pending` — long-lived subscription for pending
/// consent prompts.  The panel keeps one open for the lifetime of
/// the View.
pub fn stream_consent_pending() -> Result<wylde_gui_pipe::PipeStream, String> {
    wylde_gui_pipe::stream_call(SVC_HARNESS, "consent.stream_pending", json!({}))
}

/// One streaming chunk on the tool-activity channel
/// (`chat.stream_tools`).  Mirror of `wylde_harness::events::ToolEvent`.
/// We only project the fields the activity strip actually renders;
/// fields like `args`, `output`, `duration_ms` are discarded at parse
/// time so the strip doesn't accidentally surface a tool's input args
/// to the user.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChunk {
    Dispatched { turn_id: String, call_id: String, name: String },
    Result { turn_id: String, call_id: String, name: String },
    Error { turn_id: String, call_id: String, name: String, message: String },
    /// Memory-write side effect — used to surface a brief "remembered:
    /// …" pulse when the strip wants to acknowledge it.  Slice 5.1 just
    /// keeps the variant for future use; the strip ignores it.
    MemoryWritten,
    /// Trailing wylde-check findings.  Ignored by the strip.
    Warning,
    /// Unknown / future variant — preserved as noop.
    Unknown,
}

impl ToolChunk {
    pub fn from_value(v: &Value) -> Self {
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let turn_id = v
            .get("turn_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let call_id = v
            .get("call_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        match t {
            "tool_dispatched" => Self::Dispatched { turn_id, call_id, name },
            "tool_result" => Self::Result { turn_id, call_id, name },
            "tool_error" => Self::Error {
                turn_id,
                call_id,
                name,
                message: v
                    .get("error")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            "memory_written" => Self::MemoryWritten,
            "tool_warning" => Self::Warning,
            _ => Self::Unknown,
        }
    }
}

// ── Short-term (working) memory ───────────────────────────────────────

/// One short-term ("working memory") entry for the active conversation.
///
/// The chat-turn driver appends these as it works — the harness
/// convention is `{"kind": "<tool|file|decision|summary>", "at": <ts>,
/// "data": {...}}` (see `Core/harness/memory/conversation.py::
/// append_working_memory`).  `data` is freeform, so we project it down to
/// a single human-readable `summary` line for the strip; the raw payload
/// is intentionally dropped so the view can't accidentally surface a
/// tool's full input/output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkingMemoryEntry {
    pub kind: String,
    pub summary: String,
}

impl WorkingMemoryEntry {
    pub fn from_value(v: &Value) -> Self {
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("entry")
            .to_owned();
        Self {
            kind,
            summary: summarize_working_data(v.get("data")),
        }
    }
}

/// Collapse a freeform working-memory `data` value into one short line.
/// Strings pass through; objects prefer a known descriptive field
/// (`summary` / `text` / `title` / `path` / `name`) and fall back to a
/// comma-joined key list; everything else is rendered compactly.
fn summarize_working_data(data: Option<&Value>) -> String {
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

/// `memory.short_term.get` — the rolling working-memory buffer for
/// `conversation_id`.  Reply shape is `{ working_memory: [...],
/// conversation_id }`; a missing / non-array `working_memory` reads as
/// an empty buffer (a conversation that hasn't accrued any yet).
pub async fn fetch_working_memory(
    conversation_id: &str,
) -> Result<Vec<WorkingMemoryEntry>, String> {
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
    Ok(arr.iter().map(WorkingMemoryEntry::from_value).collect())
}

/// `memory.short_term.clear` — drop the working-memory buffer for
/// `conversation_id`.  Returns whether anything was actually cleared
/// (`{ cleared: bool, conversation_id }`).
pub async fn clear_working_memory(conversation_id: &str) -> Result<bool, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "memory.short_term.clear",
            "payload": { "conversation_id": conversation_id },
        })),
    )
    .await?;
    Ok(v.get("cleared").and_then(|x| x.as_bool()).unwrap_or(false))
}

// ── Conversations (Memory Slice B) ───────────────────────────────────

/// One conversation's lightweight metadata for the switcher rail.
/// Mirrors a `conversations.list` entry; `working_memory_count` is the
/// additive field the Rust port surfaces for the WM badge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: u64,
    pub working_memory_count: u64,
    pub model: String,
}

impl ConversationMeta {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Untitled")
                .to_owned(),
            created_at: v.get("created_at").and_then(Value::as_i64).unwrap_or(0),
            updated_at: v.get("updated_at").and_then(Value::as_i64).unwrap_or(0),
            message_count: v.get("message_count").and_then(Value::as_u64).unwrap_or(0),
            working_memory_count: v
                .get("working_memory_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            model: v.get("model").and_then(Value::as_str).unwrap_or_default().to_owned(),
        }
    }
}

/// `conversations.list` — every saved chat's metadata, newest-first.
pub async fn list_conversations() -> Result<Vec<ConversationMeta>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "conversations.list", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("conversations").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(ConversationMeta::from_value).collect())
}

/// One persisted chat message projected from a `conversations.get`
/// document's `messages` array — just the `role` + `content` the bubble
/// log needs to rehydrate a conversation when the user switches to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedMessage {
    pub role: String,
    pub content: String,
}

/// `conversations.get` → the conversation's stored `messages`, projected
/// to `(role, content)`. System messages are already stripped server-side
/// (they're regenerated each turn); we additionally drop any empty-content
/// rows so the rehydrated log matches what the user actually sees. A
/// `not_found` (e.g. a brand-new conversation with no turns yet) yields an
/// empty list rather than an error — switching to it just shows a blank log.
pub async fn fetch_conversation_messages(id: &str) -> Result<Vec<LoadedMessage>, String> {
    let v = match wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "conversations.get",
            "payload": { "id": id },
        })),
    )
    .await
    {
        Ok(v) => v,
        // A not-yet-persisted conversation reads as an empty log, not an
        // error — the harness returns not_found before the first turn.
        Err(e) if e.contains("not_found") => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let Some(arr) = v.get("messages").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|x| x.as_str()).unwrap_or_default();
            let content = m.get("content").and_then(|x| x.as_str()).unwrap_or_default();
            if content.is_empty() {
                return None;
            }
            Some(LoadedMessage {
                role: role.to_owned(),
                content: content.to_owned(),
            })
        })
        .collect())
}

/// `conversations.new` — mint a fresh conversation id. Returns the id.
pub async fn new_conversation() -> Result<String, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "conversations.new", "payload": {} })),
    )
    .await?;
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default();
    if id.is_empty() {
        return Err("conversations.new returned no id".to_owned());
    }
    Ok(id.to_owned())
}

/// `conversations.delete` — remove a conversation. Returns whether a file
/// was actually deleted (`false` when it was already gone).
pub async fn delete_conversation(id: &str) -> Result<bool, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "conversations.delete",
            "payload": { "id": id },
        })),
    )
    .await?;
    Ok(v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
}

/// `conversations.get_active` — the persisted active-conversation
/// selection. `Ok(None)` when none has been chosen yet (the reply's `id`
/// is `""`).
pub async fn get_active_conversation() -> Result<Option<String>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "conversations.get_active", "payload": {} })),
    )
    .await?;
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default();
    Ok(if id.is_empty() { None } else { Some(id.to_owned()) })
}

/// `conversations.set_active` — persist the active-conversation selection
/// so it survives an app restart. An empty id clears it.
pub async fn set_active_conversation(id: &str) -> Result<(), String> {
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({
            "action": "conversations.set_active",
            "payload": { "id": id },
        })),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_turn_reply_parses_full_payload() {
        let r = StartTurnReply::from_value(&json!({
            "turn_id": "abc",
            "conversation_id": "default",
        }));
        assert_eq!(r.turn_id, "abc");
        assert_eq!(r.conversation_id, "default");
    }

    #[test]
    fn workspace_summary_parses() {
        let s = WorkspaceSummary::from_value(&json!({
            "id": "wylde",
            "path": "/tmp/wylde",
        }));
        assert_eq!(s.id, "wylde");
        assert_eq!(s.path, "/tmp/wylde");
    }

    #[test]
    fn turn_chunk_parses_token() {
        let c = TurnChunk::from_value(&json!({
            "type": "token",
            "turn_id": "t",
            "text": "hello",
        }));
        assert!(matches!(c, TurnChunk::Token { ref text, .. } if text == "hello"));
    }

    #[test]
    fn turn_chunk_parses_turn_complete() {
        let c = TurnChunk::from_value(&json!({
            "type": "turn_complete",
            "turn_id": "t",
            "final_message": "done",
        }));
        match c {
            TurnChunk::TurnComplete { final_message, .. } => {
                assert_eq!(final_message, "done");
            }
            _ => panic!("expected TurnComplete"),
        }
    }

    #[test]
    fn turn_chunk_unknown_does_not_panic() {
        let c = TurnChunk::from_value(&json!({"type": "future_event"}));
        assert!(matches!(c, TurnChunk::Unknown));
    }

    #[test]
    fn consent_event_parses_pending() {
        let e = ConsentEvent::from_value(&json!({
            "type": "pending",
            "id": "p1",
            "tool": "fs.write_file",
            "summary": "write README.md",
            "default_action": "deny",
            "awaiting_since": 1700000000,
        }));
        match e {
            Some(ConsentEvent::Pending(p)) => {
                assert_eq!(p.id, "p1");
                assert_eq!(p.tool, "fs.write_file");
                assert_eq!(p.default_action, "deny");
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn consent_event_parses_resolved() {
        let e = ConsentEvent::from_value(&json!({
            "type": "resolved",
            "id": "p1",
            "tool": "fs.write_file",
            "decision": "approved",
        }));
        match e {
            Some(ConsentEvent::Resolved { decision, .. }) => {
                assert_eq!(decision, "approved");
            }
            _ => panic!("expected Resolved"),
        }
    }

    #[test]
    fn consent_event_parses_heartbeat() {
        let e = ConsentEvent::from_value(&json!({"type": "heartbeat", "ts": 1}));
        assert!(matches!(e, Some(ConsentEvent::Heartbeat)));
    }

    #[test]
    fn consent_event_parses_lagged() {
        let e = ConsentEvent::from_value(&json!({"type": "lagged"}));
        assert!(matches!(e, Some(ConsentEvent::Lagged)));
    }

    #[test]
    fn consent_event_unknown_type_is_none() {
        let e = ConsentEvent::from_value(&json!({"type": "what"}));
        assert!(e.is_none());
    }

    #[test]
    fn harness_service_name_matches_pipe_prefix() {
        assert_eq!(SVC_HARNESS, "wylde-harness");
    }

    #[test]
    fn each_pipe_call_compiles() {
        let _ = start_turn;
        let _ = eject_model;
        let _ = recent_workspaces;
        let _ = activate_workspace;
        let _ = respond_consent;
        let _ = stream_turn;
        let _ = stream_consent_pending;
        let _ = fetch_working_memory;
        let _ = clear_working_memory;
        let _ = list_conversations;
        let _ = new_conversation;
        let _ = delete_conversation;
        let _ = get_active_conversation;
        let _ = set_active_conversation;
        let _ = fetch_conversation_messages;
    }

    #[test]
    fn conversation_meta_parses_full_entry() {
        let m = ConversationMeta::from_value(&json!({
            "id": "c1",
            "title": "Plan the trip",
            "created_at": 100,
            "updated_at": 200,
            "message_count": 4,
            "working_memory_count": 2,
            "model": "qwen2.5",
        }));
        assert_eq!(m.id, "c1");
        assert_eq!(m.title, "Plan the trip");
        assert_eq!(m.updated_at, 200);
        assert_eq!(m.message_count, 4);
        assert_eq!(m.working_memory_count, 2);
        assert_eq!(m.model, "qwen2.5");
    }

    #[test]
    fn conversation_meta_defaults_blank_title_to_untitled() {
        let m = ConversationMeta::from_value(&json!({ "id": "c2", "title": "" }));
        assert_eq!(m.title, "Untitled");
        assert_eq!(m.message_count, 0);
        assert_eq!(m.working_memory_count, 0);
    }

    #[test]
    fn working_memory_entry_parses_object_data_with_summary() {
        let e = WorkingMemoryEntry::from_value(&json!({
            "kind": "tool",
            "at": 1_700_000_000_i64,
            "data": { "summary": "searched memory for 'rust'", "name": "memory.long_term.search" },
        }));
        assert_eq!(e.kind, "tool");
        // `summary` wins over `name`.
        assert_eq!(e.summary, "searched memory for 'rust'");
    }

    #[test]
    fn working_memory_entry_falls_back_through_known_keys() {
        // No `summary`/`text`/`title` → first present of path/name.
        let e = WorkingMemoryEntry::from_value(&json!({
            "kind": "file",
            "data": { "path": "src/lib.rs", "bytes": 42 },
        }));
        assert_eq!(e.kind, "file");
        assert_eq!(e.summary, "src/lib.rs");
    }

    #[test]
    fn working_memory_entry_string_data_passes_through() {
        let e = WorkingMemoryEntry::from_value(&json!({
            "kind": "decision",
            "data": "use the strangler fallback",
        }));
        assert_eq!(e.summary, "use the strangler fallback");
    }

    #[test]
    fn working_memory_entry_defaults_kind_when_absent() {
        let e = WorkingMemoryEntry::from_value(&json!({ "data": "x" }));
        assert_eq!(e.kind, "entry");
    }

    #[test]
    fn working_memory_entry_unknown_object_joins_keys() {
        let e = WorkingMemoryEntry::from_value(&json!({
            "kind": "raw",
            "data": { "alpha": 1, "beta": 2 },
        }));
        // Order is the serde_json map order; both keys present.
        assert!(e.summary.contains("alpha"));
        assert!(e.summary.contains("beta"));
    }

    #[test]
    fn working_memory_entry_missing_data_is_empty_summary() {
        let e = WorkingMemoryEntry::from_value(&json!({ "kind": "summary" }));
        assert_eq!(e.kind, "summary");
        assert!(e.summary.is_empty());
    }
}
