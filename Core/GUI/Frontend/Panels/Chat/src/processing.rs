//! Live "processing" status for an in-flight chat turn — the
//! chat-processing-indicator. Replaces slice-5.1's static `…` bubble with a
//! Claude-style animated status line that surfaces the turn's CURRENT phase,
//! an expandable log of what the system did (context gather, concept
//! routing, tool calls, model thinking), and a live token meter.
//!
//! Signal sources (every one degrades gracefully — a missing signal hides
//! its own bit of UI, never a broken/empty section):
//!   * **phase**        — `TurnChunk::Phase` (`gathering_context` /
//!     `generating` / `running_tools`). Absent on an older harness ⇒ the
//!     generic [`ProcessingPhase::Working`].
//!   * **tool activity**— `chat.stream_tools` (`ToolChunk`): names the
//!     active tool and feeds the log. (Friendly names only — never args /
//!     output; see the tool-visibility note in the build report.)
//!   * **thinking**     — `TurnChunk::Thinking` deltas (the model's exposed
//!     reasoning), shown inside the dropdown.
//!   * **token meter**  — `TurnChunk::Usage`: a throttled running tick plus
//!     the authoritative end-of-turn total from Ollama's
//!     `prompt_eval_count` / `eval_count`.
//!
//! This module is pure data + logic (no gpui) so the phase state machine,
//! the token formatter, the activity-log folding, and graceful degradation
//! are all unit-tested without a window. `chat_panel.rs` owns the rendering
//! and the animation ticker.

use std::time::Instant;

/// High-level phase of the in-flight turn, rendered as the animated status
/// line. [`Working`](ProcessingPhase::Working) is the generic fallback used
/// before any phase signal arrives, or when the harness predates the phase
/// events entirely (graceful degradation — the indicator still animates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingPhase {
    /// Pre-signal / unknown — generic "Working".
    Working,
    /// RAG retrieval + concept routing/injection (before the first LLM round).
    GatheringContext,
    /// The LLM is generating (an `ollama.chat_stream` round is open).
    Generating,
    /// The `running_tools` phase with no specific tool named yet.
    UsingTools,
    /// A specific tool is in flight — a friendly, user-facing phrase.
    Tool(String),
}

impl ProcessingPhase {
    /// The status-line text, sans trailing dots (the renderer appends the
    /// animated ellipsis).
    pub fn label(&self) -> String {
        match self {
            ProcessingPhase::Working => "Working".to_owned(),
            ProcessingPhase::GatheringContext => "Retrieving context".to_owned(),
            ProcessingPhase::Generating => "Generating".to_owned(),
            ProcessingPhase::UsingTools => "Using tools".to_owned(),
            ProcessingPhase::Tool(phrase) => phrase.clone(),
        }
    }

    /// Map a raw `TurnChunk::Phase` wire string. An unrecognised value maps
    /// to [`Working`](ProcessingPhase::Working) so a future backend phase
    /// never breaks the indicator.
    pub fn from_wire(s: &str) -> ProcessingPhase {
        match s {
            "gathering_context" => ProcessingPhase::GatheringContext,
            "generating" => ProcessingPhase::Generating,
            "running_tools" => ProcessingPhase::UsingTools,
            _ => ProcessingPhase::Working,
        }
    }
}

/// A friendly, user-facing phrase for a raw tool id (e.g. `memory.search` →
/// "Consulting memory"). Deliberately coarse: it surfaces *what kind* of
/// work is happening, never the tool's args or output.
pub fn friendly_tool(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    let has = |needle: &str| n.contains(needle);
    if has("memory") {
        "Consulting memory".to_owned()
    } else if has("concept") {
        "Routing concepts".to_owned()
    } else if has("anchor") {
        "Resolving anchors".to_owned()
    } else if has("graph") {
        "Querying the graph".to_owned()
    } else if has("workspace") || has("rag") || has("search") || has("retriev") {
        "Searching the workspace".to_owned()
    } else if has("file") || has("fs.") || has("read") || has("dir") {
        "Reading files".to_owned()
    } else {
        // Humanise the last dotted segment: `foo.bar_baz` → "Running bar baz".
        let leaf = name.rsplit('.').next().unwrap_or(name);
        let words = leaf.replace(['_', '-'], " ");
        let words = words.trim();
        if words.is_empty() {
            "Working".to_owned()
        } else {
            format!("Running {words}")
        }
    }
}

/// One line in the expandable activity log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// A turn-phase milestone (context gather, generating, …).
    Phase,
    /// A granular context-gather step (retrieval / routing / injection / …).
    Step,
    /// A tool was dispatched.
    Tool,
    /// A tool completed successfully.
    ToolOk,
    /// A tool failed.
    ToolErr,
    /// A memory write side-effect.
    Memory,
    /// The model exposed reasoning ("thinking").
    Thinking,
}

impl ActivityKind {
    /// Which dropdown section this entry groups under (Claude-style sections,
    /// so a busy turn isn't an undifferentiated wall). `0` = Context (gather
    /// pipeline), `1` = Tools, `2` = Thinking.
    pub fn group(self) -> u8 {
        match self {
            ActivityKind::Phase | ActivityKind::Step => 0,
            ActivityKind::Tool
            | ActivityKind::ToolOk
            | ActivityKind::ToolErr
            | ActivityKind::Memory => 1,
            ActivityKind::Thinking => 2,
        }
    }
}

/// A single entry in the activity log (the dropdown). `text` is the
/// human-readable line; `detail` is optional supporting text — concept names
/// for a routing step, or a tool's (truncated) args / output for full
/// visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub kind: ActivityKind,
    pub text: String,
    pub detail: Option<String>,
}

impl ActivityEntry {
    fn new(kind: ActivityKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            detail: None,
        }
    }

    fn with_detail(kind: ActivityKind, text: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            detail,
        }
    }
}

/// Collapse a raw args/output string onto a single truncated line for the
/// activity log (full visibility, but never a wall of JSON). Whitespace is
/// flattened and the result capped at `max` chars with an ellipsis.
pub fn compact_detail(raw: &str, max: usize) -> Option<String> {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.trim();
    if flat.is_empty() || flat == "null" {
        return None;
    }
    let out = if flat.chars().count() > max {
        let mut s: String = flat.chars().take(max).collect();
        s.push('…');
        s
    } else {
        flat.to_owned()
    };
    Some(out)
}

/// Live processing state for the active turn. Reset on each new turn;
/// folded into a [`MessageActivity`] on the message when the turn settles.
#[derive(Debug, Clone)]
pub struct ProcessingState {
    pub phase: ProcessingPhase,
    pub log: Vec<ActivityEntry>,
    /// `(call_id, friendly name)` for tools dispatched but not yet resolved,
    /// so a result/error can clear the matching one and we know when the
    /// model has resumed generating.
    pub active_tools: Vec<(String, String)>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Accumulated `TurnChunk::Thinking` deltas (the model's exposed
    /// reasoning), shown in the dropdown when non-empty.
    pub thinking: String,
    pub started: Instant,
    /// Animation frame counter, advanced by the panel's ticker.
    pub tick: u64,
    /// Whether the activity dropdown is open.
    pub expanded: bool,
}

impl Default for ProcessingState {
    fn default() -> Self {
        Self {
            phase: ProcessingPhase::Working,
            log: Vec::new(),
            active_tools: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
            thinking: String::new(),
            started: Instant::now(),
            tick: 0,
            expanded: false,
        }
    }
}

impl ProcessingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Transition the high-level phase. Logs a milestone line, but never two
    /// identical phase lines in a row (the driver re-emits `generating` each
    /// tool round — we don't want a wall of duplicates).
    pub fn set_phase(&mut self, phase: ProcessingPhase) {
        if self.phase == phase {
            return;
        }
        // A specific tool phase already logged its own dispatch line; don't
        // double-log a generic phase milestone for it.
        if !matches!(phase, ProcessingPhase::Tool(_)) {
            let line = phase.label();
            let dup = matches!(self.log.last(), Some(e) if e.kind == ActivityKind::Phase && e.text == line);
            if !dup {
                self.log.push(ActivityEntry::new(ActivityKind::Phase, line));
            }
        }
        self.phase = phase;
    }

    /// A granular context-gather step (retrieval / routing / injection /
    /// memory). Logged verbatim with its optional detail (full visibility).
    pub fn on_step(&mut self, summary: impl Into<String>, detail: Option<String>) {
        self.log.push(ActivityEntry::with_detail(
            ActivityKind::Step,
            summary,
            detail,
        ));
    }

    /// A tool was dispatched. Names the phase after it (friendly) and logs the
    /// raw tool name + its (truncated) args for full visibility.
    pub fn on_tool_dispatched(&mut self, call_id: &str, name: &str, args: Option<String>) {
        self.log
            .push(ActivityEntry::with_detail(ActivityKind::Tool, name, args));
        // Keep the RAW name keyed by call_id so the result line can name it.
        self.active_tools
            .push((call_id.to_owned(), name.to_owned()));
        self.phase = ProcessingPhase::Tool(friendly_tool(name));
    }

    /// A dispatched tool resolved (`ok` = success). Logs the raw name + its
    /// (truncated) output / error, clears it from the active set, and — when
    /// nothing else is in flight — falls the phase back to generating.
    pub fn on_tool_done(&mut self, call_id: &str, ok: bool, result: Option<String>) {
        let removed = if let Some(pos) = self.active_tools.iter().position(|(id, _)| id == call_id)
        {
            Some(self.active_tools.remove(pos).1)
        } else {
            None
        };
        if let Some(name) = removed {
            let kind = if ok {
                ActivityKind::ToolOk
            } else {
                ActivityKind::ToolErr
            };
            let suffix = if ok { "done" } else { "failed" };
            self.log.push(ActivityEntry::with_detail(
                kind,
                format!("{name} — {suffix}"),
                result,
            ));
        }
        if self.active_tools.is_empty() && matches!(self.phase, ProcessingPhase::Tool(_)) {
            self.phase = ProcessingPhase::Generating;
        }
    }

    /// Record a memory-write side-effect in the log.
    pub fn on_memory_written(&mut self) {
        // Coalesce a run of writes into one line — they tend to arrive in
        // bursts and one "Saved to memory" is enough signal.
        if matches!(self.log.last(), Some(e) if e.kind == ActivityKind::Memory) {
            return;
        }
        self.log
            .push(ActivityEntry::new(ActivityKind::Memory, "Saved to memory"));
    }

    /// Append a thinking delta. Logs the "Thinking…" line once.
    pub fn on_thinking(&mut self, delta: &str) {
        if self.thinking.is_empty() && !delta.is_empty() {
            self.log
                .push(ActivityEntry::new(ActivityKind::Thinking, "Thinking"));
        }
        self.thinking.push_str(delta);
    }

    /// Apply a token-usage update.
    pub fn on_usage(&mut self, prompt_tokens: Option<u64>, completion_tokens: u64) {
        if let Some(p) = prompt_tokens {
            self.prompt_tokens = Some(p);
        }
        self.completion_tokens = Some(completion_tokens);
    }

    /// Combined prompt+completion count, if any usage has been seen.
    pub fn total_tokens(&self) -> Option<u64> {
        match (self.prompt_tokens, self.completion_tokens) {
            (None, None) => None,
            (p, c) => Some(p.unwrap_or(0) + c.unwrap_or(0)),
        }
    }

    /// One-line token meter for the status row, e.g. "1.2k tokens". `None`
    /// when no usage has arrived (so the meter stays hidden rather than
    /// showing a bogus `0`).
    pub fn token_meter(&self) -> Option<String> {
        let total = self.total_tokens()?;
        Some(format!("{} tokens", fmt_tokens(total)))
    }

    /// Detailed prompt/completion split for the dropdown, when both are
    /// known (e.g. "40 in · 18 out").
    pub fn token_detail(&self) -> Option<String> {
        match (self.prompt_tokens, self.completion_tokens) {
            (Some(p), Some(c)) => Some(format!("{} in · {} out", fmt_tokens(p), fmt_tokens(c))),
            _ => None,
        }
    }

    /// Fold the live state into the compact form persisted on the finished
    /// assistant message so the bubble keeps an expandable activity
    /// disclosure after the turn.
    pub fn into_message_activity(self) -> MessageActivity {
        MessageActivity {
            log: self.log,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
        }
    }
}

/// The activity log + token totals persisted onto a settled assistant
/// message, powering the post-turn "Activity" disclosure.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageActivity {
    pub log: Vec<ActivityEntry>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

impl MessageActivity {
    /// Nothing worth showing — no tools ran, no thinking, no token counts.
    /// The bubble hides the disclosure entirely (graceful degradation).
    pub fn is_empty(&self) -> bool {
        self.log.is_empty() && self.prompt_tokens.is_none() && self.completion_tokens.is_none()
    }

    /// Number of tools that ran this turn (dispatch lines in the log).
    pub fn tool_count(&self) -> usize {
        self.log
            .iter()
            .filter(|e| e.kind == ActivityKind::Tool)
            .count()
    }

    /// Collapsed one-line summary, e.g. "Used 2 tools · 1.2k tokens" or just
    /// "1.2k tokens". Empty string when [`is_empty`](MessageActivity::is_empty).
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let tools = self.tool_count();
        if tools == 1 {
            parts.push("Used 1 tool".to_owned());
        } else if tools > 1 {
            parts.push(format!("Used {tools} tools"));
        }
        if let Some(total) = match (self.prompt_tokens, self.completion_tokens) {
            (None, None) => None,
            (p, c) => Some(p.unwrap_or(0) + c.unwrap_or(0)),
        } {
            parts.push(format!("{} tokens", fmt_tokens(total)));
        }
        if parts.is_empty() {
            // Log has thinking / memory lines but no tools or tokens.
            "Activity".to_owned()
        } else {
            parts.join("  ·  ")
        }
    }
}

/// Compact token formatter: `0`–`999` verbatim, then `1.2k`, `12.3k`,
/// `1.0M`. Keeps the meter narrow enough for the status row.
pub fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Which of the three status dots is bright this animation frame (`0..3`).
/// A pure function of the tick counter so the bounce is deterministic and
/// testable.
pub fn active_dot(tick: u64) -> usize {
    (tick % 3) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_from_wire_maps_known_and_falls_back() {
        assert_eq!(
            ProcessingPhase::from_wire("gathering_context"),
            ProcessingPhase::GatheringContext
        );
        assert_eq!(
            ProcessingPhase::from_wire("generating"),
            ProcessingPhase::Generating
        );
        assert_eq!(
            ProcessingPhase::from_wire("running_tools"),
            ProcessingPhase::UsingTools
        );
        // Unknown / future phase degrades to the generic label.
        assert_eq!(
            ProcessingPhase::from_wire("teleporting"),
            ProcessingPhase::Working
        );
    }

    #[test]
    fn friendly_tool_names_are_user_facing() {
        assert_eq!(friendly_tool("memory.search"), "Consulting memory");
        assert_eq!(friendly_tool("concept.route"), "Routing concepts");
        assert_eq!(
            friendly_tool("workspace.rag_query"),
            "Searching the workspace"
        );
        assert_eq!(friendly_tool("fs.read_file"), "Reading files");
        // Unknown id → humanised leaf, never the raw dotted name.
        assert_eq!(
            friendly_tool("widget.frobnicate_thing"),
            "Running frobnicate thing"
        );
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_200), "1.2k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn active_dot_cycles_three_frames() {
        assert_eq!(active_dot(0), 0);
        assert_eq!(active_dot(1), 1);
        assert_eq!(active_dot(2), 2);
        assert_eq!(active_dot(3), 0);
    }

    #[test]
    fn idle_to_phases_drives_status_line() {
        let mut p = ProcessingState::new();
        // Idle / pre-signal.
        assert_eq!(p.phase, ProcessingPhase::Working);
        assert_eq!(p.phase.label(), "Working");

        p.set_phase(ProcessingPhase::GatheringContext);
        assert_eq!(p.phase.label(), "Retrieving context");
        p.set_phase(ProcessingPhase::Generating);
        assert_eq!(p.phase.label(), "Generating");

        // Phases logged as milestones, in order, no duplicates.
        let phase_lines: Vec<&str> = p
            .log
            .iter()
            .filter(|e| e.kind == ActivityKind::Phase)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(phase_lines, vec!["Retrieving context", "Generating"]);
    }

    #[test]
    fn repeated_generating_phase_is_not_relogged() {
        let mut p = ProcessingState::new();
        p.set_phase(ProcessingPhase::Generating);
        p.set_phase(ProcessingPhase::Generating);
        let count = p
            .log
            .iter()
            .filter(|e| e.kind == ActivityKind::Phase && e.text == "Generating")
            .count();
        assert_eq!(count, 1, "identical consecutive phase not duplicated");
    }

    #[test]
    fn tool_dispatch_then_done_names_and_reverts_phase() {
        let mut p = ProcessingState::new();
        p.set_phase(ProcessingPhase::Generating);
        p.on_tool_dispatched(
            "c1",
            "memory.search",
            Some("{\"query\":\"vpn\"}".to_owned()),
        );
        // Friendly phase for the animated line…
        assert_eq!(
            p.phase,
            ProcessingPhase::Tool("Consulting memory".to_owned())
        );
        assert_eq!(p.active_tools.len(), 1);

        p.on_tool_done("c1", true, Some("{\"hits\":3}".to_owned()));
        // No tools left → back to generating.
        assert_eq!(p.phase, ProcessingPhase::Generating);
        assert!(p.active_tools.is_empty());

        // …but the LOG shows the raw tool name + args/output (full visibility).
        let dispatch = p.log.iter().find(|e| e.kind == ActivityKind::Tool).unwrap();
        assert_eq!(dispatch.text, "memory.search");
        assert_eq!(dispatch.detail.as_deref(), Some("{\"query\":\"vpn\"}"));
        let done = p
            .log
            .iter()
            .find(|e| e.kind == ActivityKind::ToolOk)
            .unwrap();
        assert_eq!(done.text, "memory.search — done");
        assert_eq!(done.detail.as_deref(), Some("{\"hits\":3}"));
    }

    #[test]
    fn concurrent_tools_hold_phase_until_all_resolve() {
        let mut p = ProcessingState::new();
        p.on_tool_dispatched("a", "memory.search", None);
        p.on_tool_dispatched("b", "workspace.rag_query", None);
        p.on_tool_done("a", true, None);
        // One still in flight → stays a tool phase.
        assert!(matches!(p.phase, ProcessingPhase::Tool(_)));
        p.on_tool_done("b", false, None);
        assert_eq!(p.phase, ProcessingPhase::Generating);
    }

    #[test]
    fn step_entries_log_with_detail_and_group_as_context() {
        let mut p = ProcessingState::new();
        p.on_step("Retrieved 8 workspace snippets", None);
        p.on_step(
            "Routed to 3 concepts",
            Some("nextcloud, ddns, vpn".to_owned()),
        );
        let routing = p.log.iter().find(|e| e.text.starts_with("Routed")).unwrap();
        assert_eq!(routing.kind, ActivityKind::Step);
        assert_eq!(routing.detail.as_deref(), Some("nextcloud, ddns, vpn"));
        // Steps group under Context (0); tools under Tools (1).
        assert_eq!(ActivityKind::Step.group(), 0);
        assert_eq!(ActivityKind::Tool.group(), 1);
        assert_eq!(ActivityKind::Thinking.group(), 2);
    }

    #[test]
    fn compact_detail_flattens_truncates_and_drops_null() {
        assert_eq!(compact_detail("null", 80), None);
        assert_eq!(compact_detail("  \n ", 80), None);
        assert_eq!(
            compact_detail("{\n  \"q\": \"hi\"\n}", 80),
            Some("{ \"q\": \"hi\" }".to_owned())
        );
        let long = "x".repeat(200);
        let got = compact_detail(&long, 10).unwrap();
        assert_eq!(got.chars().count(), 11); // 10 + ellipsis
        assert!(got.ends_with('…'));
    }

    #[test]
    fn thinking_accumulates_and_logs_once() {
        let mut p = ProcessingState::new();
        p.on_thinking("Let me ");
        p.on_thinking("think.");
        assert_eq!(p.thinking, "Let me think.");
        let n = p
            .log
            .iter()
            .filter(|e| e.kind == ActivityKind::Thinking)
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn token_meter_hidden_until_usage_then_formats() {
        let mut p = ProcessingState::new();
        // Graceful degradation: no usage yet → no meter.
        assert_eq!(p.token_meter(), None);
        assert_eq!(p.token_detail(), None);

        // Running tick: completion only.
        p.on_usage(None, 800);
        assert_eq!(p.token_meter(), Some("800 tokens".to_owned()));
        assert_eq!(p.token_detail(), None);

        // Authoritative final: prompt + completion.
        p.on_usage(Some(1_500), 900);
        assert_eq!(p.token_meter(), Some("2.4k tokens".to_owned()));
        assert_eq!(p.token_detail(), Some("1.5k in · 900 out".to_owned()));
    }

    #[test]
    fn message_activity_summary_and_emptiness() {
        // Empty → hidden.
        let empty = MessageActivity::default();
        assert!(empty.is_empty());

        // Fold a finished turn.
        let mut p = ProcessingState::new();
        p.on_tool_dispatched("c1", "memory.search", None);
        p.on_tool_done("c1", true, None);
        p.on_tool_dispatched("c2", "fs.read_file", None);
        p.on_tool_done("c2", true, None);
        p.on_usage(Some(1_000), 200);
        let ma = p.into_message_activity();
        assert!(!ma.is_empty());
        assert_eq!(ma.tool_count(), 2);
        assert_eq!(ma.summary(), "Used 2 tools  ·  1.2k tokens");
    }

    #[test]
    fn message_activity_tokens_only_summary() {
        let ma = MessageActivity {
            log: vec![ActivityEntry::new(ActivityKind::Phase, "Generating")],
            prompt_tokens: Some(40),
            completion_tokens: Some(18),
        };
        assert_eq!(ma.summary(), "58 tokens");
        assert!(!ma.is_empty());
    }
}
