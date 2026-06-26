//! `HarnessEvent` taxonomy — the wire shape Python's `_streaming.py`
//! emits today, mirrored in Rust for the streaming slice (5.B).
//!
//! Two disjoint streams come off a turn:
//!
//! * **user-facing** (`chat.stream_turn`) — `token`, `thinking`,
//!   `turn_complete`, `turn_aborted`.
//! * **tool-activity** (`chat.stream_tools`) — `tool_dispatched`,
//!   `tool_result`, `tool_error`, `memory_written`, `tool_warning`.
//!
//! Wire shape per event is `{type: <snake_name>, ...fields}` — the
//! Python pipe `_chat.py::_stream` flattens `TurnEvent.data` next to
//! `type`. We mirror that so the long-poll JSON envelope stays
//! byte-equivalent across the strangler.
//!
//! Slice 5.A defined the types; slice 5.B wires `Token` events through
//! the `chat.stream_turn` streaming action. Tool events stay unused
//! until slice 5.C lands tool decode.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Discriminator for the user-facing stream. Wire `type` strings match
/// Python's [`Core/harness/turn/_state.py`] event names exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    Token {
        turn_id: String,
        text: String,
    },
    Thinking {
        turn_id: String,
        text: String,
    },
    /// Coarse turn-phase transition (chat-processing-indicator). Purely
    /// informational — the GUI animates a Claude-style status line from
    /// these. Additive: older GUIs (whose `TurnChunk` mirror lacks the
    /// variant) decode it to `Unknown` and noop, so the stream stays
    /// backward-compatible. The driver emits at most a handful per turn
    /// (one per boundary), never per token.
    Phase {
        turn_id: String,
        phase: TurnPhase,
    },
    /// Token-usage progress (chat-processing-indicator). `done = false`
    /// is a throttled *running* completion-token tick read from the
    /// streamed Ollama frames (≈ one frame per generated token); `done =
    /// true` is the authoritative end-of-stream count taken from Ollama's
    /// `prompt_eval_count` / `eval_count`. `prompt_tokens` is `None` until
    /// the final frame carries it. Additive — see [`TurnEvent::Phase`].
    Usage {
        turn_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_tokens: Option<u64>,
        completion_tokens: u64,
        done: bool,
    },
    /// One granular context-gather step (chat-processing-indicator, full
    /// visibility). Surfaces the retrieval / concept-routing / injection /
    /// memory pipeline activity as an honest, ordered log — each carries a
    /// human `summary` and an optional `detail` (counts, concept names, a
    /// degraded reason). Emitted between the `gathering_context` and
    /// `generating` phases. Additive — see [`TurnEvent::Phase`].
    Step {
        turn_id: String,
        stage: StepStage,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    TurnComplete {
        turn_id: String,
        final_message: String,
    },
    TurnAborted {
        turn_id: String,
        reason: AbortReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Coarse, ordered phases a streaming turn passes through. Maps to real
/// boundaries in the turn driver: context gather (retrieval + concept
/// routing/injection) → LLM generation → tool dispatch between rounds.
/// Finer-grained tool phases come off the disjoint tool-activity stream
/// ([`ToolEvent`]); these are the turn-level milestones the user-facing
/// status line shows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// Gathering the turn's context — RAG retrieval, concept routing and
    /// injection. Emitted before the first LLM round.
    GatheringContext,
    /// The LLM is generating (an `ollama.chat_stream` round is open).
    Generating,
    /// Recovered tool calls are being dispatched between generation rounds.
    RunningTools,
}

/// Which slice of the context-gather pipeline a [`TurnEvent::Step`] reports.
/// The GUI groups the activity log by these (Claude-style sections).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStage {
    /// RAG / workspace retrieval (snippets, notes, persona, active-file boost).
    Retrieval,
    /// Concept routing — the candidate concepts the query mapped to.
    Routing,
    /// Concept injection — the curated concept definitions folded into context.
    Injection,
    /// Memory slots — prior-turn history, long-term, working, workspace memory.
    Memory,
    /// Resolved code-symbol / anchor context.
    Symbol,
    /// A degrade / fallback notice (workspace unreachable, tier-7 shrink).
    Notice,
}

/// Discriminator for the tool-activity stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolEvent {
    ToolDispatched {
        turn_id: String,
        call_id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        turn_id: String,
        call_id: String,
        name: String,
        output: Value,
        duration_ms: f64,
    },
    ToolError {
        turn_id: String,
        call_id: String,
        name: String,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<ToolErrorReason>,
        duration_ms: f64,
    },
    /// Emitted when a tool result represents a memory write (canonical
    /// memory-tool ids enumerated in Python's `_MEMORY_WRITE_TOOL_IDS`).
    /// Slice 5.C wires the detection; for now the variant exists so
    /// downstream consumers can keep their match arms exhaustive.
    MemoryWritten {
        turn_id: String,
        source: MemoryWriteSource,
        scope: MemoryWriteScope,
        memory_id: String,
        /// Truncated to 200 chars on the Python side; Rust mirrors that
        /// in slice 5.C.
        body: String,
        importance: Option<f64>,
        timestamp_ms: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// End-of-turn architectural-check finding (wylde_check sweep).
    /// Slice 5.C emits these.
    ToolWarning {
        turn_id: String,
        source: String,
        findings: Vec<Value>,
        truncated: bool,
        files_checked: usize,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    Cancelled,
    Error,
    ToolLoopLimit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorReason {
    TierReadOnly,
    TierToolUseBlockedDestructive,
    ToolCallTextUnrecognised,
    ToolCallTextDuplicate,
    /// Phase 12.2 consent gate — the tool has no stored decision; the
    /// dispatcher returned a `consent_required` error and the GUI is
    /// expected to surface the prompt and call `consent.respond`.
    ConsentRequired,
    /// Phase 12.2 consent gate — the user previously denied this tool;
    /// dispatch is refused without prompting.
    ConsentDenied,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteSource {
    LlmTool,
    Auto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteScope {
    LongTerm,
    Workspace,
    ShortTerm,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_event_serialises_with_flat_type_field() {
        // Long-poll wire shape: `{type: "token", turn_id, text}` —
        // i.e. type alongside fields (not nested). Matches Python
        // `_chat.py::_stream` which does `{"type": ev.type, **ev.data}`.
        let ev = TurnEvent::Token {
            turn_id: "abc".into(),
            text: "hello".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "token");
        assert_eq!(v["turn_id"], "abc");
        assert_eq!(v["text"], "hello");
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn turn_aborted_skips_optional_error_field() {
        let ev = TurnEvent::TurnAborted {
            turn_id: "abc".into(),
            reason: AbortReason::Cancelled,
            error: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "turn_aborted");
        assert_eq!(v["reason"], "cancelled");
        assert!(!v.as_object().unwrap().contains_key("error"));
    }

    #[test]
    fn tool_dispatched_carries_call_args_verbatim() {
        let ev = ToolEvent::ToolDispatched {
            turn_id: "t".into(),
            call_id: "c".into(),
            name: "fs.read".into(),
            args: serde_json::json!({"path": "foo.txt"}),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "tool_dispatched");
        assert_eq!(v["args"]["path"], "foo.txt");
    }

    #[test]
    fn phase_event_serialises_with_snake_case_phase() {
        // Chat-processing-indicator: the GUI reads `{type:"phase",
        // turn_id, phase}` to drive the animated status line.
        let ev = TurnEvent::Phase {
            turn_id: "t".into(),
            phase: TurnPhase::GatheringContext,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "phase");
        assert_eq!(v["turn_id"], "t");
        assert_eq!(v["phase"], "gathering_context");
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn usage_event_omits_prompt_tokens_until_known() {
        // Running tick: completion count only, prompt unknown mid-stream.
        let tick = TurnEvent::Usage {
            turn_id: "t".into(),
            prompt_tokens: None,
            completion_tokens: 12,
            done: false,
        };
        let v = serde_json::to_value(&tick).unwrap();
        assert_eq!(v["type"], "usage");
        assert_eq!(v["completion_tokens"], 12);
        assert_eq!(v["done"], false);
        assert!(!v.as_object().unwrap().contains_key("prompt_tokens"));

        // Final frame: authoritative counts from Ollama's eval fields.
        let done = TurnEvent::Usage {
            turn_id: "t".into(),
            prompt_tokens: Some(40),
            completion_tokens: 18,
            done: true,
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["prompt_tokens"], 40);
        assert_eq!(v["completion_tokens"], 18);
        assert_eq!(v["done"], true);
    }

    #[test]
    fn step_event_serialises_and_omits_absent_detail() {
        let with = TurnEvent::Step {
            turn_id: "t".into(),
            stage: StepStage::Routing,
            summary: "Routed to 3 concepts".into(),
            detail: Some("nextcloud, ddns, vpn".into()),
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["type"], "step");
        assert_eq!(v["stage"], "routing");
        assert_eq!(v["summary"], "Routed to 3 concepts");
        assert_eq!(v["detail"], "nextcloud, ddns, vpn");

        let without = TurnEvent::Step {
            turn_id: "t".into(),
            stage: StepStage::Retrieval,
            summary: "Retrieved 8 snippets".into(),
            detail: None,
        };
        let v = serde_json::to_value(&without).unwrap();
        assert!(!v.as_object().unwrap().contains_key("detail"));
    }

    #[test]
    fn turn_complete_carries_final_message() {
        // Pins the wire shape `chat.stream_turn` emits at end-of-turn —
        // {type: "turn_complete", turn_id, final_message} flat. The
        // strangler's Python long-poll handler relies on this shape.
        let ev = TurnEvent::TurnComplete {
            turn_id: "t".into(),
            final_message: "hello world".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "turn_complete");
        assert_eq!(v["turn_id"], "t");
        assert_eq!(v["final_message"], "hello world");
        assert_eq!(v.as_object().unwrap().len(), 3);
    }
}
