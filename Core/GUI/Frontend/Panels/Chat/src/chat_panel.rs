//! Chat panel View — slice 5.1 polish edition.
//!
//! Architecture:
//!   * `messages`               — durable message list rendered as
//!     bubbles.  User on the right, assistant on the left.  Assistant
//!     content goes through the markdown renderer.
//!   * `streaming_*`            — partial chunks accumulating on the
//!     in-flight assistant bubble.  Flushed on `turn_complete` /
//!     `turn_aborted`.
//!   * `active_turn_id`         — None when idle, Some during a turn.
//!   * `prompt_input`           — `Entity<TextInput>` from
//!     `wylde-gpui-input`.  Replaces the slice-5 hand-rolled key
//!     dispatch.  Multi-line; Enter submits, Shift+Enter newline.
//!   * `workspaces` / `models`  — MRU + picker state for the inference
//!     bar's two pills.
//!   * `processing`             — live status for the in-flight turn
//!     (chat-processing-indicator): current phase, an activity log, the
//!     token meter, and the thinking buffer, fed by both `chat.stream_turn`
//!     (phase / usage / thinking) and `chat.stream_tools` (tool activity).
//!     Drives the animated indicator that replaces the old static `…`, and
//!     is folded onto the assistant bubble as a collapsible disclosure when
//!     the turn settles.  Tool calls still never become bubbles (the
//!     `wylde_inference_bar_scope` rule) — they surface only as friendly
//!     activity-log lines, never raw args/output.
//!   * `active_stream` /
//!     `active_tool_stream`      — the two open `PipeStream`s for the
//!     in-flight turn.  Dropping them cancels server-side handlers; the
//!     Stop button also fires `chat.cancel` unary so the server can free
//!     resources synchronously rather than waiting on the close-detect.
//!   * `pending_consents` +
//!     `consent_stream`         — inline consent cards surfaced from a
//!     long-lived `consent.stream_pending` subscription.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    div, list, prelude::*, px, rgb, AnyElement, AnyView, App, AppContext, AsyncApp, Context,
    ElementId, Entity, FocusHandle, Focusable, FollowMode, FontWeight, IntoElement, KeyDownEvent,
    ListAlignment, ListState, MouseDownEvent, Render, SharedString, Stateful, Subscription,
    WeakEntity, Window,
};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_700, SURFACE_800,
    SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::composer::{self, ComposerState, IgnoreTierTag, WordRecognition};
use crate::ipc::{
    activate_workspace, cancel_turn, clear_working_memory, delete_conversation, eject_model,
    export_conversation, fetch_conversation_messages, fetch_working_memory,
    get_active_conversation, get_active_conversation_for_workspace, import_conversation,
    list_conversations, list_conversations_for_workspace, list_models, new_conversation,
    reasoning_fit_check, reasoning_settings, recent_workspaces, respond_consent,
    set_active_conversation, set_active_conversation_for_workspace, set_active_model,
    set_active_workspace, set_reasoning_mode, set_workspace_for_conversation,
    start_turn_with_model, stream_consent_pending, stream_tools, stream_turn, ConsentEvent,
    ConversationMeta, PendingConsent, ToolChunk, TurnChunk, WorkingMemoryEntry, WorkspaceSummary,
};
use crate::markdown;
use crate::processing::{self, MessageActivity, ProcessingPhase, ProcessingState};
use wylde_gui_controls::control;

const WORKSPACE_MRU_LIMIT: u32 = 5;

/// How often the processing-indicator animation advances (the bouncing-dot
/// frame + any elapsed display). Cheap: one `cx.notify()` per tick, only
/// while a turn is in flight.
const PROCESSING_TICK: Duration = Duration::from_millis(360);

/// Process-wide shared [`ChatPanel`] singleton (UX rework decision 6). Holds a
/// weak handle so the entity is freed if every surface that renders it unmounts;
/// [`ChatPanel::shared`] rebuilds it on the next request. Lazily created on
/// first use, like the cross-panel buses.
fn shared_cell() -> &'static Mutex<Option<WeakEntity<ChatPanel>>> {
    static SHARED: OnceLock<Mutex<Option<WeakEntity<ChatPanel>>>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(None))
}

/// Process-wide [`ChatScope::Docked`] singleton — the Workspaces InferenceBar
/// dock's own [`ChatPanel`], kept in a cell SEPARATE from [`shared_cell`] so the
/// dock and the Chat slot are distinct live entities with independent
/// conversation scope (C1 un-shares them). Same weak-handle / lazy-rebuild
/// discipline as the global cell.
fn docked_cell() -> &'static Mutex<Option<WeakEntity<ChatPanel>>> {
    static DOCKED: OnceLock<Mutex<Option<WeakEntity<ChatPanel>>>> = OnceLock::new();
    DOCKED.get_or_init(|| Mutex::new(None))
}

/// Consent-subscription reconnect backoff floor.  First retry after a
/// failed/dropped `consent.stream_pending` waits this long.
const CONSENT_RECONNECT_MIN: Duration = Duration::from_millis(250);

/// Consent-subscription reconnect backoff ceiling.  The exponential
/// backoff never waits longer than this between attempts, so the stream
/// recovers within a few seconds of the harness coming up without
/// hammering the pipe during a long outage.
const CONSENT_RECONNECT_MAX: Duration = Duration::from_secs(5);

/// Next backoff in the consent-reconnect schedule: double the previous
/// wait, capped at [`CONSENT_RECONNECT_MAX`].  Pure so the schedule is
/// unit-testable without driving the gpui executor.
fn next_consent_backoff(prev: Duration) -> Duration {
    let doubled = prev.saturating_mul(2);
    if doubled > CONSENT_RECONNECT_MAX {
        CONSENT_RECONNECT_MAX
    } else {
        doubled
    }
}

/// One rendered chat message.  Pipes don't carry a "message id" today;
/// we mint one locally so the View key stays stable across renders.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    pub content: String,
    /// Optional "thinking" extension that the model surfaced before
    /// the final answer.  Rendered as a smaller block above `content`.
    pub thinking: Option<String>,
    /// `true` while the chunk loop is still appending tokens to this
    /// bubble.  The bubble shows a typing indicator until this clears.
    pub streaming: bool,
    /// Activity log + token totals folded from the live processing state
    /// when this turn settled (chat-processing-indicator). `None` on user /
    /// system messages and on assistant turns with nothing worth showing;
    /// `Some` powers the collapsible "Activity" disclosure on the bubble.
    pub activity: Option<MessageActivity>,
    /// Whether this bubble's activity disclosure is expanded.
    pub activity_expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    /// System-level info — "Ollama offline", inline errors.  Rendered
    /// centred and muted; never sent to the harness.
    System,
}

impl ChatMessage {
    fn user(content: String) -> Self {
        Self {
            id: new_message_id(),
            role: MessageRole::User,
            content,
            thinking: None,
            streaming: false,
            activity: None,
            activity_expanded: false,
        }
    }

    fn assistant_streaming() -> Self {
        Self {
            id: new_message_id(),
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            streaming: true,
            activity: None,
            activity_expanded: false,
        }
    }

    /// Rehydrate a persisted message when switching to a conversation.
    /// `role` is the wire role string from the stored document; only
    /// `user`/`assistant` survive a save (system messages are stripped),
    /// so anything that isn't `user` renders as the assistant.
    fn loaded(role: &str, content: String) -> Self {
        let role = if role == "user" {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        Self {
            id: new_message_id(),
            role,
            content,
            thinking: None,
            streaming: false,
            activity: None,
            activity_expanded: false,
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Which surface a [`ChatPanel`] backs, and thus which process-wide scope the
/// entity owns. Two distinct entities live at once: the Chat slot's [`Global`]
/// singleton ([`shared_cell`]) and the Workspaces dock's [`Docked`] singleton
/// ([`docked_cell`]). They hold independent conversations, so the two surfaces
/// can show different threads with different scope — C1 un-shares the dock from
/// the old single `ChatPanel::shared` entity.
///
/// [`Global`]: ChatScope::Global
/// [`Docked`]: ChatScope::Docked
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatScope {
    /// The Chat panel slot — an *unbound* conversation (layers 1 + 3). Owns the
    /// single process-wide consent subscription and restores the global
    /// active-conversation pointer.
    #[default]
    Global,
    /// The Workspaces InferenceBar dock — *bound* to a workspace's scope
    /// (wired in C3+). Does NOT subscribe consent (the Global singleton owns
    /// that one subscription) and restores a per-workspace pointer (C7; stubbed
    /// to the default conversation until then).
    Docked,
}

impl ChatScope {
    /// Only the Global singleton wires the process-wide consent stream, so the
    /// just-merged single-consent invariant (UX rework decision 6) survives the
    /// dock owning its own entity: mounting both surfaces yields exactly one
    /// `consent.stream_pending` subscription, never two competing prompts.
    pub fn wires_consent(self) -> bool {
        matches!(self, ChatScope::Global)
    }

    /// D1: only a *bound* surface — the Workspaces dock ([`ChatScope::Docked`])
    /// — may carry a `workspace_id`. The Global Chat slot is **structurally
    /// unbound**: there is no escape hatch to attach a workspace to it, so this
    /// returns `false` for `Global`. Render code hides the workspace pills on
    /// Global; the setters and the turn-send read both force the id to `None`
    /// through [`resolve_workspace_id`] so the field can never drift.
    ///
    /// [`resolve_workspace_id`]: ChatScope::resolve_workspace_id
    pub fn allows_workspace_bind(self) -> bool {
        matches!(self, ChatScope::Docked)
    }

    /// The workspace id this surface may actually use, given a `candidate`
    /// (a picked folder, a dropdown selection, a restored pointer, or the
    /// value read at turn-send). On `Global` this is always `None` (D1 — no
    /// opt-in); on `Docked` the candidate passes through unchanged.
    pub fn resolve_workspace_id(self, candidate: Option<String>) -> Option<String> {
        if self.allows_workspace_bind() {
            candidate
        } else {
            None
        }
    }
}

/// Per-turn reasoning depth — the thinking TIERS (modelled on Claude's
/// think / think-harder / ultrathink levels). Surfaced as a cycling pill
/// in the InferenceBar per the maintainer's confirmed placement.
///
/// **`Fast` is the default** — never planning-by-default (the reasoning
/// tax). Each click cycles one tier up: fast → think → think harder →
/// ultrathink → fast. The wire token rides the send payload as `depth`;
/// the harness maps tiers to plan-call deliberation budgets (`think` runs
/// the planner with deliberation off — seconds; `ultrathink` deliberates
/// at length — up to ~a minute).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningDepth {
    /// The everyday fast-model ReAct loop — no reasoning tax. Default.
    #[default]
    Fast,
    /// Plan grammar-first, deliberation off (~seconds).
    Think,
    /// Plan with a bounded deliberation budget (tens of seconds).
    ThinkHarder,
    /// Plan with the heavy-rumination budget (up to ~a minute).
    Ultrathink,
}

impl ReasoningDepth {
    /// The next tier up (wrapping) — used by the cycling pill.
    pub fn toggled(self) -> Self {
        match self {
            ReasoningDepth::Fast => ReasoningDepth::Think,
            ReasoningDepth::Think => ReasoningDepth::ThinkHarder,
            ReasoningDepth::ThinkHarder => ReasoningDepth::Ultrathink,
            ReasoningDepth::Ultrathink => ReasoningDepth::Fast,
        }
    }

    /// Wire token — matches the harness's `Depth::parse` strings.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningDepth::Fast => "fast",
            ReasoningDepth::Think => "think",
            ReasoningDepth::ThinkHarder => "think_harder",
            ReasoningDepth::Ultrathink => "ultrathink",
        }
    }

    /// Human label for the pill (the wire token reads awkwardly).
    pub fn label(self) -> &'static str {
        match self {
            ReasoningDepth::Fast => "fast",
            ReasoningDepth::Think => "think",
            ReasoningDepth::ThinkHarder => "think harder",
            ReasoningDepth::Ultrathink => "ultrathink",
        }
    }
}

/// Split vs Single reasoning mode (agentic reasoning S1 — the maintainer's confirmed
/// InferenceBar placement, scope DECISION #11). Mirrors the harness's
/// `ReasonMode`; the pill is a facade over `settings.reasoning.{get,set}` so
/// the harness-owned store stays the single source of truth. Defaults to
/// `Single` (the maintainer 2026-07-13: PLAN and EXECUTE run on the same model).
/// Inert while the reasoning master toggle is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasonMode {
    /// fast slot ≠ reasoner slot — reason once, execute on fast.
    Split,
    /// fast slot == reasoner slot — one brain plans and executes. Default.
    #[default]
    Single,
}

impl ReasonMode {
    /// The other mode — used by the toggle pill.
    pub fn toggled(self) -> Self {
        match self {
            ReasonMode::Split => ReasonMode::Single,
            ReasonMode::Single => ReasonMode::Split,
        }
    }

    /// Short wire/label token (`"split"` / `"single"`).
    pub fn as_str(self) -> &'static str {
        match self {
            ReasonMode::Split => "split",
            ReasonMode::Single => "single",
        }
    }

    /// Tolerant wire parse; unknown values keep the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "split" => Some(ReasonMode::Split),
            "single" => Some(ReasonMode::Single),
            _ => None,
        }
    }
}

/// Root Chat panel.
pub struct ChatPanel {
    pub focus_handle: FocusHandle,
    /// Which surface this entity backs (Chat slot vs Workspaces dock). Drives
    /// per-mode wiring: only [`ChatScope::Global`] subscribes consent, and each
    /// mode restores its own session pointer.
    pub scope: ChatScope,
    pub messages: Vec<ChatMessage>,
    /// Virtualized backing store for the message log. The log renders through
    /// gpui's [`gpui::list`], which paints only the items in (and just around)
    /// the viewport — so a long conversation costs a bounded number of bubble
    /// builds per frame instead of one per message. Bubbles are variable height
    /// (message length, thinking block, markdown), so this is `list`/[`ListState`]
    /// (measured items) rather than `uniform_list`. [`ChatPanel::sync_message_list`]
    /// keeps its item count + measurements in lock-step with `messages` before
    /// each paint; [`ListAlignment::Top`] keeps short logs pinned to the top
    /// (identical to the old flex column) while [`FollowMode::Tail`] gives
    /// stick-to-bottom while streaming.
    pub message_list: ListState,
    /// Id of `messages[0]` as of the last [`sync_message_list`] pass. Lets the
    /// sync tell a pure append within the current thread (same head → `splice`
    /// the tail delta, preserve scroll) from a wholesale swap (head changed →
    /// `reset` + re-engage tail-follow so a switched/loaded thread opens pinned
    /// to its newest message).
    ///
    /// [`sync_message_list`]: ChatPanel::sync_message_list
    list_head_id: Option<String>,
    /// Cheap signature of the tail bubble's rendered size inputs (content +
    /// thinking length, plus the streaming flag) at the last sync. A streaming
    /// bubble grows token-by-token at a fixed item count, so the list must
    /// remeasure that one item (`remeasure_items`, which preserves the scroll
    /// anchor) whenever this changes — including the final non-streaming swap
    /// `TurnComplete` may apply.
    list_tail_sig: usize,
    pub active_turn_id: Option<String>,
    pub conversation_id: String,
    /// Conversation switcher (Memory Slice B): the saved-chat list for the
    /// rail, whether the rail is open, and — when the user clicks a row's
    /// delete affordance — the id awaiting an inline delete confirmation.
    pub conversations: Vec<ConversationMeta>,
    pub show_conversations: bool,
    /// Outcome of the last export/import (Slice J) — one quiet line in the
    /// conversations rail; `Ok` info / `Err` failure.
    pub transfer_status: Option<Result<String, String>>,
    pub confirm_delete: Option<String>,
    /// Short-term ("working memory") buffer for the active conversation.
    /// Loaded on mount and refreshed after each turn — the chat-turn
    /// driver appends entries server-side as it works.  Rendered in the
    /// collapsible strip toggled by [`ChatPanel::show_working_memory`].
    pub working_memory: Vec<WorkingMemoryEntry>,
    /// Whether the working-memory strip above the InferenceBar is open.
    pub show_working_memory: bool,
    pub workspaces: Vec<WorkspaceSummary>,
    pub active_workspace_id: Option<String>,
    /// C6 empty-state *lazy* bind. When a workspace is entered with no existing
    /// threads, the dock mints a fresh **fileless** conversation (so the
    /// switcher stays empty and merely peeking into a workspace never litters a
    /// bound thread) and parks the entered `workspace_id` here. The *first send*
    /// then binds that thread to the workspace via `set_workspace_for_conversation`
    /// so it joins the scoped list (C4) and routes reflection to workspace
    /// memory (C8). Cleared once bound, and whenever the active thread changes
    /// to an already-known one (select / new / delete). Only ever `Some` on a
    /// Docked dock — Global is structurally unbound (D1).
    pub pending_bind_workspace: Option<String>,
    pub show_ws_dropdown: bool,
    pub models: Vec<String>,
    pub active_model: Option<String>,
    pub show_model_dropdown: bool,
    /// Per-turn reasoning depth (agentic reasoning tier P1b). Defaults to
    /// [`ReasoningDepth::Fast`]; toggled from the InferenceBar pill. As of
    /// S1 the pill's value rides the send payload as `depth` — the harness
    /// parses + logs it; `fast` (the default) stays byte-identical, and the
    /// Deep pipeline itself lands in S3.
    pub reasoning_depth: ReasoningDepth,
    /// Split/Single selector state (agentic reasoning S1) — a facade over
    /// the harness `settings.reasoning` store, hydrated on mount and
    /// persisted on toggle. Inert while the reasoning master toggle is off.
    pub reason_mode: ReasonMode,
    /// The inline VRAM fit-chip text (readiness-chip pattern): the first
    /// warning from `reasoning.fit_check`, `None` when the slot set fits or
    /// the probe soft-failed.
    pub fit_warning: Option<String>,
    /// In-flight latch for the eject button — set while an `ollama.eject`
    /// round-trip is pending so the button dims and ignores re-clicks.
    pub ejecting: bool,
    pub pending_consents: BTreeMap<String, PendingConsent>,
    pub error: Option<String>,
    /// Synchronous in-flight latch covering the window between a submit
    /// and `active_turn_id` becoming `Some` after the async `start_turn`
    /// round-trip.  Without it a second Enter in that window slips past
    /// the `active_turn_id.is_some()` guard and starts a duplicate turn.
    pub starting: bool,
    pub active_stream: Option<wylde_gui_pipe::PipeStream>,
    pub active_tool_stream: Option<wylde_gui_pipe::PipeStream>,
    pub consent_stream: Option<wylde_gui_pipe::PipeStream>,
    /// Live processing status for the in-flight turn (chat-processing-
    /// indicator): current phase, the activity log, the token meter, and the
    /// dropdown's expanded flag. `Some` only while a turn is active; folded
    /// onto the assistant message and cleared when the turn settles.
    pub processing: Option<ProcessingState>,
    pub prompt_input: Entity<TextInput>,
    /// Held to keep the input → panel subscription alive for the
    /// lifetime of the panel.
    _input_sub: Subscription,
    /// Symbol-aware composer recognition state (Slice F).
    pub composer: ComposerState,
    /// The floating Thought-Bubble layer (Plan §5.2–5.5).
    pub bubbles: composer::bubbles::BubbleLayer,
    /// Bubble-op half of the unified §5.9 undo timeline (the text half
    /// lives in the prompt input's snapshot ring; both stamp from the
    /// shared clock and `on_panel_key` arbitrates by recency).
    pub bubble_undo: wylde_anchor_actions::UndoStack<composer::bubbles::BubbleOp>,
    /// True while the arbiter replays an op — suppresses re-recording and
    /// cross-stack redo invalidation from its own effects.
    replaying_undo: bool,
    /// The bubble strip's window-absolute origin, captured by its tether
    /// canvas at paint (the graph panel's CanvasRect pattern) — bubble divs
    /// and tether endpoints position from it.
    pub bubble_strip_origin: (f32, f32, f32),
    /// The Ctrl+P symbol palette's query field (rendered only while the
    /// palette is open).
    pub palette_input: Entity<TextInput>,
    _palette_sub: Subscription,
}

impl ChatPanel {
    pub fn new(scope: ChatScope, cx: &mut Context<Self>) -> Self {
        let prompt_input = cx.new(|input_cx| {
            TextInput::multi_line(input_cx)
                .with_placeholder("Send a message  ·  Enter to send, Shift+Enter for newline")
                .with_submit_mode(SubmitMode::EnterSubmits)
                .with_min_height(60.0)
                .with_max_height(180.0)
                .with_element_key("chat-prompt")
                // Typed text in true white (TEXT_PRIMARY): the composer
                // otherwise inherits gpui's dark default, which is near-
                // invisible on the bar's SURFACE_900 fill.
                .with_text_color(TEXT_PRIMARY)
                // Unified undo (§5.9): the prompt's Ctrl+Z chords bubble to
                // `on_panel_key`, which interleaves text undo with bubble
                // ops by timeline recency. Only this input opts in.
                .with_external_undo()
        });
        let prompt_input_for_submit = prompt_input.clone();
        let input_sub = cx.subscribe(
            &prompt_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| match event
            {
                InputEvent::Submit(text) => {
                    this.submit_text(text.clone(), &prompt_input_for_submit, cx);
                }
                // Symbol-aware composer (Slice F): every edit schedules a
                // debounced recognition scan.
                InputEvent::Changed(text) => {
                    // Unified linear history (§5.9): a NEW text op
                    // invalidates the bubble redo branch — but the
                    // arbiter's own undo/redo (which also emit Changed)
                    // must not.
                    if !this.replaying_undo {
                        this.bubble_undo.clear_redo();
                    }
                    this.schedule_composer_scan(text.clone(), cx);
                }
            },
        );

        // Ctrl+P symbol palette query field (Slice F).
        let palette_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_submit_mode(SubmitMode::EnterSubmits)
                .with_element_key("chat-symbol-palette")
                .with_placeholder("Search symbols…")
        });
        let palette_sub = cx.subscribe(
            &palette_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                match event {
                    InputEvent::Changed(q) => this.schedule_palette_query(q.clone(), cx),
                    // Enter accepts the current (top) hit.
                    InputEvent::Submit(_) => {
                        let name = this
                            .composer
                            .palette
                            .as_ref()
                            .and_then(|p| p.selection())
                            .map(|h| h.name.clone());
                        if let Some(name) = name {
                            this.insert_palette_reference(&name, cx);
                        }
                    }
                }
            },
        );

        Self {
            focus_handle: cx.focus_handle(),
            scope,
            messages: Vec::new(),
            // Bottom-anchored chat behaviour without a Bottom alignment: Top keeps
            // a short log pinned to the top (matching the old flex column), and
            // Tail follow snaps to / re-engages the bottom as the log grows past
            // the viewport — i.e. stick-to-bottom while streaming. The overdraw
            // measures a little above/below the fold so scrolling doesn't pop in.
            message_list: {
                let state = ListState::new(0, ListAlignment::Top, px(256.0));
                state.set_follow_mode(FollowMode::Tail);
                state
            },
            list_head_id: None,
            list_tail_sig: 0,
            active_turn_id: None,
            conversation_id: "default".to_owned(),
            conversations: Vec::new(),
            show_conversations: false,
            transfer_status: None,
            confirm_delete: None,
            working_memory: Vec::new(),
            show_working_memory: false,
            workspaces: Vec::new(),
            active_workspace_id: None,
            pending_bind_workspace: None,
            show_ws_dropdown: false,
            models: Vec::new(),
            active_model: None,
            show_model_dropdown: false,
            reasoning_depth: ReasoningDepth::Fast,
            reason_mode: ReasonMode::default(),
            fit_warning: None,
            ejecting: false,
            pending_consents: BTreeMap::new(),
            error: None,
            starting: false,
            active_stream: None,
            active_tool_stream: None,
            consent_stream: None,
            processing: None,
            prompt_input,
            _input_sub: input_sub,
            composer: ComposerState::default(),
            bubbles: composer::bubbles::BubbleLayer::default(),
            bubble_undo: wylde_anchor_actions::UndoStack::default(),
            replaying_undo: false,
            bubble_strip_origin: (0.0, 0.0, 0.0),
            palette_input,
            _palette_sub: palette_sub,
        }
    }

    /// Registry factory for the Chat slot. Resolves the **Global singleton**
    /// (see [`ChatPanel::shared`]) — the Workspaces InferenceBar dock resolves a
    /// *separate* [`ChatScope::Docked`] entity (see [`ChatPanel::docked`]), so
    /// the two surfaces hold independent conversations (C1).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        Self::shared(cx).into()
    }

    /// Resolve the process-wide **Global** ChatPanel singleton (the Chat slot),
    /// creating + wiring it on first use.
    ///
    /// This is the [`ChatScope::Global`] entity. The Workspaces dock no longer
    /// resolves here — C1 gave it its own [`ChatScope::Docked`] entity
    /// ([`ChatPanel::docked`]). The Global singleton owns the **single**
    /// process-wide consent subscription (UX rework decision 6's invariant):
    /// idempotent — a live handle is reused; a dropped one is rebuilt with the
    /// full network setup (workspaces/models/session/consent), so consent is
    /// subscribed exactly once while the entity is live.
    pub fn shared(cx: &mut App) -> Entity<ChatPanel> {
        if let Some(existing) = shared_cell()
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|weak| weak.upgrade())
        {
            return existing;
        }
        let entity = cx.new(|cx| {
            let panel = Self::new(ChatScope::Global, cx);
            // Announce the conversation this panel owns on the cross-panel
            // bus so a sibling that's already mounted (e.g. the Memory
            // panel's short-term view) reflects it immediately.  Until a
            // turn adopts a harness-minted id this is the "default"
            // conversation the working-memory pill already queries.
            wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
            Self::spawn_load_workspaces(cx);
            Self::spawn_load_models(cx);
            Self::spawn_load_reasoning(cx);
            // Restore the persisted active conversation (Slice B), then load
            // its working-memory buffer + the switcher list — sequenced in
            // one task so the WM load reads the *restored* id, not "default".
            Self::spawn_restore_session(cx);
            Self::wire_consent_for_scope(ChatScope::Global, cx);
            panel
        });
        if let Ok(mut g) = shared_cell().lock() {
            *g = Some(entity.downgrade());
        }
        entity
    }

    /// Resolve the process-wide **Docked** ChatPanel singleton (the Workspaces
    /// InferenceBar dock), creating + wiring it on first use.
    ///
    /// Distinct from [`ChatPanel::shared`]: a separate entity in
    /// [`ChatScope::Docked`] mode with its own conversation scope, so the dock
    /// and the Chat slot show independent threads (C1). Two deliberate
    /// differences from the Global wiring:
    /// 1. **No consent subscription** — the Global singleton owns the one
    ///    `consent.stream_pending` subscription (UX rework decision 6); the dock
    ///    must not double-subscribe.
    /// 2. **Per-mode restore** — the dock keeps its own (per-workspace, C7)
    ///    pointer instead of adopting the global active-conversation pointer,
    ///    and it does not announce on the active-conversation bus (that bus
    ///    tracks the Global surface the Memory panel mirrors).
    pub fn docked(cx: &mut App) -> Entity<ChatPanel> {
        if let Some(existing) = docked_cell()
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|weak| weak.upgrade())
        {
            return existing;
        }
        let entity = cx.new(|cx| {
            let panel = Self::new(ChatScope::Docked, cx);
            Self::spawn_load_workspaces(cx);
            Self::spawn_load_models(cx);
            Self::spawn_load_reasoning(cx);
            // Per-mode restore (stubbed to "default" until C7's per-workspace
            // pointer): hydrate the WM strip + switcher without adopting the
            // global active-conversation pointer.
            Self::spawn_restore_session_docked(cx);
            // C3: follow the cross-panel workspace-scope bus. Entering a
            // workspace in the Workspaces panel re-scopes this dock to that
            // `workspace_id`; leaving clears it. This is the consumer half of
            // the A1 live-bug fix — without it, dock turns stay unbound.
            Self::spawn_workspace_scope_drain(cx);
            // No-op for Docked: the Global singleton owns the single consent
            // subscription (the decision lives in `ChatScope::wires_consent`).
            Self::wire_consent_for_scope(ChatScope::Docked, cx);
            panel
        });
        if let Ok(mut g) = docked_cell().lock() {
            *g = Some(entity.downgrade());
        }
        entity
    }

    pub fn spawn_load_workspaces(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = recent_workspaces(WORKSPACE_MRU_LIMIT).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(rows) = outcome {
                    panel.workspaces = rows;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn spawn_load_models(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_models().await;
            let _ = this.update(app_cx, |panel, cx| {
                // Soft-fail: missing Ollama → no models, picker shows
                // "(auto)" only, harness's default_model still drives
                // the turn.
                if let Ok(rows) = outcome {
                    panel.models = rows;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Hydrate the Split/Single pill + fit chip from the harness-owned
    /// reasoning store (agentic reasoning S1). Soft-fail throughout: an
    /// unreachable harness leaves the defaults (Single, no chip) — the
    /// pill is inert while the master toggle is off anyway. The fit chip
    /// only renders when reasoning is enabled AND the fit probe warned,
    /// so a default install shows nothing new.
    pub fn spawn_load_reasoning(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let settings = reasoning_settings().await;
            let (mode, enabled) = match &settings {
                Ok(v) => (
                    v.get("mode")
                        .and_then(|m| m.as_str())
                        .and_then(ReasonMode::parse)
                        .unwrap_or_default(),
                    v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false),
                ),
                Err(_) => (ReasonMode::default(), false),
            };
            let fit_warning = if enabled {
                Self::fetch_fit_warning().await
            } else {
                None
            };
            let _ = this.update(app_cx, |panel, cx| {
                panel.reason_mode = mode;
                panel.fit_warning = fit_warning;
                cx.notify();
            });
        })
        .detach();
    }

    /// First `reasoning.fit_check` warning, `None` on a clean fit or any
    /// probe failure (advisory chip — never surface an error for it).
    async fn fetch_fit_warning() -> Option<String> {
        let v = reasoning_fit_check().await.ok()?;
        v.get("warnings")
            .and_then(|w| w.as_array())
            .and_then(|arr| arr.first())
            .and_then(|w| w.as_str())
            .map(str::to_owned)
    }

    /// Flip the Split/Single selector: optimistic local flip, then persist
    /// through `settings.reasoning.set` and refresh the fit chip against
    /// the new mode. A failed persist flips back on the next hydrate.
    pub fn toggle_reason_mode(&mut self, cx: &mut Context<Self>) {
        self.reason_mode = self.reason_mode.toggled();
        let mode = self.reason_mode;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let persisted = set_reasoning_mode(mode.as_str()).await;
            let enabled = persisted
                .as_ref()
                .ok()
                .and_then(|v| v.get("enabled"))
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let fit_warning = if enabled {
                Self::fetch_fit_warning().await
            } else {
                None
            };
            let _ = this.update(app_cx, |panel, cx| {
                panel.fit_warning = fit_warning;
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the working-memory buffer for the active conversation.  Run
    /// once on mount and again after each turn settles, so the strip
    /// reflects whatever the chat-turn driver appended while it worked.
    pub fn spawn_load_working_memory(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::reload_working_memory(&this, app_cx).await;
        })
        .detach();
    }

    /// Read `memory.short_term.get` for the panel's current
    /// `conversation_id` and replace the buffer.  Soft-fail: a transport
    /// error (harness still cold-starting, conversation not persisted
    /// yet) leaves the existing buffer untouched rather than surfacing a
    /// loud error in the chat surface.  Shared by the mount load, the
    /// post-turn refresh, and the clear flow.
    async fn reload_working_memory(this: &gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let Ok(cid) = this.update(app_cx, |panel, _| panel.conversation_id.clone()) else {
            return;
        };
        if let Ok(entries) = fetch_working_memory(&cid).await {
            let _ = this.update(app_cx, |panel, cx| {
                panel.working_memory = entries;
                cx.notify();
            });
        }
    }

    /// Toggle the working-memory strip.  Mutually exclusive with the two
    /// InferenceBar pickers so only one overlay is open at a time.
    pub fn toggle_working_memory(&mut self, cx: &mut Context<Self>) {
        self.show_working_memory = !self.show_working_memory;
        self.show_ws_dropdown = false;
        self.show_model_dropdown = false;
        self.show_conversations = false;
        cx.notify();
    }

    // ── Conversation switcher (Memory Slice B) ───────────────────────

    /// Restore the persisted active conversation on mount, then load its
    /// message history + working-memory buffer + the switcher list — all
    /// in one task so each step reads the *restored* id, not "default".
    pub fn spawn_restore_session(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(Some(active)) = get_active_conversation().await {
                let changed = this
                    .update(app_cx, |panel, cx| {
                        if panel.conversation_id != active {
                            panel.conversation_id = active.clone();
                            wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if changed {
                    Self::reload_conversation_messages(&this, app_cx).await;
                }
            }
            Self::reload_working_memory(&this, app_cx).await;
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// Docked-mode session restore (C1). Unlike [`spawn_restore_session`], the
    /// dock does NOT adopt the process-wide active-conversation pointer. Its
    /// per-workspace pointer (C7) is restored by the enter flow
    /// ([`spawn_enter_workspace_flow`]) once a workspace is in scope — on a bare
    /// mount no workspace is entered yet, so here it just hydrates the
    /// working-memory strip + the switcher list so the dock isn't blank, and
    /// the scope drain (seeded from the bus latch) drives the per-workspace
    /// restore when a workspace was already entered before this dock mounted.
    pub fn spawn_restore_session_docked(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::reload_working_memory(&this, app_cx).await;
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// Follow the cross-panel **workspace-scope** bus (C3) for the life of the
    /// docked dock. Seeds the dock's scope synchronously from the bus latch —
    /// covering a workspace entered *before* this dock first mounted, the
    /// late-mounter gap the bus documents — then drains the receiver so every
    /// later enter/leave re-scopes the dock live.
    ///
    /// Single-consumer, mirroring [`workspaces_panel`]'s focus drain: the dock
    /// is a process-wide singleton, so the receiver is taken exactly once. On a
    /// rebuild after the weak handle lapsed, the receiver is already gone — the
    /// latch seed alone then carries the rebuilt dock to the current scope.
    pub fn spawn_workspace_scope_drain(cx: &mut Context<Self>) {
        let seed = wylde_gui_pipe::current_active_workspace();
        let mut rx = wylde_gui_pipe::take_workspace_scope_receiver();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if this
                .update(app_cx, |panel, cx| panel.apply_workspace_scope(seed, cx))
                .is_err()
            {
                return;
            }
            // `rx` is `None` only on a rebuilt dock whose predecessor took the
            // single-consumer receiver; the latch seed above is then the best
            // available current value.
            let Some(rx) = rx.as_mut() else { return };
            while let Some(scope) = rx.recv().await {
                if this
                    .update(app_cx, |panel, cx| panel.apply_workspace_scope(scope, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    /// Apply a cross-panel workspace-scope change (C3). The Workspaces panel's
    /// `enter_workspace` publishes `Some(id)` and `leave_workspace` publishes
    /// `None`; the dock re-scopes its chat so the *next turn rides this
    /// `workspace_id`*. This is the A1 live-bug fix: before C3 nothing ever set
    /// the dock's id, so every dock turn ran on base (unbound) context.
    ///
    /// Routed through [`ChatScope::resolve_workspace_id`] so D1 holds even here:
    /// on the Global singleton it forces `None`, so the Chat slot could never
    /// become bound even if it somehow drained this bus. Only the Docked dock
    /// adopts the id. No-op (no re-render) when the scope is unchanged.
    pub fn apply_workspace_scope(
        &mut self,
        scope: wylde_gui_pipe::WorkspaceScope,
        cx: &mut Context<Self>,
    ) {
        let resolved = self.scope.resolve_workspace_id(scope);
        if self.active_workspace_id == resolved {
            return;
        }
        self.active_workspace_id = resolved.clone();
        // A scope change abandons any not-yet-bound empty-state thread; the flow
        // below recomputes it for the new scope (Empty case re-sets it).
        self.pending_bind_workspace = None;
        cx.notify();
        // C6: re-scope the dock's open thread to the entered workspace. Entering
        // (`Some`) runs the enter flow (last-open → most-recent → empty);
        // leaving (`None`, only reachable on Docked) resets to a clean unbound
        // default. Global never reaches here with a non-`None` resolution (D1),
        // and a no-op scope change short-circuited above.
        match resolved {
            Some(_) => Self::spawn_enter_workspace_flow(cx),
            None => Self::spawn_leave_workspace_reset(cx),
        }
    }

    /// C6 enter flow. Refresh the entered workspace's scoped conversation list,
    /// then open the right thread:
    /// - the per-workspace last-open pointer, if its thread is still in the list
    ///   (**has-last-open**; the pointer source lands in C7 — `None` until then);
    /// - otherwise the most-recent scoped thread (**has-threads**);
    /// - otherwise mint a fresh fileless thread and defer its workspace bind to
    ///   the first send (**none** — the empty state).
    ///
    /// The selection itself is decided by the pure [`pick_enter_conversation`]
    /// so the three cases are unit-testable without a live panel. Opening a
    /// thread switches the dock's view only — it does **not** persist the global
    /// active-conversation pointer (that would leak a bound workspace thread
    /// into the Global slot's restore); the per-workspace pointer write is C7.
    fn spawn_enter_workspace_flow(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Reflects the just-set `active_workspace_id`, so this is the scoped
            // list for the entered workspace (C4).
            Self::reload_conversations(&this, app_cx).await;
            let Ok(ws) = this.update(app_cx, |panel, _| panel.active_workspace_id.clone()) else {
                return;
            };
            // C7: the thread the user last had open *in this workspace*, if any.
            let last_open = match ws.as_deref() {
                Some(w) => get_active_conversation_for_workspace(w)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let Ok(target) = this.update(app_cx, |panel, _| {
                pick_enter_conversation(last_open.as_deref(), &panel.conversations)
            }) else {
                return;
            };
            match target {
                EnterTarget::Open(id) => {
                    let switched = this
                        .update(app_cx, |panel, cx| {
                            if panel.conversation_id == id {
                                return false;
                            }
                            panel.conversation_id = id.clone();
                            wylde_gui_pipe::publish_active_conversation(&id);
                            panel.messages.clear();
                            panel.working_memory.clear();
                            panel.bubbles.on_conversation_changed();
                            panel.bubble_undo.clear();
                            // Opening an existing thread abandons any pending
                            // empty-state bind.
                            panel.pending_bind_workspace = None;
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if switched {
                        Self::reload_conversation_messages(&this, app_cx).await;
                        Self::reload_working_memory(&this, app_cx).await;
                        // C7: remember this as the workspace's last-open thread.
                        if let Some(w) = ws.as_deref() {
                            let _ = set_active_conversation_for_workspace(w, &id).await;
                        }
                    }
                }
                EnterTarget::Empty => {
                    // Mint a fresh, *fileless* thread so the first send doesn't
                    // pollute the shared "default" doc; the bind to `ws` is
                    // deferred to that send (lazy — see `pending_bind_workspace`).
                    let fresh = new_conversation().await.ok();
                    let _ = this.update(app_cx, |panel, cx| {
                        match fresh {
                            Some(id) => {
                                panel.conversation_id = id;
                                // Bind on first send. Only set when the mint
                                // succeeded — never park a workspace on "default".
                                panel.pending_bind_workspace = ws.clone();
                            }
                            None => {
                                panel.conversation_id = "default".to_owned();
                                panel.pending_bind_workspace = None;
                            }
                        }
                        panel.messages.clear();
                        panel.working_memory.clear();
                        panel.bubbles.on_conversation_changed();
                        panel.bubble_undo.clear();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// C6 leave reset (Docked only). Returns the dock to its unbound home: the
    /// full (unscoped) conversation list and a clean default thread, dropping
    /// any pending empty-state bind. Mirrors [`spawn_restore_session_docked`]'s
    /// "stays on the default conversation" baseline.
    fn spawn_leave_workspace_reset(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = this.update(app_cx, |panel, cx| {
                if panel.conversation_id != "default" {
                    panel.conversation_id = "default".to_owned();
                    panel.messages.clear();
                    panel.working_memory.clear();
                    panel.bubbles.on_conversation_changed();
                    panel.bubble_undo.clear();
                }
                panel.pending_bind_workspace = None;
                cx.notify();
            });
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// Re-fetch the saved-chat list and replace the rail's copy. Soft-fail
    /// (a transport error leaves the existing list untouched).
    ///
    /// Source is picked by surface scope (C4): the **Docked** dock, once a
    /// workspace is entered, shows only that workspace's bound conversations;
    /// the **Global** slot keeps the full unbound list. The choice flows
    /// through [`ChatScope::resolve_workspace_id`], so a Global panel always
    /// resolves to `None` (D1) and never filters — only a Docked dock with an
    /// active `workspace_id` gets the scoped list. A Docked dock with no
    /// workspace yet (`None`) falls back to the full list until C6's
    /// empty-state lands.
    async fn reload_conversations(this: &gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let Ok(scoped_to) = this.update(app_cx, |panel, _| {
            panel
                .scope
                .resolve_workspace_id(panel.active_workspace_id.clone())
        }) else {
            return;
        };
        let rows = match scoped_to {
            Some(ws) => list_conversations_for_workspace(&ws).await,
            None => list_conversations().await,
        };
        if let Ok(rows) = rows {
            let _ = this.update(app_cx, |panel, cx| {
                panel.conversations = rows;
                cx.notify();
            });
        }
    }

    /// Rehydrate the bubble log from the active conversation's persisted
    /// `messages`. Soft-fail: a transport error leaves the current log
    /// alone; a not-yet-persisted conversation yields an empty log.
    async fn reload_conversation_messages(this: &gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let Ok(cid) = this.update(app_cx, |panel, _| panel.conversation_id.clone()) else {
            return;
        };
        if let Ok(loaded) = fetch_conversation_messages(&cid).await {
            let _ = this.update(app_cx, |panel, cx| {
                // Don't clobber an in-flight turn's live bubbles.
                if panel.active_turn_id.is_some() || panel.starting {
                    return;
                }
                panel.messages = loaded
                    .into_iter()
                    .map(|m| ChatMessage::loaded(&m.role, m.content))
                    .collect();
                cx.notify();
            });
        }
    }

    /// Toggle the conversation rail. Mutually exclusive with the other
    /// InferenceBar overlays, and closing it dismisses any pending delete
    /// confirmation so a stale prompt doesn't survive a reopen.
    pub fn toggle_conversations(&mut self, cx: &mut Context<Self>) {
        self.show_conversations = !self.show_conversations;
        if !self.show_conversations {
            self.confirm_delete = None;
        }
        self.show_ws_dropdown = false;
        self.show_model_dropdown = false;
        self.show_working_memory = false;
        cx.notify();
    }

    /// Switch the active conversation: adopt its id, announce it on the
    /// cross-panel bus (the Memory panel follows), persist the selection,
    /// and rehydrate the bubble log + working-memory buffer. No-op when
    /// it's already active (beyond closing the rail).
    pub fn select_conversation(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let switched = self.conversation_id != id;
        if switched {
            self.conversation_id = id.to_owned();
            // Switching to an existing thread abandons any empty-state lazy bind.
            self.pending_bind_workspace = None;
            wylde_gui_pipe::publish_active_conversation(&self.conversation_id);
            // Optimistically clear the old conversation's view; the reloads
            // below reconcile against the harness.
            self.messages.clear();
            self.working_memory.clear();
            // Bubble pins are per-conversation (§5.4) — a switch resets all,
            // and the §5.9 undo stack is per-conversation too.
            self.bubbles.on_conversation_changed();
            self.bubble_undo.clear();
        }
        self.show_conversations = false;
        self.confirm_delete = None;
        self.focus_prompt(window, cx);
        cx.notify();
        if !switched {
            return;
        }
        let cid = self.conversation_id.clone();
        // C7: persist the selection on the *right* pointer for this surface — a
        // Docked dock inside a workspace updates that workspace's per-workspace
        // pointer (so re-entering restores this thread); the Global slot (and a
        // Docked dock with no workspace) updates the single global pointer.
        // Routing through `scope` keeps a bound workspace thread out of the
        // Global slot's restore (D1).
        let pointer_ws = self
            .scope
            .resolve_workspace_id(self.active_workspace_id.clone());
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            match pointer_ws.as_deref() {
                Some(ws) => {
                    let _ = set_active_conversation_for_workspace(ws, &cid).await;
                }
                None => {
                    let _ = set_active_conversation(&cid).await;
                }
            }
            Self::reload_conversation_messages(&this, app_cx).await;
            Self::reload_working_memory(&this, app_cx).await;
        })
        .detach();
    }

    /// "+ New": mint a fresh conversation id, switch to it (blank log),
    /// persist it as active, and refresh the rail + cross-panel bus.
    /// The workspace a freshly minted thread should bind to (C5). Routes the
    /// current `active_workspace_id` through [`ChatScope::resolve_workspace_id`]
    /// so a Docked dock inside workspace X mints a *bound* thread (`Some(X)`),
    /// while the Global slot — structurally unbound (D1) — always yields `None`
    /// and never binds, even if a stale id somehow sat in the field. A Docked
    /// dock with no workspace entered yet also yields `None` (an unbound thread,
    /// indistinguishable from a global one until a workspace is entered).
    fn new_conversation_bind_target(&self) -> Option<String> {
        self.scope
            .resolve_workspace_id(self.active_workspace_id.clone())
    }

    pub fn spawn_new_conversation(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let Ok(id) = new_conversation().await else {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some("Couldn't start a new conversation".to_owned());
                    cx.notify();
                });
                return;
            };
            // C5: minting a thread on the Docked dock binds it to the entered
            // workspace so it joins that workspace's scoped list (C4) and routes
            // reflection to workspace memory (C8). Resolved through `scope` off
            // the live panel, so the Global slot can never bind (D1).
            let bind_workspace = this
                .update(app_cx, |panel, _cx| panel.new_conversation_bind_target())
                .ok()
                .flatten();
            // Bind the fresh thread *before* announcing/selecting it: the harness
            // upserts the doc, so this both records the `workspace_id` and lands
            // the document on disk — which is what lets the bound thread appear in
            // the workspace's scoped list immediately, before its first turn (an
            // unbound thread has no file until then). A bind failure is surfaced
            // but non-fatal: the thread is still usable, just unbound this session.
            if let Some(ws) = bind_workspace.as_deref() {
                if let Err(e) = set_workspace_for_conversation(&id, ws).await {
                    let _ = this.update(app_cx, |panel, cx| {
                        panel.error = Some(format!(
                            "Couldn't bind the new thread to this workspace: {e}"
                        ));
                        cx.notify();
                    });
                }
            }
            let _ = this.update(app_cx, |panel, cx| {
                panel.conversation_id = id.clone();
                // This path mints + binds explicitly (C5 above), so any deferred
                // empty-state bind is moot.
                panel.pending_bind_workspace = None;
                panel.messages.clear();
                panel.working_memory.clear();
                panel.show_conversations = false;
                panel.confirm_delete = None;
                wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
                cx.notify();
            });
            // C7: persist the new thread on the right pointer — a bound thread
            // becomes its workspace's last-open (so re-entering restores it),
            // an unbound one updates the global pointer.
            match bind_workspace.as_deref() {
                Some(ws) => {
                    let _ = set_active_conversation_for_workspace(ws, &id).await;
                }
                None => {
                    let _ = set_active_conversation(&id).await;
                }
            }
            // An *unbound* brand-new conversation has no file until its first turn,
            // so it won't appear in the list yet; a *bound* one was just upserted
            // by the set_workspace above and will. Either way, announce the list
            // change and refresh our own copy.
            wylde_gui_pipe::publish_conversation_list_changed();
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// Arm the inline delete confirmation for `id` (the row swaps to a
    /// "Delete? / Cancel" prompt). A second different id replaces the
    /// first, so only one confirmation is ever live.
    pub fn request_delete_conversation(&mut self, id: &str, cx: &mut Context<Self>) {
        self.confirm_delete = Some(id.to_owned());
        cx.notify();
    }

    /// Dismiss the inline delete confirmation without deleting.
    pub fn cancel_delete_conversation(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    /// Confirm the delete: remove the file, drop it from the rail, and —
    /// if it was the active conversation — fall back to the next one
    /// (newest remaining), persisting + announcing the new selection.
    pub fn confirm_delete_conversation(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.to_owned();
        self.confirm_delete = None;
        // Optimistically drop it from the rail; the reload reconciles.
        self.conversations.retain(|c| c.id != id);
        let was_active = self.conversation_id == id;
        let fallback = if was_active {
            pick_next_active(&self.conversations, &id)
        } else {
            None
        };
        if was_active {
            // The active thread is being replaced — drop any empty-state bind so
            // a later send can't bind the fallback/default to a stale workspace.
            self.pending_bind_workspace = None;
            match &fallback {
                Some(next) => {
                    self.conversation_id = next.clone();
                    self.messages.clear();
                    self.working_memory.clear();
                    wylde_gui_pipe::publish_active_conversation(&self.conversation_id);
                }
                None => {
                    // Nothing left to fall back to — start a clean default.
                    self.conversation_id = "default".to_owned();
                    self.messages.clear();
                    self.working_memory.clear();
                    wylde_gui_pipe::publish_active_conversation(&self.conversation_id);
                }
            }
        }
        cx.notify();
        let active_after = self.conversation_id.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Surface a delete failure instead of swallowing it: the row was
            // dropped optimistically above, and the reload below would
            // silently flicker it back with no explanation otherwise.
            if let Err(e) = delete_conversation(&id).await {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("delete conversation: {e}"));
                    cx.notify();
                });
            }
            if was_active {
                let _ = set_active_conversation(&active_after).await;
                Self::reload_conversation_messages(&this, app_cx).await;
                Self::reload_working_memory(&this, app_cx).await;
            }
            wylde_gui_pipe::publish_conversation_list_changed();
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// "Clear working memory" — fire `memory.short_term.clear` for the
    /// active conversation, then reload so the strip reflects the now-empty
    /// buffer.  Optimistically clears the local copy first so the UI
    /// responds immediately; the reload reconciles against the harness.
    pub fn clear_working_memory(&mut self, cx: &mut Context<Self>) {
        let cid = self.conversation_id.clone();
        self.working_memory.clear();
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = clear_working_memory(&cid).await;
            Self::reload_working_memory(&this, app_cx).await;
        })
        .detach();
    }

    /// Wire the process-wide consent subscription iff `scope` owns it
    /// ([`ChatScope::wires_consent`]). The single source of truth for the
    /// single-consent invariant (UX rework decision 6): both constructors route
    /// their consent decision through here, so the Global singleton subscribes
    /// exactly once and the Docked dock never double-subscribes.
    fn wire_consent_for_scope(scope: ChatScope, cx: &mut Context<Self>) {
        if scope.wires_consent() {
            Self::spawn_consent_subscription(cx);
        }
    }

    pub fn spawn_consent_subscription(cx: &mut Context<Self>) {
        // Cold-start tolerant.  The harness pipe may not be accepting
        // connections yet when the Chat panel first mounts (the daemon
        // is still coming up).  The pre-fix code subscribed once and, on
        // failure, set an error string and returned — leaving the
        // consent stream permanently dead.  Every pending-tool prompt
        // the harness broadcast after that was silently dropped: the
        // user saw no consent card and the turn stalled.
        //
        // Now a single long-lived task owns the subscription and retries
        // with a capped exponential backoff.  A failed subscribe (or a
        // mid-flight stream error) reconnects instead of giving up; the
        // loop only exits when the panel entity is torn down.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let mut backoff = CONSENT_RECONNECT_MIN;
            loop {
                match stream_consent_pending() {
                    Ok(mut stream) => {
                        // Connected.  Clear any cold-start error we'd
                        // surfaced and reset the backoff for the next
                        // disconnect.
                        let alive = this
                            .update(app_cx, |panel, cx| {
                                if panel
                                    .error
                                    .as_deref()
                                    .is_some_and(|e| e.starts_with("consent stream"))
                                {
                                    panel.error = None;
                                }
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                        backoff = CONSENT_RECONNECT_MIN;

                        // Drain until the stream ends (None) or errors,
                        // then fall through to the reconnect wait.
                        while let Some(chunk) = stream.recv().await {
                            match chunk {
                                Ok(value) => {
                                    let Some(ev) = ConsentEvent::from_value(&value) else {
                                        continue;
                                    };
                                    let alive = this
                                        .update(app_cx, |panel, cx| {
                                            match ev {
                                                ConsentEvent::Pending(entry) => {
                                                    panel
                                                        .pending_consents
                                                        .insert(entry.id.clone(), entry);
                                                }
                                                ConsentEvent::Resolved { id, .. } => {
                                                    panel.pending_consents.remove(&id);
                                                }
                                                ConsentEvent::Lagged | ConsentEvent::Heartbeat => {}
                                            }
                                            cx.notify();
                                        })
                                        .is_ok();
                                    // Entity gone (Shell torn down) → stop.
                                    if !alive {
                                        return;
                                    }
                                }
                                Err(_e) => {
                                    // Transient stream error → break out to
                                    // reconnect rather than dying.
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Subscribe failed — almost always the cold-start
                        // race (pipe not up yet).  Surface it once so the
                        // user knows we're retrying, then back off.
                        let alive = this
                            .update(app_cx, |panel, cx| {
                                if panel.error.is_none() {
                                    panel.error =
                                        Some(format!("consent stream: {e} (reconnecting…)"));
                                }
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                    }
                }

                // Wait before the next attempt; bail if the panel is gone
                // so a torn-down entity doesn't spin a zombie reconnect.
                // gpui's executor has no tokio reactor, so use its native
                // timer — `tokio::time::sleep` here panics ("no reactor").
                app_cx.background_executor().timer(backoff).await;
                if this.update(app_cx, |_, _| {}).is_err() {
                    return;
                }
                backoff = next_consent_backoff(backoff);
            }
        })
        .detach();
    }

    fn submit_text(&mut self, text: String, input: &Entity<TextInput>, cx: &mut Context<Self>) {
        let trimmed = text.trim().to_owned();
        // Block empty sends and double-sends: `active_turn_id` covers a
        // turn that has already started, `starting` covers the gap while
        // `start_turn` is still in flight.
        if trimmed.is_empty() || self.active_turn_id.is_some() || self.starting {
            return;
        }
        // Clear the input now — the panel takes over from here.
        input.update(cx, |i, cx| i.clear(cx));
        self.send_user_message(trimmed, cx);
    }

    /// Drive the processing-indicator animation: advance the tick + notify
    /// every [`PROCESSING_TICK`] while a turn is in flight. Exits the moment
    /// `processing` clears (turn settled) or the entity is gone, so there's
    /// never a runaway timer between turns.
    fn spawn_processing_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            app_cx.background_executor().timer(PROCESSING_TICK).await;
            let keep_going = this.update(app_cx, |panel, cx| match panel.processing.as_mut() {
                Some(p) => {
                    p.tick = p.tick.wrapping_add(1);
                    cx.notify();
                    true
                }
                None => false,
            });
            match keep_going {
                Ok(true) => {}
                _ => break,
            }
        })
        .detach();
    }

    /// Fold the live processing state onto the just-settled assistant bubble
    /// so its activity log + token meter survive as a collapsible disclosure,
    /// then clear `processing` (which also stops the ticker). No-op when
    /// there's no in-flight processing. Drops an empty log (nothing ran) so
    /// the bubble shows no disclosure rather than an empty one.
    fn settle_processing(&mut self, assistant_id: &str) {
        let Some(state) = self.processing.take() else {
            return;
        };
        let activity = state.into_message_activity();
        if activity.is_empty() {
            return;
        }
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == assistant_id) {
            msg.activity = Some(activity);
        }
    }

    /// Toggle the live indicator's activity dropdown.
    pub fn toggle_processing_expanded(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.processing.as_mut() {
            p.expanded = !p.expanded;
            cx.notify();
        }
    }

    /// Toggle a settled bubble's persisted activity disclosure.
    pub fn toggle_message_activity(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == id) {
            msg.activity_expanded = !msg.activity_expanded;
            cx.notify();
        }
    }

    /// Reconcile the virtualized [`ListState`] with `self.messages` before each
    /// paint. `list` tracks an opaque item count; it can't see how the backing
    /// vec changed, so we tell it — and crucially we choose the *kind* of update
    /// so scroll position survives:
    ///
    /// * **empty** → reset to zero items (the empty-state element renders
    ///   instead, but keep the list count honest so the next message rebuilds);
    /// * **pure append within the current thread** (same `messages[0]`, count
    ///   only grew) → [`ListState::splice`] the new tail items, leaving existing
    ///   measurements and the scroll anchor untouched. This is the in-turn path:
    ///   the user+assistant bubbles get spliced once, then the assistant bubble
    ///   grows *in place* (count unchanged) and only needs remeasuring;
    /// * **wholesale swap** (head id changed — switch / load / new / clear — or
    ///   the log shrank) → [`ListState::reset`] and re-engage [`FollowMode::Tail`]
    ///   so the freshly loaded thread opens pinned to its newest message, the
    ///   chat convention;
    /// * **streaming tail** → whenever the last bubble's size inputs change
    ///   (content/thinking length or the streaming flag) [`ListState::remeasure_items`]
    ///   the single tail item. That re-measures the growing bubble while
    ///   preserving the scroll anchor (and tail-follow keeps it on screen).
    ///
    /// Identity for short logs: with nothing scrolled and everything fitting the
    /// viewport, Top alignment paints from the top exactly like the old column.
    ///
    /// Called from `render`; `pub` only so the windowed tests can drive the
    /// reconciler deterministically without depending on paint scheduling.
    pub fn sync_message_list(&mut self) {
        let n = self.messages.len();
        let head_id = self.messages.first().map(|m| m.id.clone());
        let tail_sig = self.messages.last().map_or(0, |m| {
            m.content.len() + m.thinking.as_ref().map_or(0, String::len) + m.streaming as usize
        });
        let known = self.message_list.item_count();

        if n == 0 {
            if known != 0 {
                self.message_list.reset(0);
            }
        } else if head_id == self.list_head_id && n >= known {
            if n > known {
                self.message_list.splice(known..known, n - known);
            }
            if n > known || tail_sig != self.list_tail_sig {
                self.message_list.remeasure_items(n - 1..n);
            }
        } else {
            self.message_list.reset(n);
            self.message_list.set_follow_mode(FollowMode::Tail);
        }

        self.list_head_id = head_id;
        self.list_tail_sig = tail_sig;
    }

    pub fn send_user_message(&mut self, text: String, cx: &mut Context<Self>) {
        self.error = None;
        self.messages.push(ChatMessage::user(text.clone()));
        let assistant = ChatMessage::assistant_streaming();
        let assistant_id = assistant.id.clone();
        self.messages.push(assistant);
        // A new turn always sticks to the bottom while it streams, regardless of
        // where the user had scrolled — re-engage tail-follow. `sync_message_list`
        // then splices the two new bubbles in on the next paint.
        self.message_list.set_follow_mode(FollowMode::Tail);
        // Latch synchronously — see the `starting` field doc.  Cleared
        // the moment `active_turn_id` is published (or on any failure).
        self.starting = true;
        // Arm the live processing indicator (chat-processing-indicator). It
        // shows "Working…" through the `start_turn` round-trip, then tracks
        // real phase / tool / token / thinking signals as they stream. Folded
        // onto the assistant bubble and cleared when the turn settles.
        self.processing = Some(ProcessingState::new());
        self.spawn_processing_ticker(cx);
        cx.notify();

        let conversation_id = self.conversation_id.clone();
        // D1: the turn carries a workspace_id only on a *bound* (Docked) surface.
        // Global is structurally unbound, so this resolves to `None` regardless
        // of the field — the single read that guarantees a global turn never
        // rides a workspace context.
        let workspace_id = self
            .scope
            .resolve_workspace_id(self.active_workspace_id.clone());
        // C6: a first send out of the empty state binds the fresh thread to the
        // entered workspace so it joins the scoped list. Only ever `Some` on a
        // Docked dock that minted a fileless thread on enter (see
        // `pending_bind_workspace`); cleared once we've bound below.
        let pending_bind = self.pending_bind_workspace.clone();
        // 2.5 (active-file boost): on a bound (workspace) turn, ride the file
        // open in the Workspaces editor so the harness biases RAG toward it.
        // Only when bound — D1 keeps the Global slot workspace-free, and the
        // boost is meaningless without a workspace index.
        let active_file = if workspace_id.is_some() {
            wylde_gui_pipe::current_active_file()
        } else {
            None
        };
        let model = self.active_model.clone();
        // The composer's per-message ✕/↺ choices ride the send (Slices F+M).
        let (excluded_tokens, reactivated_tokens) = self.composer.send_overrides();
        // Agentic-reasoning S1: the fast/deep pill rides the wire. `fast`
        // (the default) is behaviourally inert harness-side.
        let depth = self.reasoning_depth;

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let start = start_turn_with_model(
                &text,
                &conversation_id,
                workspace_id.as_deref(),
                model.as_deref(),
                &excluded_tokens,
                &reactivated_tokens,
                active_file.as_deref(),
                depth.as_str(),
            )
            .await;
            let (turn_id, reply_conversation_id) = match start {
                Ok(r) => (r.turn_id, r.conversation_id),
                Err(e) => {
                    let _ = this.update(app_cx, |panel, cx| {
                        let msg = format!("[Failed to start turn: {e}]");
                        if let Some(last) = panel.messages.iter_mut().find(|m| m.id == assistant_id)
                        {
                            last.content = msg.clone();
                            last.streaming = false;
                        }
                        panel.active_turn_id = None;
                        panel.starting = false;
                        panel.processing = None;
                        panel.error = Some(msg);
                        cx.notify();
                    });
                    return;
                }
            };

            // Open the user-facing stream before publishing the turn so a
            // stream-open failure leaves the panel idle, not half-armed.
            let user_stream = match stream_turn(&turn_id) {
                Ok(s) => s,
                Err(e) => {
                    let _ = this.update(app_cx, |panel, cx| {
                        if let Some(m) = panel.messages.iter_mut().find(|m| m.id == assistant_id) {
                            m.content = format!("[stream error: {e}]");
                            m.streaming = false;
                        }
                        panel.active_turn_id = None;
                        panel.starting = false;
                        panel.active_tool_stream = None;
                        panel.processing = None;
                        cx.notify();
                    });
                    return;
                }
            };

            // Publish the turn id + both stream handles.  `active_stream`
            // is the cancel handle the Stop button drops; the tool stream
            // feeds the activity strip.  Both keyed off the same turn_id.
            let published = this
                .update(app_cx, |panel, cx| {
                    // Adopt the harness-minted conversation id so the
                    // working-memory strip queries the right buffer (and
                    // future turns thread the same conversation).  Empty
                    // reply → keep the existing id.  On an actual change,
                    // re-announce on the cross-panel bus so the Memory
                    // panel's short-term view follows us onto the new
                    // conversation.
                    if !reply_conversation_id.is_empty()
                        && panel.conversation_id != reply_conversation_id
                    {
                        panel.conversation_id = reply_conversation_id.clone();
                        wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
                    }
                    panel.active_turn_id = Some(turn_id.clone());
                    panel.starting = false;
                    panel.active_stream = Some(user_stream);
                    if let Ok(stream) = stream_tools(&turn_id) {
                        panel.active_tool_stream = Some(stream);
                    }
                    cx.notify();
                })
                .is_ok();
            if !published {
                // Entity torn down between start and publish — `user_stream`
                // drops here, cancelling the just-opened subscription.
                return;
            }

            // C6: bind the freshly-minted empty-state thread now that the turn
            // has started (the harness echoes our conversation id, so it has a
            // doc on disk to bind). This is what makes "enter a fresh workspace,
            // send → thread appears in its list" hold — without it the thread
            // would persist unbound (the turn path never writes `workspace_id`).
            // Idempotent + non-fatal: a bind failure leaves the thread usable,
            // just unbound this session.
            if let Some(ws) = pending_bind {
                let bound_target = if reply_conversation_id.is_empty() {
                    conversation_id.clone()
                } else {
                    reply_conversation_id.clone()
                };
                if let Err(e) = set_workspace_for_conversation(&bound_target, &ws).await {
                    let _ = this.update(app_cx, |panel, cx| {
                        panel.error =
                            Some(format!("Couldn't bind this thread to the workspace: {e}"));
                        cx.notify();
                    });
                }
                let _ = this.update(app_cx, |panel, cx| {
                    // Clear only if it's still this same pending bind — a newer
                    // enter/select may have re-parked or dropped it meanwhile.
                    if panel.pending_bind_workspace.as_deref() == Some(ws.as_str()) {
                        panel.pending_bind_workspace = None;
                    }
                    cx.notify();
                });
                // C7: the just-bound thread is now this workspace's last-open.
                let _ = set_active_conversation_for_workspace(&ws, &bound_target).await;
                wylde_gui_pipe::publish_conversation_list_changed();
                Self::reload_conversations(&this, app_cx).await;
            }

            // Drive the tool-activity strip in the background.
            Self::pump_tool_stream(this.clone(), app_cx);

            // Drain the user-facing chunks.  Borrow the stream slot one
            // frame at a time (the same idiom `pump_tool_stream` uses) so
            // the Stop button — or panel teardown — can drop the handle
            // from under us between frames.  A turn-id guard keeps a late
            // chunk from a cancelled / superseded turn out of the bubble.
            loop {
                let frame = match this.update(app_cx, |panel, _| panel.active_stream.take()) {
                    Ok(Some(mut s)) => {
                        let f = s.recv().await;
                        let tid = turn_id.clone();
                        // Re-home the stream only while this turn is still
                        // the active one.  If Stop / completion nulled the
                        // turn while we awaited, `s` drops here — the
                        // client-side half of cancellation.
                        let _ = this.update(app_cx, move |panel, _| {
                            if panel.active_turn_id.as_deref() == Some(tid.as_str())
                                && panel.active_stream.is_none()
                            {
                                panel.active_stream = Some(s);
                            }
                        });
                        f
                    }
                    // Slot empty (Stop took it) or entity gone — stop.
                    _ => break,
                };
                let Some(chunk) = frame else {
                    // Stream closed (server done, transport ended, drop).
                    break;
                };
                match chunk {
                    Ok(v) => {
                        let event = TurnChunk::from_value(&v);
                        let mut done = false;
                        let alive = this
                            .update(app_cx, |panel, cx| {
                                // Drop chunks from a turn the user already
                                // cancelled / that already completed.
                                if panel.active_turn_id.as_deref() != Some(turn_id.as_str()) {
                                    done = true;
                                    return;
                                }
                                // Feed the live indicator (phase / token meter
                                // / thinking) before the bubble apply.
                                apply_processing_turn_event(panel.processing.as_mut(), &event);
                                apply_turn_chunk(&mut panel.messages, &assistant_id, &event);
                                if matches!(
                                    event,
                                    TurnChunk::TurnComplete { .. } | TurnChunk::TurnAborted { .. }
                                ) {
                                    done = true;
                                    // Fold the activity onto the bubble, then
                                    // clear (also stops the ticker).
                                    panel.settle_processing(&assistant_id);
                                    panel.active_turn_id = None;
                                    panel.active_stream = None;
                                    panel.active_tool_stream = None;
                                }
                                cx.notify();
                            })
                            .is_ok();
                        if done || !alive {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = this.update(app_cx, |panel, cx| {
                            flush_streaming_bubble(
                                &mut panel.messages,
                                &assistant_id,
                                &format!("[stream error: {e}]"),
                            );
                            panel.settle_processing(&assistant_id);
                            panel.active_turn_id = None;
                            panel.active_stream = None;
                            panel.active_tool_stream = None;
                            cx.notify();
                        });
                        break;
                    }
                }
            }

            // Recovery: if the loop broke while this turn was still the
            // active one (stream closed without a terminal event, e.g. a
            // server crash), flush the bubble + clear state so the panel
            // doesn't get wedged showing a perpetual typing indicator and
            // a Stop button.  No-op on the normal/cancel paths (the turn
            // is already None there) and only ever touches *our* turn.
            let _ = this.update(app_cx, |panel, cx| {
                if panel.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                    flush_streaming_bubble(&mut panel.messages, &assistant_id, "[stream ended]");
                    panel.settle_processing(&assistant_id);
                    panel.active_turn_id = None;
                    panel.active_stream = None;
                    panel.active_tool_stream = None;
                    cx.notify();
                }
            });

            // The turn may have appended working-memory entries (tool
            // calls, decisions, summaries) server-side; refresh the strip
            // so it reflects the conversation's post-turn buffer.
            Self::reload_working_memory(&this, app_cx).await;
            // The first turn of a fresh conversation persists its file (and
            // derives a title); refresh the rail so it appears / re-sorts.
            Self::reload_conversations(&this, app_cx).await;
        })
        .detach();
    }

    /// Pump events off the active tool stream into the live processing
    /// indicator (`processing`). Runs until the stream ends (turn complete,
    /// drop, or transport
    /// error) — the parent task that spawned the user-facing stream
    /// owns the actual stream slot; this task only borrows it briefly
    /// per `recv`.  When the slot is taken from under us (turn ends),
    /// `recv` returns None and we exit.
    fn pump_tool_stream(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let app_cx = app_cx.clone();
        app_cx
            .spawn(async move |app_cx| {
                loop {
                    // Acquire the stream slot, take one frame, put it
                    // back if we got something — keeps the slot field
                    // re-entrant with the cancel path that nulls it.
                    let next = match this.update(app_cx, |panel, _| panel.active_tool_stream.take())
                    {
                        Ok(Some(mut s)) => {
                            let frame = s.recv().await;
                            // Stash the stream back so the cancel path
                            // can drop it.
                            let _ = this.update(app_cx, |panel, _| {
                                if panel.active_tool_stream.is_none() {
                                    panel.active_tool_stream = Some(s);
                                }
                            });
                            frame
                        }
                        _ => return,
                    };
                    let Some(chunk) = next else {
                        // Stream ended naturally.
                        let _ = this.update(app_cx, |panel, _| {
                            panel.active_tool_stream = None;
                        });
                        return;
                    };
                    let value = match chunk {
                        Ok(v) => v,
                        Err(_) => {
                            // Transport hiccup; let it ride.  The
                            // user-facing stream will surface a louder
                            // error if the whole turn died.
                            return;
                        }
                    };
                    let event = ToolChunk::from_value(&value);
                    let _ = this.update(app_cx, |panel, cx| {
                        match event {
                            ToolChunk::Dispatched {
                                call_id,
                                name,
                                args,
                                ..
                            } => {
                                if let Some(p) = panel.processing.as_mut() {
                                    p.on_tool_dispatched(&call_id, &name, tool_detail(&args, None));
                                }
                            }
                            ToolChunk::Result {
                                call_id,
                                output,
                                duration_ms,
                                ..
                            } => {
                                if let Some(p) = panel.processing.as_mut() {
                                    p.on_tool_done(
                                        &call_id,
                                        true,
                                        tool_detail(&output, Some(duration_ms)),
                                    );
                                }
                            }
                            ToolChunk::Error {
                                call_id,
                                message,
                                duration_ms,
                                ..
                            } => {
                                if let Some(p) = panel.processing.as_mut() {
                                    let detail = tool_detail(
                                        &serde_json::Value::String(message),
                                        Some(duration_ms),
                                    );
                                    p.on_tool_done(&call_id, false, detail);
                                }
                            }
                            ToolChunk::MemoryWritten => {
                                if let Some(p) = panel.processing.as_mut() {
                                    p.on_memory_written();
                                }
                            }
                            ToolChunk::Warning | ToolChunk::Unknown => {}
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
    }

    /// "Stop generating" — drops both open `PipeStream`s + fires the
    /// server-side cancel.  The server-side cancel is best-effort: if
    /// the harness has already moved past the cancellable phase the
    /// call returns a no-op, which we silently absorb.
    pub fn cancel_active_turn(&mut self, cx: &mut Context<Self>) {
        let Some(turn_id) = self.active_turn_id.clone() else {
            return;
        };
        // Client-side half of cancel: drop both open PipeStreams.
        // Dropping the user-facing stream aborts its IO task and closes
        // the pipe, which the harness reads as a cancel; the `chat.cancel`
        // unary below makes that synchronous so the server frees its
        // resources immediately instead of waiting on close-detect.
        self.active_stream.take();
        self.active_tool_stream.take();
        // Abandon any consent cards belonging to this turn.  Only one
        // turn is ever in flight (sends are blocked while active), so
        // every pending card belongs to the turn being cancelled.
        self.pending_consents.clear();
        // Flush the in-flight assistant bubble's streaming flag, preserving
        // whatever activity ran before the stop as a disclosure on it.
        let cancelled_id = self
            .messages
            .iter()
            .rev()
            .find(|m| m.streaming)
            .map(|m| m.id.clone());
        if let Some(id) = &cancelled_id {
            self.settle_processing(id);
        }
        self.processing = None;
        if let Some(msg) = self.messages.iter_mut().rev().find(|m| m.streaming) {
            msg.streaming = false;
            if msg.content.is_empty() {
                msg.content = "[cancelled]".to_owned();
            }
        }
        self.active_turn_id = None;
        cx.notify();
        cx.spawn(async move |_this, _app_cx: &mut AsyncApp| {
            let _ = cancel_turn(&turn_id).await;
        })
        .detach();
    }

    pub fn toggle_ws_dropdown(&mut self, cx: &mut Context<Self>) {
        self.show_ws_dropdown = !self.show_ws_dropdown;
        self.show_model_dropdown = false;
        self.show_conversations = false;
        cx.notify();
    }

    pub fn select_workspace(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // D1: the Global Chat slot is structurally unbound — it never carries a
        // workspace, so a selection there resolves to `None` (the affordance is
        // also hidden on Global, this is the defense-in-depth setter guard).
        self.active_workspace_id = self.scope.resolve_workspace_id(Some(id.to_owned()));
        self.show_ws_dropdown = false;
        // Persist the active pointer + MRU bump on the harness, then
        // refresh the dropdown so it reflects the new MRU order.
        let id_owned = id.to_owned();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = set_active_workspace(&id_owned).await;
            let mru = recent_workspaces(WORKSPACE_MRU_LIMIT)
                .await
                .unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                if !mru.is_empty() {
                    panel.workspaces = mru;
                }
                cx.notify();
            });
        })
        .detach();
        // Picker closed → hand focus straight back to the prompt so the
        // user can keep typing without re-clicking the input.
        self.focus_prompt(window, cx);
        cx.notify();
    }

    pub fn toggle_model_dropdown(&mut self, cx: &mut Context<Self>) {
        self.show_model_dropdown = !self.show_model_dropdown;
        self.show_ws_dropdown = false;
        self.show_conversations = false;
        cx.notify();
    }

    /// Cycle the per-turn thinking tier (fast → think → think harder →
    /// ultrathink → fast). An instant local flip; the chosen tier rides
    /// the next send's `depth` field.
    pub fn toggle_reasoning_depth(&mut self, cx: &mut Context<Self>) {
        self.reasoning_depth = self.reasoning_depth.toggled();
        cx.notify();
    }

    pub fn select_model(
        &mut self,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_model = model.clone();
        self.show_model_dropdown = false;
        // Picker closed → restore focus to the prompt input.
        self.focus_prompt(window, cx);
        cx.notify();

        // Persist the pick so other processes/panels can observe it: the
        // Settings → Ollama section resolves the *effective* model via
        // `models.get_effective`, which reads this `active_model.json`.
        // Fire-and-forget — a transport failure only means Settings won't
        // track this pick until the next refresh, never blocks the click.
        let persist = model.clone();
        cx.spawn(async move |_this, _app_cx: &mut AsyncApp| {
            let _ = set_active_model(persist.as_deref()).await;
        })
        .detach();

        // Publish on the model bus so an open Settings panel re-queries
        // this model's parameter defaults live (its "State 4").
        let _ = wylde_gui_pipe::publish_active_model(model);
    }

    /// Eject the active model from VRAM via `ollama.eject`.  No-op when
    /// no concrete model is selected ("(auto)") or an eject is already in
    /// flight — the button is dimmed in both states, this guards the
    /// programmatic path.  Frontend-only: reuses the existing harness verb.
    pub fn eject_active_model(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.active_model.clone() else {
            return;
        };
        if self.ejecting {
            return;
        }
        self.ejecting = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = eject_model(&model).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.ejecting = false;
                if let Err(e) = outcome {
                    panel.error = Some(format!("eject {model}: {e}"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Move keyboard focus to the prompt input.  Used after a dropdown
    /// picker closes so typing resumes immediately — without it the
    /// focus stayed on the (now-hidden) dropdown row and the next
    /// keystroke went nowhere until the user clicked back in.
    fn focus_prompt(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.prompt_input.read(cx).focus_handle.clone();
        handle.focus(window, cx);
    }

    /// Export one conversation to a user-picked file (TBS Slice J): fetch
    /// the portable envelope from the harness, then a native save dialog,
    /// then a plaintext-JSON write. A cancelled dialog is silent.
    pub fn spawn_export_conversation(id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome: Result<String, String> = async {
                let envelope = export_conversation(&id).await?;
                let default_name = format!("{id}.wylde-conv.json");
                let picked: Option<PathBuf> = wylde_gui_pipe::bridged_spawn_blocking(move || {
                    rfd::FileDialog::new()
                        .set_title("Export conversation")
                        .set_file_name(&default_name)
                        .add_filter("Wylde conversation export", &["json"])
                        .save_file()
                })
                .await;
                let Some(path) = picked else {
                    return Ok(String::new()); // cancelled — no status
                };
                let body = serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())?;
                std::fs::write(&path, body).map_err(|e| e.to_string())?;
                Ok(format!("Exported to {}", path.display()))
            }
            .await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(msg) if msg.is_empty() => {}
                    Ok(msg) => panel.transfer_status = Some(Ok(msg)),
                    Err(e) => panel.transfer_status = Some(Err(format!("Export failed: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Import a conversation export file as a standalone conversation (TBS
    /// Slice J): native open dialog → parse → `chat.import`. An id collision
    /// surfaces the harness's `already_exists` message (delete or rename the
    /// existing conversation first — nothing is silently replaced).
    pub fn spawn_import_conversation(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome: Result<Option<String>, String> = async {
                let picked: Option<PathBuf> = wylde_gui_pipe::bridged_spawn_blocking(|| {
                    rfd::FileDialog::new()
                        .set_title("Import conversation")
                        .add_filter("Wylde conversation export", &["json"])
                        .pick_file()
                })
                .await;
                let Some(path) = picked else { return Ok(None) };
                let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                let envelope: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|e| format!("not valid JSON: {e}"))?;
                import_conversation(envelope).await.map(Some)
            }
            .await;
            match outcome {
                Ok(None) => {}
                Ok(Some(id)) => {
                    let _ = this.update(app_cx, |panel, cx| {
                        panel.transfer_status = Some(Ok(format!("Imported {id}")));
                        cx.notify();
                    });
                    Self::reload_conversations(&this, app_cx).await;
                }
                Err(e) => {
                    let _ = this.update(app_cx, |panel, cx| {
                        panel.transfer_status = Some(Err(format!("Import failed: {e}")));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    pub fn spawn_pick_workspace(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Runs on gpui's executor (no tokio reactor) — `tokio::task::
            // spawn_blocking` would panic. Hop onto the bridge runtime.
            let picked: Option<PathBuf> = wylde_gui_pipe::bridged_spawn_blocking(pick_folder).await;
            let Some(path) = picked else {
                return;
            };
            let path_str = path.to_string_lossy().to_string();
            let outcome = activate_workspace(&path_str).await;
            let _ = this.update(app_cx, |panel, _cx| {
                if let Err(e) = &outcome {
                    panel.error = Some(format!("activate workspace: {e}"));
                }
                if let Ok(ws) = &outcome {
                    // D1: Global stays unbound even via the folder picker — the
                    // scope resolves the id to `None` there (picker is hidden on
                    // Global; this guards the setter against any future caller).
                    panel.active_workspace_id =
                        panel.scope.resolve_workspace_id(Some(ws.id.clone()));
                }
            });
            let mru = recent_workspaces(WORKSPACE_MRU_LIMIT)
                .await
                .unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                if !mru.is_empty() {
                    panel.workspaces = mru;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn spawn_respond_consent(
        prompt_id: String,
        tool_id: String,
        decision: &'static str,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = this.update(app_cx, |panel, cx| {
                panel.pending_consents.remove(&prompt_id);
                cx.notify();
            });
            let outcome = respond_consent(&tool_id, decision, remember).await;
            if let Err(e) = outcome {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("consent {decision}: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Focusable for ChatPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Flush a streaming assistant bubble: clear its `streaming` flag and,
/// if it never received any content, substitute `fallback`.  Used by the
/// stream-error and unexpected-close paths so a turn that dies mid-flight
/// never leaves a perpetual typing indicator (and a stuck Stop button).
fn flush_streaming_bubble(messages: &mut [ChatMessage], assistant_id: &str, fallback: &str) {
    if let Some(m) = messages.iter_mut().find(|m| m.id == assistant_id) {
        if m.streaming {
            if m.content.is_empty() {
                m.content = fallback.to_owned();
            }
            m.streaming = false;
        }
    }
}

/// Format a tool's args / output (+ optional duration) into a compact,
/// truncated activity-log detail line (chat-processing-indicator, full
/// visibility). A bare string value is used verbatim (error messages), other
/// JSON is serialised; `Null`/empty collapses to just the duration, or `None`.
fn tool_detail(value: &serde_json::Value, duration_ms: Option<f64>) -> Option<String> {
    let raw = match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    };
    let body = raw
        .as_deref()
        .and_then(|s| processing::compact_detail(s, 160));
    match (body, duration_ms) {
        (Some(b), Some(ms)) if ms > 0.0 => Some(format!("{b}  ·  {ms:.0}ms")),
        (Some(b), _) => Some(b),
        (None, Some(ms)) if ms > 0.0 => Some(format!("{ms:.0}ms")),
        (None, _) => None,
    }
}

/// Feed a user-facing turn chunk into the live processing indicator
/// (chat-processing-indicator). A `None` state — no in-flight indicator — is
/// a silent noop, so this is safe to call unconditionally. Only the
/// indicator-relevant variants do anything; bubble text, completion, and
/// abort are handled by [`apply_turn_chunk`] and the pump's settle.
fn apply_processing_turn_event(state: Option<&mut ProcessingState>, event: &TurnChunk) {
    let Some(p) = state else {
        return;
    };
    match event {
        TurnChunk::Phase { phase, .. } => {
            p.set_phase(ProcessingPhase::from_wire(phase));
        }
        TurnChunk::Usage {
            prompt_tokens,
            completion_tokens,
            ..
        } => {
            p.on_usage(*prompt_tokens, *completion_tokens);
        }
        TurnChunk::Thinking { text, .. } => {
            p.on_thinking(text);
        }
        TurnChunk::Step {
            stage,
            summary,
            detail,
            ..
        } => {
            p.on_step(stage, summary.clone(), detail.clone());
        }
        TurnChunk::Token { .. }
        | TurnChunk::TurnComplete { .. }
        | TurnChunk::TurnAborted { .. }
        | TurnChunk::Unknown => {}
    }
}

/// Apply a single `chat.stream_turn` chunk to the assistant bubble.
fn apply_turn_chunk(messages: &mut [ChatMessage], assistant_id: &str, event: &TurnChunk) {
    let Some(msg) = messages.iter_mut().find(|m| m.id == assistant_id) else {
        return;
    };
    match event {
        TurnChunk::Token { text, .. } => {
            msg.content.push_str(text);
        }
        TurnChunk::Thinking { text, .. } => {
            let buf = msg.thinking.get_or_insert_with(String::new);
            buf.push_str(text);
        }
        TurnChunk::TurnComplete { final_message, .. } => {
            if msg.content.is_empty() && !final_message.is_empty() {
                msg.content = final_message.clone();
            }
            msg.streaming = false;
        }
        TurnChunk::TurnAborted { reason, error, .. } => {
            if msg.content.is_empty() && reason != "cancelled" {
                let suffix = error
                    .as_deref()
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default();
                msg.content = format!("[Turn ended: {reason}{suffix}]");
            }
            msg.streaming = false;
        }
        // Phase / Usage / Step feed only the processing indicator
        // (`apply_processing_turn_event`); the bubble itself is unchanged.
        TurnChunk::Phase { .. } | TurnChunk::Usage { .. } | TurnChunk::Step { .. } => {}
        TurnChunk::Unknown => { /* future variant — noop */ }
    }
}

/// Choose the conversation to make active after deleting the current one.
/// `remaining` is the post-delete list (newest-first, with the deleted
/// entry already removed); returns the newest remaining id — the closest
/// thing to "the previous chat" — or `None` when nothing is left. Pure so
/// the fallback rule is unit-testable without driving the gpui executor.
fn pick_next_active(remaining: &[ConversationMeta], _deleted_id: &str) -> Option<String> {
    remaining.first().map(|c| c.id.clone())
}

/// What the dock should open when a workspace is entered (C6).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnterTarget {
    /// Switch the dock's view to this existing thread.
    Open(String),
    /// No thread to restore — show the empty state ("How can I help?") on a
    /// fresh fileless thread whose bind is deferred to the first send.
    Empty,
}

/// Decide which thread the dock opens on entering a workspace, purely from the
/// per-workspace last-open pointer (C7) and the workspace's scoped, newest-first
/// conversation list (C4). Split out so the three enter cases are unit-testable
/// without a live panel:
/// - the pointer names a thread still in the list → open it (**has-last-open**);
/// - no usable pointer but the list is non-empty → open the most-recent, i.e.
///   the head of the newest-first list (**has-threads**);
/// - neither → [`EnterTarget::Empty`] (**none**).
///
/// A pointer is only honoured when its thread is still present: a deleted thread
/// (or one whose binding moved) falls through to most-recent / empty rather than
/// stranding the dock on a missing id.
fn pick_enter_conversation(last_open: Option<&str>, scoped: &[ConversationMeta]) -> EnterTarget {
    if let Some(id) = last_open {
        if scoped.iter().any(|m| m.id == id) {
            return EnterTarget::Open(id.to_owned());
        }
    }
    match scoped.first() {
        Some(meta) => EnterTarget::Open(meta.id.clone()),
        None => EnterTarget::Empty,
    }
}

// ── Symbol-aware composer plumbing (Slice F) ─────────────────────────────
impl ChatPanel {
    /// Mirror the recognition state into the prompt input as styled spans —
    /// the in-input wavy underline (the glyph-metrics pass closed Slice F's
    /// deferral). Cheap to call after any `composer.words` mutation: the
    /// input no-ops when the spans are unchanged.
    pub(crate) fn sync_prompt_highlights(&self, cx: &mut Context<Self>) {
        let spans = composer::highlight::input_spans(&self.composer.words);
        self.prompt_input
            .update(cx, |input, icx| input.set_highlights(spans, icx));
    }

    /// Debounced recognition scan: tokenize now (cheap, sync), wait out the
    /// debounce window, then resolve each token over the pipe. A newer edit
    /// bumps the generation and this scan's results drop on arrival.
    pub(crate) fn schedule_composer_scan(&mut self, text: String, cx: &mut Context<Self>) {
        let generation = self.composer.begin_scan();
        let tokens = composer::tokenizer::scan(&text);
        let conversation_id = self.conversation_id.clone();
        let Some(ws) = self.active_workspace_id.clone() else {
            // No workspace → nothing to recognize against.
            let _ = self.composer.install(generation, Vec::new(), false);
            self.sync_prompt_highlights(cx);
            cx.notify();
            return;
        };
        if tokens.is_empty() {
            let _ = self.composer.install(generation, Vec::new(), false);
            self.sync_prompt_highlights(cx);
            cx.notify();
            return;
        }
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            app_cx
                .background_executor()
                .timer(Duration::from_millis(composer::input::SCAN_DEBOUNCE_MS))
                .await;
            let still_current = this
                .update(app_cx, |p, _| p.composer.generation == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let mut words: Vec<WordRecognition> = Vec::with_capacity(tokens.len());
            let mut degraded = false;
            for t in tokens {
                let fallback = t.clone();
                match composer::ipc_to_workspaces::recognize(&ws, t).await {
                    Ok(w) => words.push(w),
                    Err(_) => {
                        // OI-1 graceful degrade: keep the token visible-less
                        // and flag the strip hint; typing is never blocked.
                        degraded = true;
                        words.push(WordRecognition::new(fallback));
                    }
                }
            }
            // Mark words covered by a durable ignore tier (Slice M) so they
            // render default-deselected with the ↺ affordance.
            let tiers = composer::ipc_to_workspaces::ignore_tiers(&ws, &conversation_id).await;
            for w in &mut words {
                if let Some(tags) = tiers.get(&w.token.text) {
                    w.ignored_tiers = tags.clone();
                }
            }
            let _ = this.update(app_cx, |p, cx| {
                if p.composer.install(generation, words, degraded) {
                    p.bubbles.on_words_changed(p.composer.words.len());
                    p.sync_prompt_highlights(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Toggle the Ctrl+P palette. Opening focuses its query field.
    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.palette.is_some() {
            self.composer.palette = None;
            self.focus_prompt(window, cx);
        } else {
            self.composer.palette = Some(Default::default());
            self.palette_input.update(cx, |i, c| i.clear(c));
            let handle = self.palette_input.read(cx).focus_handle.clone();
            handle.focus(window, cx);
        }
        cx.notify();
    }

    /// Debounced palette query → `workspaces.symbols.find`.
    pub(crate) fn schedule_palette_query(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(palette) = self.composer.palette.as_mut() else {
            return;
        };
        palette.generation += 1;
        palette.query = query.clone();
        let generation = palette.generation;
        let Some(ws) = self.active_workspace_id.clone() else {
            return;
        };
        if query.trim().is_empty() {
            palette.hits.clear();
            palette.selected = 0;
            cx.notify();
            return;
        }
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            app_cx
                .background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let still_current = this
                .update(app_cx, |p, _| {
                    p.composer
                        .palette
                        .as_ref()
                        .is_some_and(|pl| pl.generation == generation)
                })
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let hits = composer::ipc_to_workspaces::find_symbols(&ws, query.trim(), 8)
                .await
                .unwrap_or_default();
            let _ = this.update(app_cx, |p, cx| {
                if let Some(pl) = p.composer.palette.as_mut() {
                    if pl.generation == generation {
                        pl.hits = hits;
                        pl.selected = 0;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Insert an `@symbol` reference at the prompt cursor and close the
    /// palette (no refocus — used by the palette input's Enter).
    pub(crate) fn insert_palette_reference(&mut self, name: &str, cx: &mut Context<Self>) {
        self.prompt_input.update(cx, |input, c| {
            composer::input::insert_reference(input, name);
            input.emit_changed(c);
        });
        self.composer.palette = None;
        cx.notify();
    }

    /// Click-accept from a palette row: insert + hand focus back to the
    /// prompt.
    pub(crate) fn accept_palette_symbol(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_palette_reference(name, cx);
        self.focus_prompt(window, cx);
    }

    /// Panel-level keys (the input forwards what it doesn't handle):
    /// `Ctrl+P` toggles the symbol palette, `Esc` closes composer popovers,
    /// and the unified §5.9 undo chords (the prompt opts out of handling
    /// them itself — see `with_external_undo`).
    pub(crate) fn on_panel_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &ev.keystroke;
        let cmd_or_ctrl = ks.modifiers.control || ks.modifiers.platform;
        if ks.key.as_str() == "p" && cmd_or_ctrl {
            self.toggle_palette(window, cx);
            return;
        }
        // Unified undo timeline (§5.9): Ctrl+Z undoes the newest thing —
        // text edit or bubble op, whichever happened last.
        if ks.key.as_str() == "z" && cmd_or_ctrl && ks.modifiers.shift {
            self.unified_redo(cx);
            return;
        }
        if ks.key.as_str() == "z" && cmd_or_ctrl {
            self.unified_undo(cx);
            return;
        }
        if ks.key.as_str() == "y" && cmd_or_ctrl {
            self.unified_redo(cx);
            return;
        }
        if ks.key.as_str() == "escape" {
            let had_overlay = self.composer.palette.is_some()
                || self.composer.disambiguating.is_some()
                || self.composer.anchor_offer.is_some()
                || self.composer.ignore_menu.is_some()
                || self.composer.curating
                || self.bubbles.is_open();
            if had_overlay {
                self.composer.palette = None;
                self.composer.disambiguating = None;
                self.composer.anchor_offer = None;
                self.composer.ignore_menu = None;
                self.composer.curating = false;
                // §5.4: Esc collapses the bubble set back to compact.
                self.bubbles.collapse();
                self.focus_prompt(window, cx);
                cx.notify();
            }
        }
    }

    // ── Unified undo (Plan §5.9): one timeline, two stacks ───────────

    /// Undo the newest thing — a text edit group or a bubble op,
    /// whichever's timeline stamp is higher.
    pub(crate) fn unified_undo(&mut self, cx: &mut Context<Self>) {
        use composer::bubbles::UndoSide;
        let text_top = self.prompt_input.read(cx).top_undo_seq();
        match composer::bubbles::newer_side(text_top, self.bubble_undo.top_seq()) {
            UndoSide::Text => {
                self.replaying_undo = true;
                self.prompt_input.update(cx, |input, icx| {
                    input.undo(icx);
                });
                self.replaying_undo = false;
                cx.notify();
            }
            UndoSide::Bubble => {
                let Some(entry) = self.bubble_undo.undo().cloned() else {
                    return;
                };
                self.replaying_undo = true;
                self.apply_bubble_op(&entry.action, false, cx);
                self.replaying_undo = false;
                cx.notify();
            }
            UndoSide::Neither => {}
        }
    }

    /// Redo mirrors undo over the redo branches: re-apply whichever
    /// undone op is chronologically EARLIER first? No — newest-undone
    /// first means lowest stamp last; redo replays in original order, so
    /// the SMALLEST stamp on top of the redo branches goes first. Both
    /// branches are stacks whose tops are the most-recently-undone (=
    /// oldest-original) ops, so comparing tops by `newer_side` and taking
    /// the OPPOSITE side replays correctly.
    pub(crate) fn unified_redo(&mut self, cx: &mut Context<Self>) {
        use composer::bubbles::UndoSide;
        let text_top = self.prompt_input.read(cx).top_redo_seq();
        let bubble_top = self.bubble_undo.top_redo_seq();
        // Replay in original chronological order: the redo-branch top with
        // the OLDER stamp happened first, so it re-applies first.
        let side = match composer::bubbles::newer_side(text_top, bubble_top) {
            UndoSide::Neither => return,
            UndoSide::Text if bubble_top.is_some() => UndoSide::Bubble,
            UndoSide::Bubble if text_top.is_some() => UndoSide::Text,
            s => s,
        };
        match side {
            UndoSide::Text => {
                self.replaying_undo = true;
                self.prompt_input.update(cx, |input, icx| {
                    input.redo(icx);
                });
                self.replaying_undo = false;
                cx.notify();
            }
            UndoSide::Bubble => {
                let Some(entry) = self.bubble_undo.redo().cloned() else {
                    return;
                };
                self.replaying_undo = true;
                self.apply_bubble_op(&entry.action, true, cx);
                self.replaying_undo = false;
                cx.notify();
            }
            UndoSide::Neither => {}
        }
    }

    /// Apply a bubble op (`forward` = redo direction, else its inverse).
    /// Runs under `replaying_undo`, so nothing here re-records.
    fn apply_bubble_op(
        &mut self,
        op: &composer::bubbles::BubbleOp,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        use composer::bubbles::BubbleOp;
        match op {
            BubbleOp::ToggleExclude { word_idx } => {
                self.composer.toggle_excluded(*word_idx);
                self.sync_prompt_highlights(cx);
            }
            BubbleOp::TogglePin { label } => {
                self.bubbles.toggle_pin(label);
            }
            BubbleOp::SetOpenWord { from, to } => {
                let target = if forward { *to } else { *from };
                match target {
                    // `open` toggles-to-collapse on a same-word re-open, so
                    // guard: only drive it when the state actually differs.
                    Some(w) if self.bubbles.word_idx != Some(w) => {
                        self.open_word_bubbles(w, cx);
                    }
                    Some(_) => {}
                    None => self.bubbles.collapse(),
                }
            }
        }
    }

    /// Record one bubble op on the §5.9 timeline: stamp from the shared
    /// clock, seal the prompt's typing burst (so mid-burst ops interleave
    /// honestly), and invalidate the text redo branch (linear history).
    fn record_bubble_op(
        &mut self,
        label: impl Into<String>,
        op: composer::bubbles::BubbleOp,
        cx: &mut Context<Self>,
    ) {
        if self.replaying_undo {
            return;
        }
        self.bubble_undo
            .push_seq(label, op, wylde_gpui_input::next_undo_seq());
        self.prompt_input.update(cx, |input, _| {
            input.seal_undo_burst();
            input.clear_redo();
        });
    }

    /// The undoable ✕/↺ flip every exclude surface routes through (card
    /// button, shared menu, curation popover).
    pub(crate) fn toggle_word_excluded_undoable(
        &mut self,
        word_idx: usize,
        cx: &mut Context<Self>,
    ) {
        if self.composer.toggle_excluded(word_idx) {
            let label = if self.composer.words.get(word_idx).is_some_and(|w| {
                if w.is_ignored() {
                    !w.reactivated
                } else {
                    w.excluded
                }
            }) {
                "Excluded for this message"
            } else {
                "Restored to active"
            };
            self.record_bubble_op(
                label,
                composer::bubbles::BubbleOp::ToggleExclude { word_idx },
                cx,
            );
            self.sync_prompt_highlights(cx);
        }
    }

    /// The undoable 📌 flip (card button + shared menu).
    pub(crate) fn toggle_bubble_pin_undoable(&mut self, label: &str, cx: &mut Context<Self>) {
        let now_pinned = self.bubbles.toggle_pin(label);
        self.record_bubble_op(
            if now_pinned {
                "Pinned to this conversation"
            } else {
                "Unpinned from this conversation"
            },
            composer::bubbles::BubbleOp::TogglePin {
                label: label.to_owned(),
            },
            cx,
        );
    }

    // ── The Thought-Bubble layer (Plan §5.2–5.5) ─────────────────────

    /// Open (or swap to / collapse) a word's bubble set (§5.2, OI-17) and
    /// fetch its anchors. Spawned from a chip click or a click on the
    /// highlighted word itself.
    pub(crate) fn open_word_bubbles(&mut self, word_idx: usize, cx: &mut Context<Self>) {
        let from = self.bubbles.word_idx;
        if !self.bubbles.open(word_idx) {
            // Re-click on the open word → collapse (§5.2). Undoable.
            self.record_bubble_op(
                "Bubbles collapsed",
                composer::bubbles::BubbleOp::SetOpenWord { from, to: None },
                cx,
            );
            cx.notify();
            return;
        }
        let Some(word) = self.composer.words.get(word_idx).cloned() else {
            self.bubbles.collapse();
            cx.notify();
            return;
        };
        // Spawn/swap is §5.9-undoable (the spec's "Spawn (left-click)").
        self.record_bubble_op(
            format!("Bubbles for \u{201c}{}\u{201d}", word.token.text),
            composer::bubbles::BubbleOp::SetOpenWord {
                from,
                to: Some(word_idx),
            },
            cx,
        );
        cx.notify();
        let ws = self.active_workspace_id.clone().unwrap_or_default();
        let token = word.token.text.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Anchors are best-effort: a down service still yields the
            // symbol bubble (OI-1).
            let anchors = if word.anchor_count > 0 && !ws.is_empty() {
                composer::ipc_to_workspaces::anchors_for_token(&ws, &token)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let _ = this.update(app_cx, |panel, cx| {
                if panel.bubbles.word_idx != Some(word_idx) {
                    return; // swapped/collapsed while fetching
                }
                panel.bubbles.bubbles = composer::bubbles::bubbles_for(&word, &anchors);
                cx.notify();
            });
        })
        .detach();
    }

    /// Expand one bubble's card (§5.4) and pull its drill-in context when
    /// it's a code symbol.
    pub(crate) fn expand_bubble(&mut self, bubble_ix: usize, cx: &mut Context<Self>) {
        if !self.bubbles.toggle_expanded(bubble_ix) {
            cx.notify();
            return;
        }
        cx.notify();
        let Some(composer::bubbles::BubbleKind::Symbol { id, .. }) =
            self.bubbles.bubbles.get(bubble_ix).map(|b| b.kind.clone())
        else {
            return; // anchor bubbles show their description; no fetch
        };
        let Some(ws) = self.active_workspace_id.clone() else {
            return;
        };
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let reply = composer::ipc_to_workspaces::symbol_context(&ws, &id).await;
            let _ = this.update(app_cx, |panel, cx| {
                if panel.bubbles.expanded != Some(bubble_ix) {
                    return;
                }
                match reply {
                    Ok(v) => {
                        panel.bubbles.context =
                            Some(composer::bubbles::CardContext::from_reply(&v));
                    }
                    // Surface the failure so the card stops spinning forever
                    // (e.g. workspaces service unreachable).
                    Err(_) => panel.bubbles.context_failed = true,
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Route one shared-menu action (Plan §6 — `wylde_anchor_actions`'
    /// first menu renderer) onto the composer's existing handlers.
    pub(crate) fn apply_bubble_menu_action(
        &mut self,
        action: wylde_anchor_actions::MenuAction,
        cx: &mut Context<Self>,
    ) {
        use wylde_anchor_actions::MenuAction;
        let Some(word_idx) = self.bubbles.word_idx else {
            return;
        };
        let target_label = self.bubbles.menu_target_label();
        self.bubbles.menu = None;
        match action {
            MenuAction::ToggleExclude { .. } => {
                self.toggle_word_excluded_undoable(word_idx, cx);
            }
            MenuAction::TogglePin { .. } => {
                if let Some(label) =
                    target_label.or_else(|| self.bubbles.bubbles.first().map(|b| b.label.clone()))
                {
                    self.toggle_bubble_pin_undoable(&label, cx);
                }
            }
            MenuAction::ToggleIgnore { tier, .. } => {
                let tag = match tier {
                    wylde_anchor_actions::IgnoreTier::Conversation => IgnoreTierTag::Conversation,
                    wylde_anchor_actions::IgnoreTier::Workspace => IgnoreTierTag::Workspace,
                    wylde_anchor_actions::IgnoreTier::Global => IgnoreTierTag::Global,
                };
                self.toggle_ignore_tier(word_idx, tag, cx);
            }
            // Rows the composer can't route yet are filtered at render
            // (cross-panel routing / drawing mode — see the slice report).
            _ => {}
        }
        cx.notify();
    }

    /// Mint a symbol anchor for a disambiguated word (Slice N's
    /// "Anchor this?" — the composer's anchor-creation entry point).
    pub(crate) fn create_anchor_for_word(&mut self, word_idx: usize, cx: &mut Context<Self>) {
        let Some(ws) = self.active_workspace_id.clone() else {
            self.error = Some("No active workspace to anchor into".to_owned());
            cx.notify();
            return;
        };
        let Some(word) = self.composer.words.get(word_idx) else {
            return;
        };
        let Some(sym) = word.effective_symbol() else {
            return;
        };
        let identifier = word.token.text.clone();
        let symbol_id = sym.id.clone();
        let description = format!("Symbol {} ({}:{})", sym.name, sym.file, sym.line);
        self.composer.anchor_offer = None;
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = composer::ipc_to_workspaces::create_symbol_anchor(
                &ws,
                &identifier,
                &symbol_id,
                &description,
            )
            .await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(_) => {
                        // The new anchor shows in the word's chip count
                        // immediately; the next scan re-reads the store.
                        if let Some(w) = panel
                            .composer
                            .words
                            .iter_mut()
                            .find(|w| w.token.text == identifier)
                        {
                            w.anchor_count += 1;
                        }
                    }
                    Err(e) => panel.error = Some(format!("Anchor creation failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a right-click ignore-menu choice (Slice M): add/remove `token`
    /// in one durable tier, then patch the word's local tier tags so the
    /// chip re-renders without a rescan. The mutation is fire-and-confirm —
    /// a failed pipe write leaves the local state untouched.
    pub(crate) fn toggle_ignore_tier(
        &mut self,
        word_idx: usize,
        tier: IgnoreTierTag,
        cx: &mut Context<Self>,
    ) {
        let Some(w) = self.composer.words.get(word_idx) else {
            return;
        };
        let token = w.token.text.clone();
        let currently = w.ignored_tiers.contains(&tier);
        let ws = self.active_workspace_id.clone().unwrap_or_default();
        let conv = self.conversation_id.clone();
        self.composer.ignore_menu = None;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome =
                composer::ipc_to_workspaces::set_ignored(&ws, &conv, tier, &token, !currently)
                    .await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(()) => {
                        if let Some(w) = panel
                            .composer
                            .words
                            .iter_mut()
                            .find(|w| w.token.text == token)
                        {
                            if currently {
                                w.ignored_tiers.retain(|t| *t != tier);
                            } else {
                                w.ignored_tiers.push(tier);
                                // A fresh ignore starts deselected (§5.8).
                                w.reactivated = false;
                            }
                        }
                        panel.sync_prompt_highlights(cx);
                    }
                    Err(e) => {
                        panel.error = Some(format!("Ignore update failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for ChatPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Reconcile the virtualized list with `messages` before anything reads it.
        self.sync_message_list();
        let inference_bar = inference_bar(self, cx);
        let log = message_log(self, cx);
        let consent_strip = consent_card_strip(self, cx);

        let mut body =
            div()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(pack(SURFACE_900)))
                // Composer keys the input doesn't claim (Ctrl+P palette, Esc).
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    this.on_panel_key(ev, window, cx)
                }))
                .child(log)
                .child(consent_strip);

        if let Some(err) = &self.error {
            body = body.child(error_strip(err));
        }
        body.child(inference_bar)
    }
}

/// A thin View that renders ONLY the shared [`ChatPanel`]'s InferenceBar — the
/// docked composer at the bottom of the Workspaces in-workspace view (UX rework
/// decision 6). Holds the shared singleton entity and delegates its render into
/// it, so the docked bar reflects the exact same conversation / model picker /
/// composer state as the Chat panel — full bar, shared conversation.
///
/// Safe because the Shell shows one panel at a time: the InferenceBar is only
/// ever rendered in one place per frame (the Chat slot OR this dock, never
/// both), so the shared `prompt_input` and the bar's fixed ElementIds never
/// collide.
pub struct InferenceBarDock {
    chat: Entity<ChatPanel>,
}

impl InferenceBarDock {
    /// Registry/factory entry — builds the dock over the **Docked** ChatPanel
    /// singleton (creating + wiring it on first use). C1: this is a *separate*
    /// entity from the Chat slot's Global singleton, so the dock owns its own
    /// conversation scope.
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        let chat = ChatPanel::docked(cx);
        cx.new(|_cx| Self { chat }).into()
    }

    /// The Docked ChatPanel entity this dock renders (test/inspection hook).
    pub fn chat(&self) -> &Entity<ChatPanel> {
        &self.chat
    }
}

impl Render for InferenceBarDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Delegate into the Docked ChatPanel: build its InferenceBar with the
        // chat entity's own context, so every listener routes back to the docked
        // panel (and thus its own scoped conversation), not this dock wrapper.
        self.chat.update(cx, |panel, ccx| inference_bar(panel, ccx))
    }
}

fn message_log(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> AnyElement {
    if panel.messages.is_empty() {
        return div()
            .id(ElementId::Name("chat-log".into()))
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .font_family(FAMILY_INTER)
            .text_size(px(size::SM))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from("How can I help?"))
            .into_any_element();
    }

    // Virtualized log: `list` invokes this closure only for the items it needs
    // to paint (visible range + overdraw), so render cost is bounded regardless
    // of how long the conversation is. The closure can't borrow `panel` (it must
    // be `'static`), so it reads the live messages back out of the entity by
    // index each time an item is (re)built — the same bubble builders as before.
    //
    // Each list item is exactly one message (item count == messages.len(), the
    // reconciler's contract), with the chat-processing-indicator extras nested
    // *under* the bubble inside the same item: a settled assistant turn's
    // collapsible activity disclosure, or the live animated indicator on the
    // in-flight tail bubble. Both are interactive, so they dispatch through the
    // panel `entity` handle — the `list` closure only hands out `&mut App`, not
    // a `Context<ChatPanel>`, so `cx.listener` isn't available here.
    let entity = cx.entity();
    list(panel.message_list.clone(), move |ix, _window, cx| {
        let Some(m) = entity.read(cx).messages.get(ix).cloned() else {
            // Index briefly out of range between a `messages` mutation and the
            // next `sync_message_list` — render nothing rather than panic.
            return div().into_any_element();
        };
        // `gap_3` between bubbles in the old flex column = a 12px top gap on
        // every item but the first; reproduce it exactly here. The bubble and
        // its extras stack in a column so they read as one message block.
        let mut item = div()
            .when(ix > 0, |d| d.pt_3())
            .flex()
            .flex_col()
            .gap_3()
            .child(bubble(&m));
        // A settled assistant turn keeps its activity as a collapsible
        // disclosure under the bubble (chat-processing-indicator).
        if m.role == MessageRole::Assistant && !m.streaming {
            if let Some(act) = &m.activity {
                if !act.is_empty() {
                    item = item.child(message_activity_disclosure(
                        &m.id,
                        act,
                        m.activity_expanded,
                        &entity,
                    ));
                }
            }
        }
        // The in-flight assistant bubble carries the live, animated status
        // indicator in place of the old static `…`.
        if m.role == MessageRole::Assistant && m.streaming {
            if let Some(p) = entity.read(cx).processing.clone() {
                item = item.child(processing_indicator(&p, &entity));
            }
        }
        item.into_any_element()
    })
    .flex_1()
    .p_5()
    .into_any_element()
}

/// The live, animated processing status (chat-processing-indicator): a
/// clickable row — bouncing dots + the current phase + a live token meter +
/// a ▸/▾ chevron — over an expandable activity dropdown. Every sub-part
/// degrades gracefully: no phase signal ⇒ "Working"; no usage ⇒ no meter;
/// nothing logged ⇒ no chevron / dropdown.
fn processing_indicator(p: &ProcessingState, view: &Entity<ChatPanel>) -> gpui::Div {
    let label = format!("{}{}", p.phase.label(), processing_dots(p.tick));
    let has_detail = !p.log.is_empty() || !p.thinking.is_empty();

    // Three bouncing dots — the bright one cycles with the tick. Plain
    // rounded divs, so no glyph/font dependency.
    let active = processing::active_dot(p.tick);
    let mut dots = div().flex().flex_row().gap_1().items_center().mr_1();
    for i in 0..3usize {
        let color = if i == active { BRAND } else { TEXT_MUTED };
        dots = dots.child(div().size(px(6.0)).rounded(px(3.0)).bg(rgb(pack(color))));
    }

    let mut row = control(div(), ElementId::Name("chat-processing".into()))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(dots)
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(label)),
        );

    // Live token meter, when the stream has reported any usage.
    if let Some(meter) = p.token_meter() {
        row = row.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(meter)),
        );
    }

    // Expand affordance — only when there's something to expand.
    if has_detail {
        row = row
            .cursor_pointer()
            .child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(if p.expanded { "▾" } else { "▸" })),
            )
            .on_mouse_down(gpui::MouseButton::Left, {
                let view = view.clone();
                move |_ev, _w: &mut Window, cx: &mut App| {
                    view.update(cx, |this, cx| this.toggle_processing_expanded(cx));
                }
            });
    }

    let mut col = div().flex().flex_col().gap_1().max_w(px(720.0)).child(row);
    if has_detail && p.expanded {
        col = col.child(activity_dropdown(&p.log, &p.thinking, p.token_detail()));
    }
    col
}

/// The expandable activity dropdown body — the honest, full-visibility log of
/// what the turn did, **grouped** Claude-style into Context (the gather
/// pipeline), Tools (each call with its args/result), and Thinking (the
/// model's reasoning), plus the prompt/completion token split. Each group is
/// rendered only when it has content, so a turn never shows an empty section.
/// Shared by the live indicator and the settled-message disclosure.
fn activity_dropdown(
    entries: &[processing::ActivityEntry],
    thinking: &str,
    token_detail: Option<String>,
) -> gpui::Div {
    let mut body = div()
        .flex()
        .flex_col()
        .gap_2()
        .ml_4()
        .pl_3()
        .border_l_2()
        .border_color(rgb(pack(BORDER_SUBTLE)));

    // Context (the gather pipeline), Plan (the reasoning tier's grounded
    // step checklist — agentic-reasoning S3), and Tools sections, in time
    // order within each. The Thinking marker rows are skipped here — the
    // reasoning text is rendered as its own block below.
    let context: Vec<&processing::ActivityEntry> =
        entries.iter().filter(|e| e.kind.group() == 0).collect();
    let plan: Vec<&processing::ActivityEntry> =
        entries.iter().filter(|e| e.kind.group() == 3).collect();
    let tools: Vec<&processing::ActivityEntry> =
        entries.iter().filter(|e| e.kind.group() == 1).collect();

    if !context.is_empty() {
        body = body.child(activity_section("Context", &context));
    }
    if !plan.is_empty() {
        body = body.child(activity_section("Plan", &plan));
    }
    if !tools.is_empty() {
        body = body.child(activity_section("Tools", &tools));
    }

    if !thinking.is_empty() {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(activity_section_header("Thinking"))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(thinking.to_owned())),
                ),
        );
    }

    if let Some(detail) = token_detail {
        body = body.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(format!("tokens · {detail}"))),
        );
    }

    body
}

/// A small uppercase section header for the grouped activity dropdown.
fn activity_section_header(title: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(title.to_uppercase()))
}

/// One titled group of activity rows.
fn activity_section(title: &str, rows: &[&processing::ActivityEntry]) -> gpui::Div {
    let mut sec = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(activity_section_header(title));
    for e in rows {
        sec = sec.child(activity_log_row(e));
    }
    sec
}

/// One row in the activity dropdown — a small coloured kind-glyph + text, with
/// the optional `detail` (concept names / tool args / output) on a dim,
/// indented second line for full visibility without crowding the line.
fn activity_log_row(e: &processing::ActivityEntry) -> gpui::Div {
    use processing::ActivityKind;
    let (glyph, color) = match e.kind {
        ActivityKind::Phase => ("▸", TEXT_MUTED),
        ActivityKind::Step => ("•", TEXT_SECONDARY),
        ActivityKind::Tool => ("→", TEXT_SECONDARY),
        ActivityKind::ToolOk => ("✓", BRAND),
        ActivityKind::ToolErr => ("✕", BORDER_EMPHASIS),
        ActivityKind::Memory => ("✦", TEXT_SECONDARY),
        ActivityKind::Thinking => ("…", TEXT_MUTED),
        ActivityKind::Reasoning => ("◆", TEXT_SECONDARY),
    };
    let head = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .child(
            div()
                .w(px(12.0))
                .text_color(rgb(pack(color)))
                .child(SharedString::from(glyph)),
        )
        .child(
            div()
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(e.text.clone())),
        );

    let mut row = div().flex().flex_col().child(head);
    if let Some(detail) = &e.detail {
        row = row.child(
            div()
                .ml(px(20.0))
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(detail.clone())),
        );
    }
    row
}

/// The persisted "Activity" disclosure under a settled assistant bubble: a
/// collapsed one-line summary ("Used 2 tools · 1.2k tokens") that expands to
/// the full activity log. Mirrors Claude's collapsible tool-use sections.
fn message_activity_disclosure(
    id: &str,
    act: &MessageActivity,
    expanded: bool,
    view: &Entity<ChatPanel>,
) -> gpui::Div {
    let id_owned = id.to_owned();
    let header = control(div(), ElementId::Name(format!("chat-activity-{id}").into()))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(if expanded { "▾" } else { "▸" })),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(act.summary())),
        )
        .on_mouse_down(gpui::MouseButton::Left, {
            let view = view.clone();
            move |_ev, _w: &mut Window, cx: &mut App| {
                view.update(cx, |this, cx| this.toggle_message_activity(&id_owned, cx));
            }
        });

    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .max_w(px(720.0))
        .child(header);
    if expanded {
        col = col.child(activity_dropdown(
            &act.log,
            "",
            match (act.prompt_tokens, act.completion_tokens) {
                (Some(p), Some(c)) => Some(format!(
                    "{} in · {} out",
                    processing::fmt_tokens(p),
                    processing::fmt_tokens(c)
                )),
                _ => None,
            },
        ));
    }
    col
}

/// Animated trailing ellipsis for the status label — cycles `""`, `"."`,
/// `".."`, `"..."` so the line reads as live even when no tokens are
/// arriving (the streaming driver emits text in bulk).
fn processing_dots(tick: u64) -> String {
    ".".repeat((tick % 4) as usize)
}

fn bubble(m: &ChatMessage) -> gpui::Div {
    match m.role {
        MessageRole::User => user_bubble(&m.content),
        MessageRole::Assistant => assistant_bubble(m),
        MessageRole::System => system_bubble(&m.content),
    }
}

fn user_bubble(content: &str) -> gpui::Div {
    div().flex().flex_row().justify_end().child(
        div()
            .max_w(px(560.0))
            .bg(rgb(pack(SURFACE_700)))
            .rounded(px(12.0))
            .px_4()
            .py_2()
            .font_family(FAMILY_INTER)
            .text_size(px(size::SM))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(content.to_owned())),
    )
}

fn assistant_bubble(m: &ChatMessage) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_1().max_w(px(720.0));

    if let Some(thinking) = &m.thinking {
        col = col.child(
            div()
                .border_l_2()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .pl_3()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(format!("thinking: {thinking}"))),
        );
    }

    // While streaming and still empty, the live processing indicator
    // (rendered alongside by `message_log`) stands in for the reply — no
    // static `…` placeholder. Once the first token arrives we switch to
    // markdown so the partial reply already starts formatting.
    if m.streaming && m.content.is_empty() {
        return col;
    }

    let blocks = markdown::parse(&m.content);
    col = col.child(markdown::render(&blocks, &m.id));
    col
}

fn system_bubble(content: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(content.to_owned()))
}

fn consent_card_strip(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut strip = div().flex().flex_col().gap_2();
    if panel.pending_consents.is_empty() {
        return strip;
    }
    strip = strip.px_5().pb_2();
    for entry in panel.pending_consents.values() {
        strip = strip.child(consent_card(entry, cx));
    }
    strip
}

fn consent_card(entry: &PendingConsent, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let prompt_id = entry.id.clone();
    let tool_id = entry.tool.clone();
    let summary_label = SharedString::from(if entry.summary.is_empty() {
        format!("Tool {} requests authorization.", entry.tool)
    } else {
        entry.summary.clone()
    });
    let tool_label = SharedString::from(entry.tool.clone());

    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(8.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("Requires approval"))
                .child(div().text_color(rgb(pack(BRAND))).child(tool_label)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(summary_label),
        )
        .child(consent_button_row(prompt_id, tool_id, cx))
}

fn consent_button_row(
    prompt_id: String,
    tool_id: String,
    cx: &mut Context<ChatPanel>,
) -> gpui::Div {
    let allow_id_persist = prompt_id.clone();
    let allow_tool_persist = tool_id.clone();
    let allow_id_once = prompt_id.clone();
    let allow_tool_once = tool_id.clone();
    let deny_id = prompt_id.clone();
    let deny_tool = tool_id.clone();

    div()
        .flex()
        .flex_row()
        .gap_2()
        .child(consent_button(
            ElementId::Name(format!("consent-allow::{prompt_id}").into()),
            "Allow",
            cx.listener(move |_this: &mut ChatPanel, _ev, _window, cx| {
                ChatPanel::spawn_respond_consent(
                    allow_id_persist.clone(),
                    allow_tool_persist.clone(),
                    "approved",
                    true,
                    cx,
                );
            }),
        ))
        .child(consent_button(
            ElementId::Name(format!("consent-once::{prompt_id}").into()),
            "Once",
            cx.listener(move |_this: &mut ChatPanel, _ev, _window, cx| {
                ChatPanel::spawn_respond_consent(
                    allow_id_once.clone(),
                    allow_tool_once.clone(),
                    "approved",
                    false,
                    cx,
                );
            }),
        ))
        .child(consent_button(
            ElementId::Name(format!("consent-deny::{prompt_id}").into()),
            "Deny",
            cx.listener(move |_this: &mut ChatPanel, _ev, _window, cx| {
                ChatPanel::spawn_respond_consent(
                    deny_id.clone(),
                    deny_tool.clone(),
                    "denied",
                    true,
                    cx,
                );
            }),
        ))
}

fn consent_button<F>(id: ElementId, label: &str, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    control(div(), id)
        .px_3()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from(label.to_owned()))
}

fn inference_bar(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut bar = div()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_3()
        .bg(rgb(pack(SURFACE_800)))
        .border_t_1()
        .border_color(rgb(pack(BORDER_DEFAULT)));

    // First row: pills (conversations + workspace + model).
    bar = bar.child(pill_row(panel, cx));
    if panel.show_conversations {
        bar = bar.child(conversations_panel(panel, cx));
    }
    // D1: workspace dropdown only on a bound (Docked) surface — Global can no
    // longer reach the toggle, but gate the render too so the surface stays
    // structurally workspace-free even if the flag were ever set elsewhere.
    if panel.show_ws_dropdown && panel.scope.allows_workspace_bind() {
        bar = bar.child(workspace_dropdown(panel, cx));
    }
    if panel.show_model_dropdown {
        bar = bar.child(model_dropdown(panel, cx));
    }
    if panel.show_working_memory {
        bar = bar.child(working_memory_panel(panel, cx));
    }

    // Symbol-aware composer surfaces (Slice F): chip strip, disambiguation,
    // curate-before-send, Ctrl+P palette.
    bar = crate::composer_ui::mount(bar, panel, cx);

    // Second row: prompt input + send/stop button.
    bar = bar.child(prompt_row(panel, cx));
    bar
}

fn pill_row(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let workspace_label = SharedString::from(match &panel.active_workspace_id {
        Some(id) => format!("workspace · {id}"),
        None => "workspace · none".to_owned(),
    });
    let model_label = SharedString::from(match &panel.active_model {
        Some(m) => format!("model · {m}"),
        None => "model · auto".to_owned(),
    });
    let mut row = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(conversations_pill(panel, cx));

    // D1: the workspace affordances (label/dropdown toggle + folder picker) exist
    // only on a *bound* surface — the Workspaces dock. The Global Chat slot is
    // structurally workspace-free: no opt-in, no "bind this chat" control, so the
    // pills are simply not rendered there.
    if panel.scope.allows_workspace_bind() {
        row = row
            .child(pill_button(
                ElementId::Name("chat-ws-toggle".into()),
                workspace_label,
                cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                    this.toggle_ws_dropdown(cx);
                }),
            ))
            .child(pill_button(
                ElementId::Name("chat-ws-pick".into()),
                SharedString::from("+ folder"),
                cx.listener(|_this: &mut ChatPanel, _ev, _window, cx| {
                    ChatPanel::spawn_pick_workspace(cx);
                }),
            ));
    }

    let mut row = row
        .child(pill_button(
            ElementId::Name("chat-model-toggle".into()),
            model_label,
            cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                this.toggle_model_dropdown(cx);
            }),
        ))
        .child(reasoning_depth_pill(panel, cx))
        .child(reason_mode_pill(panel, cx));
    // Inline VRAM fit chip (readiness-chip pattern): advisory only, renders
    // solely when reasoning is enabled and the fit probe warned.
    if let Some(warning) = &panel.fit_warning {
        row = row.child(fit_chip(warning.clone()));
    }
    row.child(eject_button(panel, cx))
        .child(working_memory_pill(panel, cx))
}

/// Thinking-tier pill (the maintainer's confirmed InferenceBar placement). Each
/// click cycles one tier up: fast → think → think harder → ultrathink →
/// fast. Defaults to `fast` (never planning-by-default); the tier rides
/// the send payload as `depth`.
fn reasoning_depth_pill(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let label = SharedString::from(format!("reasoning · {}", panel.reasoning_depth.label()));
    pill_button(
        ElementId::Name("chat-reasoning-toggle".into()),
        label,
        cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
            this.toggle_reasoning_depth(cx);
        }),
    )
}

/// Split/Single selector pill (agentic reasoning S1 — the maintainer's confirmed
/// InferenceBar placement, beside the fast/deep toggle). One click flips
/// the mode and persists it through `settings.reasoning.set`. Inert while
/// the reasoning master toggle is off — the harness never consults the
/// mode on a fast/off turn.
fn reason_mode_pill(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let label = SharedString::from(format!("mode · {}", panel.reason_mode.as_str()));
    pill_button(
        ElementId::Name("chat-reason-mode-toggle".into()),
        label,
        cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
            this.toggle_reason_mode(cx);
        }),
    )
}

/// Inline VRAM fit warning chip (readiness-chip style, scope §3.2): the
/// fit picker warns, never blocks — so this is a muted advisory tag, not
/// an error banner.
fn fit_chip(warning: String) -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(format!("⚠ {warning}")))
}

/// Working-memory toggle pill — shows the live entry count for the active
/// conversation and opens the strip listing them.  Mirrors the workspace /
/// model pills so the InferenceBar row stays visually uniform.
fn working_memory_pill(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let label = SharedString::from(format!("memory · {}", panel.working_memory.len()));
    pill_button(
        ElementId::Name("chat-wm-toggle".into()),
        label,
        cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
            this.toggle_working_memory(cx);
        }),
    )
}

/// The working-memory strip: a header row ("Working memory" + a Clear
/// button) over the per-entry rows for the active conversation.  Each row
/// is the entry `kind` tag + its one-line `summary`.  Empty buffer → a
/// muted hint instead of an empty box.
fn working_memory_panel(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut col = dropdown_shell();

    // Header: title + Clear (disabled when the buffer is already empty).
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .pb_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("Working memory")),
        );
    if !panel.working_memory.is_empty() {
        header = header.child(
            control(div(), ElementId::Name("chat-wm-clear".into()))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                        this.clear_working_memory(cx);
                    }),
                )
                .child(SharedString::from("Clear")),
        );
    }
    col = col.child(header);

    if panel.working_memory.is_empty() {
        return col.child(empty_dropdown_row(
            "No working memory yet — entries accrue as Wylde works this conversation.",
        ));
    }

    for entry in &panel.working_memory {
        col = col.child(working_memory_row(entry));
    }
    col
}

/// One working-memory row: a `kind` tag pill + the entry's summary line.
fn working_memory_row(entry: &WorkingMemoryEntry) -> gpui::Div {
    let summary = if entry.summary.is_empty() {
        SharedString::from("(no detail)")
    } else {
        SharedString::from(entry.summary.clone())
    };
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_start()
        .py_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BRAND)))
                .child(SharedString::from(entry.kind.clone())),
        )
        .child(
            div()
                .flex_1()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(summary),
        )
}

/// Conversation switcher toggle pill — shows the saved-chat count and
/// opens the rail. Mirrors the workspace / model / memory pills so the
/// InferenceBar row stays visually uniform.
fn conversations_pill(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let label = SharedString::from(format!("chats · {}", panel.conversations.len()));
    pill_button(
        ElementId::Name("chat-conversations-toggle".into()),
        label,
        cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
            this.toggle_conversations(cx);
        }),
    )
}

/// The conversation rail: a header ("Conversations" + "+ New") over one
/// row per saved chat, newest-first. The active conversation is
/// highlighted; each row carries a delete affordance that swaps to an
/// inline confirm. Empty list → a muted hint (the current chat may be a
/// fresh, not-yet-persisted conversation).
fn conversations_panel(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut col = dropdown_shell();

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .pb_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("Conversations")),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(
                    control(
                        // Slice J: import a portable conversation export file.
                        div(),
                        ElementId::Name("chat-conversation-import".into()),
                    )
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(rgb(pack(BORDER_SUBTLE)))
                    .cursor_pointer()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::MICRO))
                    .text_color(rgb(pack(TEXT_SECONDARY)))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_this: &mut ChatPanel, _ev, _window, cx| {
                            ChatPanel::spawn_import_conversation(cx);
                        }),
                    )
                    .child(SharedString::from("Import…")),
                )
                .child(
                    control(div(), ElementId::Name("chat-conversation-new".into()))
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(rgb(pack(BORDER_SUBTLE)))
                        .cursor_pointer()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(BRAND)))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_this: &mut ChatPanel, _ev, _window, cx| {
                                ChatPanel::spawn_new_conversation(cx);
                            }),
                        )
                        .child(SharedString::from("+ New")),
                ),
        );
    col = col.child(header);

    // Slice J: outcome of the last export/import, one quiet line.
    if let Some(status) = &panel.transfer_status {
        let (text, color) = match status {
            Ok(msg) => (msg.clone(), TEXT_MUTED),
            Err(msg) => (msg.clone(), BORDER_EMPHASIS),
        };
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(color)))
                .pb_1()
                .child(SharedString::from(text)),
        );
    }

    if panel.conversations.is_empty() {
        return col.child(empty_dropdown_row(
            "No saved conversations yet — send a message to start one.",
        ));
    }

    for meta in &panel.conversations {
        let is_active = meta.id == panel.conversation_id;
        let confirming = panel.confirm_delete.as_deref() == Some(meta.id.as_str());
        col = col.child(conversation_row(meta, is_active, confirming, cx));
    }
    col
}

/// One conversation row: a clickable title + meta block (select target) on
/// the left, and a delete affordance on the right that swaps to an inline
/// "Delete? / Cancel" confirm. Active rows get a brand-tinted border.
fn conversation_row(
    meta: &ConversationMeta,
    is_active: bool,
    confirming: bool,
    cx: &mut Context<ChatPanel>,
) -> gpui::Div {
    let id_for_select = meta.id.clone();
    let meta_line = SharedString::from(format!(
        "{}  ·  {} msg  ·  mem {}",
        relative_time(meta.updated_at),
        meta.message_count,
        meta.working_memory_count,
    ));
    let title = SharedString::from(meta.title.clone());

    // Left: the select target. A nested clickable block (not the whole
    // row) so the delete control on the right doesn't double-fire select.
    let select_block = control(
        div(),
        ElementId::Name(format!("chat-conversation-pick::{}", meta.id).into()),
    )
    .flex_1()
    .flex()
    .flex_col()
    .gap(px(1.0))
    .cursor_pointer()
    .on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |this: &mut ChatPanel, _ev, window, cx| {
            this.select_conversation(&id_for_select, window, cx);
        }),
    )
    .child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(if is_active {
                TEXT_PRIMARY
            } else {
                TEXT_SECONDARY
            })))
            .child(title),
    )
    .child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(meta_line),
    );

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(if is_active {
            BORDER_EMPHASIS
        } else {
            SURFACE_900
        })));
    if is_active {
        row = row.bg(rgb(pack(SURFACE_800)));
    }
    row = row.child(select_block);

    row = row.child(export_button(&meta.id, cx));
    if confirming {
        row = row.child(delete_confirm_controls(&meta.id, cx));
    } else {
        row = row.child(delete_request_button(&meta.id, cx));
    }
    row
}

/// The "⤓" affordance — export this conversation to a file (Slice J).
fn export_button(id: &str, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let id_for_click = id.to_owned();
    control(
        div(),
        ElementId::Name(format!("chat-conversation-export::{id}").into()),
    )
    .px_2()
    .py_1()
    .rounded(px(4.0))
    .cursor_pointer()
    .font_family(FAMILY_INTER)
    .text_size(px(size::XS))
    .text_color(rgb(pack(TEXT_MUTED)))
    .on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |_this: &mut ChatPanel, _ev, _window, cx| {
            ChatPanel::spawn_export_conversation(id_for_click.clone(), cx);
        }),
    )
    .child(SharedString::from("⤓"))
}

/// The "×" affordance that arms the inline delete confirm for `id`.
fn delete_request_button(id: &str, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let id_for_click = id.to_owned();
    control(
        div(),
        ElementId::Name(format!("chat-conversation-del::{id}").into()),
    )
    .px_2()
    .py_1()
    .rounded(px(4.0))
    .cursor_pointer()
    .font_family(FAMILY_INTER)
    .text_size(px(size::XS))
    .text_color(rgb(pack(TEXT_MUTED)))
    .on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(move |this: &mut ChatPanel, _ev, _window, cx| {
            this.request_delete_conversation(&id_for_click, cx);
        }),
    )
    .child(SharedString::from("×"))
}

/// Inline delete confirmation: a "Delete" (destructive) + "Cancel" pair
/// that replaces the "×" affordance while `confirm_delete` points at this
/// row. Standing in for a modal — same row idiom, no new gpui primitive.
fn delete_confirm_controls(id: &str, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let id_confirm = id.to_owned();
    div()
        .flex()
        .flex_row()
        .gap_1()
        .items_center()
        .child(
            control(
                div(),
                ElementId::Name(format!("chat-conversation-del-yes::{id}").into()),
            )
            .px_2()
            .py_1()
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(pack(BORDER_EMPHASIS)))
            .cursor_pointer()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this: &mut ChatPanel, _ev, _window, cx| {
                    this.confirm_delete_conversation(&id_confirm, cx);
                }),
            )
            .child(SharedString::from("Delete")),
        )
        .child(
            control(
                div(),
                ElementId::Name(format!("chat-conversation-del-no::{id}").into()),
            )
            .px_2()
            .py_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                    this.cancel_delete_conversation(cx);
                }),
            )
            .child(SharedString::from("Cancel")),
        )
}

/// Render an epoch-seconds `updated_at` as a compact relative age
/// ("just now", "5m", "3h", "2d"). Falls back to "—" for an unset (0)
/// timestamp or a clock that's behind the stored value.
fn relative_time(updated_at: i64) -> String {
    if updated_at <= 0 {
        return "—".to_owned();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - updated_at;
    if delta < 0 {
        return "just now".to_owned();
    }
    if delta < 60 {
        "just now".to_owned()
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

/// Eject button — releases the active model from VRAM via `ollama.eject`.
/// Enabled only when a concrete model is selected (not "(auto)") and no
/// eject is in flight; dimmed + click-inert otherwise.  Sits to the right
/// of the model pill in the InferenceBar row.
fn eject_button(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    // Enabled iff we have a concrete model name to send and aren't already
    // mid-eject.  "(auto)" has no name → nothing to release → disabled.
    let enabled = panel.active_model.is_some() && !panel.ejecting;
    let (text_color, border_color) = if enabled {
        (rgb(pack(TEXT_SECONDARY)), rgb(pack(BORDER_SUBTLE)))
    } else {
        (rgb(pack(TEXT_MUTED)), rgb(pack(BORDER_SUBTLE)))
    };
    let mut btn = control(div(), ElementId::Name("chat-model-eject".into()))
        .px_3()
        .py_1()
        .rounded(px(12.0))
        .border_1()
        .border_color(border_color)
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(text_color)
        // ⏏ eject glyph; trailing "…" while the round-trip is in flight.
        .child(SharedString::from(if panel.ejecting {
            "⏏ …"
        } else {
            "⏏"
        }));
    if enabled {
        btn = btn.cursor_pointer().on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                this.eject_active_model(cx);
            }),
        );
    }
    btn
}

fn pill_button<F>(id: ElementId, label: SharedString, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    control(div(), id)
        .px_3()
        .py_1()
        .rounded(px(12.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(label)
}

fn workspace_dropdown(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut col = dropdown_shell();
    if panel.workspaces.is_empty() {
        col = col.child(empty_dropdown_row("No workspaces yet"));
        return col;
    }
    for ws in &panel.workspaces {
        let id_for_select = ws.id.clone();
        col = col.child(dropdown_row(
            ElementId::Name(format!("chat-ws-pick::{}", ws.id).into()),
            SharedString::from(format!("{}  ·  {}", ws.id, ws.path)),
            cx.listener(move |this: &mut ChatPanel, _ev, window, cx| {
                this.select_workspace(&id_for_select, window, cx);
            }),
        ));
    }
    col
}

fn model_dropdown(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let mut col = dropdown_shell();
    // "(auto)" is always first — clears active_model, defers to harness
    // default_model.
    col = col.child(dropdown_row(
        ElementId::Name("chat-model-pick::auto".into()),
        SharedString::from("(auto)  ·  harness default_model"),
        cx.listener(|this: &mut ChatPanel, _ev, window, cx| {
            this.select_model(None, window, cx);
        }),
    ));
    if panel.models.is_empty() {
        col = col.child(empty_dropdown_row("Ollama offline — no models discovered"));
        return col;
    }
    for m in &panel.models {
        let m_for_select = m.clone();
        col = col.child(dropdown_row(
            ElementId::Name(format!("chat-model-pick::{m}").into()),
            SharedString::from(m.clone()),
            cx.listener(move |this: &mut ChatPanel, _ev, window, cx| {
                this.select_model(Some(m_for_select.clone()), window, cx);
            }),
        ));
    }
    col
}

fn dropdown_shell() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .bg(rgb(pack(SURFACE_900)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_2()
}

fn empty_dropdown_row(text: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(text.to_owned()))
}

fn dropdown_row<F>(id: ElementId, label: SharedString, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    control(div(), id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(label)
}

fn prompt_row(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> gpui::Div {
    let streaming = panel.active_turn_id.is_some();

    // Right-hand button: Stop while streaming, Send otherwise.  Both
    // sit in the same place to keep the InferenceBar's layout stable.
    let button = if streaming {
        stop_button(cx)
    } else {
        send_button(panel.prompt_input.clone(), cx)
    };

    // §5.2: left-clicking a highlighted word in the input spawns its
    // bubble set. The wrapper listener fires alongside the input's own
    // caret positioning (the input doesn't stop propagation); a click on
    // plain prose maps to no recognized word and does nothing extra.
    let input_for_click = panel.prompt_input.clone();
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_end()
        .child(
            div()
                .flex_1()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, ev: &MouseDownEvent, _w, cx| {
                        let Some(ix) = input_for_click.read(cx).index_at_point(ev.position) else {
                            return;
                        };
                        let hit = this.composer.words.iter().position(|w| {
                            w.is_recognized() && w.token.start <= ix && ix < w.token.end
                        });
                        if let Some(word_idx) = hit {
                            this.open_word_bubbles(word_idx, cx);
                        }
                    }),
                )
                .child(panel.prompt_input.clone()),
        )
        .child(button)
}

fn send_button(input: Entity<TextInput>, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let listener_input = input.clone();
    control(div(), ElementId::Name("chat-send".into()))
        .px_4()
        .py_2()
        .rounded(px(8.0))
        .bg(rgb(pack(BRAND)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ChatPanel, _ev, _window, cx| {
                let text = listener_input.read(cx).text().to_owned();
                this.submit_text(text, &listener_input, cx);
            }),
        )
        .child(SharedString::from("Send"))
}

fn stop_button(cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    control(div(), ElementId::Name("chat-stop".into()))
        .px_4()
        .py_2()
        .rounded(px(8.0))
        .bg(rgb(pack(BRAND_DIM)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                this.cancel_active_turn(cx);
            }),
        )
        .child(SharedString::from("Stop"))
}

fn error_strip(msg: &str) -> gpui::Div {
    div()
        .mx_5()
        .mb_2()
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_800)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts.
pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let _ = BRAND_DIM;
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Select project folder")
        .pick_folder()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Regression for the cold-start consent edge case.  Before the
    /// fix, `spawn_consent_subscription` subscribed exactly once and, on
    /// failure (harness pipe not up yet), set an error and returned —
    /// the stream stayed dead forever and every pending-tool prompt was
    /// dropped.  The fix loops with a capped exponential backoff.  This
    /// pins the reconnect *schedule* (the part that's pure and
    /// testable): it must start at the floor, double, and saturate at
    /// the ceiling rather than running away or hitting zero (which would
    /// busy-spin the pipe).
    #[test]
    fn consent_reconnect_backoff_doubles_and_caps() {
        // Floor is a sane non-zero starting point (no busy-spin).
        assert_eq!(CONSENT_RECONNECT_MIN, Duration::from_millis(250));
        assert!(CONSENT_RECONNECT_MIN > Duration::ZERO);

        // Doubles each step…
        let mut b = CONSENT_RECONNECT_MIN;
        b = next_consent_backoff(b);
        assert_eq!(b, Duration::from_millis(500));
        b = next_consent_backoff(b);
        assert_eq!(b, Duration::from_millis(1000));
        b = next_consent_backoff(b);
        assert_eq!(b, Duration::from_millis(2000));
        b = next_consent_backoff(b);
        assert_eq!(b, Duration::from_millis(4000));

        // …then saturates at the ceiling and never exceeds it, no matter
        // how many disconnects pile up.
        b = next_consent_backoff(b);
        assert_eq!(b, CONSENT_RECONNECT_MAX);
        for _ in 0..100 {
            b = next_consent_backoff(b);
            assert_eq!(b, CONSENT_RECONNECT_MAX);
            assert!(b <= CONSENT_RECONNECT_MAX);
        }
    }

    // ── C1: dock un-shared from the global singleton ─────────────────
    // No gpui-executor test harness exists in this crate (every test here is
    // pure logic — the consent-backoff test above pins "the part that's pure
    // and testable" by the same principle). So C1's two regressions are pinned
    // at the storage + wiring-decision level rather than by mounting live
    // entities: (1) the dock uses a SEPARATE singleton cell → two distinct
    // entities; (2) only the Global scope wires consent → exactly one
    // subscription across both surfaces.

    /// C1 un-shares the Workspaces dock from the Chat slot: each owns its own
    /// process-wide ChatPanel entity, stored in a SEPARATE cell. If the dock
    /// ever delegated back into the global singleton's cell, the two surfaces
    /// would again be the same live entity (the bug C1 reverses).
    #[test]
    fn dock_and_chat_slot_use_distinct_singletons() {
        assert!(
            !std::ptr::eq(shared_cell(), docked_cell()),
            "dock must not delegate into the global ChatPanel singleton cell",
        );
    }

    /// The just-merged single-consent invariant (UX rework decision 6) must
    /// survive the dock owning its own entity: with both surfaces mounted —
    /// the Chat slot (Global) and the Workspaces dock (Docked) — exactly ONE
    /// `consent.stream_pending` subscription is wired, never two competing
    /// prompts. Only Global wires it; the dock relies on the Global singleton's.
    #[test]
    fn only_global_scope_wires_consent_subscription() {
        assert!(ChatScope::Global.wires_consent());
        assert!(!ChatScope::Docked.wires_consent());

        // Both surfaces mounted → exactly one subscription, order-independent
        // (the dock may mount before the Chat slot).
        for mounted in [
            [ChatScope::Global, ChatScope::Docked],
            [ChatScope::Docked, ChatScope::Global],
        ] {
            let subscriptions = mounted.iter().filter(|s| s.wires_consent()).count();
            assert_eq!(subscriptions, 1, "exactly one consent subscription");
        }
    }

    /// The default scope is Global, so anything that builds a `ChatPanel`
    /// without naming a scope (or relies on the derive) stays on the safe,
    /// consent-owning surface rather than silently becoming a second dock.
    #[test]
    fn chat_scope_defaults_to_global() {
        assert_eq!(ChatScope::default(), ChatScope::Global);
    }

    // ── C2: Global Chat strictly workspace-free [D1] ─────────────────
    // Same testing principle as C1: no gpui-executor harness exists here, so the
    // structural invariant is pinned on the pure `ChatScope` resolver that *every*
    // workspace-id site routes through — the dropdown selection (`select_workspace`),
    // the folder picker (`spawn_pick_workspace`), and the turn-send read
    // (`send_user_message`). If any of those ever bound the Global slot, the value
    // they'd store/send is exactly `resolve_workspace_id`'s output, asserted None here.

    /// D1: the Global Chat surface is structurally unbound — there is no escape
    /// hatch to attach a workspace. Only the Docked dock may bind one. This pins
    /// the predicate the render uses to hide the workspace pills on Global.
    #[test]
    fn only_docked_scope_allows_workspace_bind() {
        assert!(
            !ChatScope::Global.allows_workspace_bind(),
            "Global Chat must be structurally workspace-free (D1)",
        );
        assert!(
            ChatScope::Docked.allows_workspace_bind(),
            "the Workspaces dock is the only bound surface",
        );
    }

    /// D1: no matter what workspace id is offered to a Global panel — a dropdown
    /// pick, a picked folder, a restored pointer, or the value read at turn-send —
    /// it resolves to `None`. The turn-send read (`send_user_message`) routes
    /// through this exact call, so a global turn can never ride a workspace
    /// context. Docked passes the candidate through unchanged.
    #[test]
    fn global_scope_forces_workspace_id_to_none() {
        // Stored id (e.g. a stale/restored value) → None on the send read.
        assert_eq!(
            ChatScope::Global.resolve_workspace_id(Some("ws-abc".into())),
            None,
            "Global turn-send must carry no workspace_id",
        );
        // Already-absent stays absent.
        assert_eq!(ChatScope::Global.resolve_workspace_id(None), None);
        // Docked is untouched — its scope arrives via C3.
        assert_eq!(
            ChatScope::Docked.resolve_workspace_id(Some("ws-abc".into())),
            Some("ws-abc".to_owned()),
        );
        assert_eq!(ChatScope::Docked.resolve_workspace_id(None), None);
    }

    // ── C3: enter/leave → workspace-scope hook (the A1 live-bug fix) ──────
    // No gpui-executor harness exists here (same constraint as C1/C2), so the
    // live `apply_workspace_scope`/`spawn_workspace_scope_drain` entity path
    // can't be mounted. But the REAL cross-panel pipe CAN be driven: the
    // workspace-scope bus's channel + latch are per-test-binary OnceLocks, so
    // this binary owns a fresh receiver. The test below drives the genuine
    // producer→bus→consumer path end to end — `publish_active_workspace` (what
    // `enter_workspace`/`leave_workspace` call) → the real receiver/latch →
    // `ChatScope::resolve_workspace_id` (what `apply_workspace_scope` applies)
    // — proving the wiring the A1 bug lacked, minus only the gpui `notify`.

    /// C3 end-to-end over the real bus: an entered workspace published by the
    /// Workspaces panel arrives on the dock's receiver/latch and resolves to a
    /// bound id on the Docked surface (so the next dock turn rides it), while
    /// the same value forced through the Global surface stays `None` (D1).
    /// Leaving clears it back to unbound. This is the producer-without-consumer
    /// defect C3 fixes — `enter_workspace` formerly published nothing.
    #[test]
    fn workspace_scope_bus_rescopes_docked_dock() {
        // Enter: the Workspaces panel publishes the entered workspace exactly as
        // `enter_workspace` now does.
        wylde_gui_pipe::publish_active_workspace(Some("ws-alpha".to_owned()));

        // The dock seeds synchronously from the latch on mount (the path
        // `spawn_workspace_scope_drain` runs before draining the receiver).
        let seeded = wylde_gui_pipe::current_active_workspace();
        assert_eq!(
            ChatScope::Docked.resolve_workspace_id(seeded.clone()),
            Some("ws-alpha".to_owned()),
            "docked dock adopts the entered workspace → its turn carries the id",
        );
        // D1 holds on the same value: the Global slot can never become bound.
        assert_eq!(
            ChatScope::Global.resolve_workspace_id(seeded),
            None,
            "global Chat stays unbound even off the scope bus",
        );

        // The dock then drains the receiver; the buffered enter is delivered.
        let mut rx =
            wylde_gui_pipe::take_workspace_scope_receiver().expect("dock takes the receiver once");
        assert_eq!(
            ChatScope::Docked.resolve_workspace_id(rx.try_recv().expect("buffered enter")),
            Some("ws-alpha".to_owned()),
        );

        // Leave: `leave_workspace` publishes `None`; the dock clears to unbound.
        wylde_gui_pipe::publish_active_workspace(None);
        assert_eq!(
            ChatScope::Docked.resolve_workspace_id(rx.try_recv().expect("buffered leave")),
            None,
            "leaving a workspace clears the dock's scope",
        );
    }

    // ── C5: create-and-bind a new workspace conversation ─────────────────
    // No gpui-executor harness exists here (same constraint as C1–C3), so a full
    // `ChatPanel` can't be mounted to drive `spawn_new_conversation` end to end.
    // What the slice newly decides is *which workspace a freshly minted thread
    // binds to* — `new_conversation_bind_target`, which routes the live
    // `active_workspace_id` through `ChatScope::resolve_workspace_id`. That is the
    // exact value handed to `conversations.set_workspace` after `conversations.new`
    // returns; the verb sequence itself is exercised against the real flat store
    // by `wylde-harness`'s `new_then_set_workspace_binds_and_lists` action test.
    // Here we pin the binding decision the GUI feeds that sequence.

    /// C5: a Docked dock inside a workspace mints a *bound* thread (the entered
    /// id), while the Global slot mints an *unbound* one (`None`) no matter what
    /// sits in its field (D1), and a Docked dock with no workspace entered yet
    /// also mints unbound. This is the predicate `spawn_new_conversation` reads
    /// to decide whether to call `set_workspace_for_conversation` at all.
    #[test]
    fn new_conversation_binds_only_on_a_scoped_docked_dock() {
        // Docked + entered workspace → bind to it.
        assert_eq!(
            ChatScope::Docked.resolve_workspace_id(Some("ws-alpha".into())),
            Some("ws-alpha".to_owned()),
            "a new thread on the dock joins the entered workspace",
        );
        // Global + any id → never binds (D1 — structurally workspace-free).
        assert_eq!(
            ChatScope::Global.resolve_workspace_id(Some("ws-alpha".into())),
            None,
            "the Global slot mints unbound threads even with a stale id present",
        );
        // Docked with nothing entered yet → unbound thread.
        assert_eq!(ChatScope::Docked.resolve_workspace_id(None), None);
    }

    #[test]
    fn apply_token_appends_to_streaming_bubble() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::Token {
                turn_id: "t".into(),
                text: "hello ".into(),
            },
        );
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::Token {
                turn_id: "t".into(),
                text: "world".into(),
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "hello world");
        assert!(msg.streaming);
    }

    #[test]
    fn apply_turn_complete_flushes_streaming_flag() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::Token {
                turn_id: "t".into(),
                text: "partial".into(),
            },
        );
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::TurnComplete {
                turn_id: "t".into(),
                final_message: "ignored because we already streamed".into(),
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "partial");
        assert!(!msg.streaming);
    }

    #[test]
    fn apply_turn_complete_falls_back_to_final_message_when_empty() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::TurnComplete {
                turn_id: "t".into(),
                final_message: "from final".into(),
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "from final");
        assert!(!msg.streaming);
    }

    #[test]
    fn apply_thinking_accumulates_in_separate_buffer() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::Thinking {
                turn_id: "t".into(),
                text: "Let me think".into(),
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.thinking.as_deref(), Some("Let me think"));
        assert!(msg.content.is_empty());
    }

    #[test]
    fn processing_event_routing_drives_indicator_through_a_turn() {
        // Idle → animating → phases → tokens, fed off the user-facing
        // stream the same way the pump routes it (chat-processing-indicator).
        let mut p = ProcessingState::new();
        // Pre-signal: generic "Working" fallback (graceful degradation).
        assert_eq!(p.phase, ProcessingPhase::Working);

        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Phase {
                turn_id: "t".into(),
                phase: "gathering_context".into(),
            },
        );
        assert_eq!(p.phase, ProcessingPhase::GatheringContext);

        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Phase {
                turn_id: "t".into(),
                phase: "generating".into(),
            },
        );
        assert_eq!(p.phase, ProcessingPhase::Generating);

        // Running usage tick, then thinking.
        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Usage {
                turn_id: "t".into(),
                prompt_tokens: None,
                completion_tokens: 120,
                done: false,
            },
        );
        assert_eq!(p.token_meter(), Some("120 tokens".to_owned()));
        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Thinking {
                turn_id: "t".into(),
                text: "hmm".into(),
            },
        );
        assert_eq!(p.thinking, "hmm");

        // Authoritative final usage.
        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Usage {
                turn_id: "t".into(),
                prompt_tokens: Some(1_000),
                completion_tokens: 200,
                done: true,
            },
        );
        assert_eq!(p.token_meter(), Some("1.2k tokens".to_owned()));
    }

    #[test]
    fn tool_detail_formats_args_output_and_duration() {
        // Object args → compact JSON.
        let d = tool_detail(&serde_json::json!({"query": "vpn"}), None);
        assert_eq!(d.as_deref(), Some("{\"query\":\"vpn\"}"));
        // String value (an error) used verbatim + duration appended.
        let d = tool_detail(&serde_json::Value::String("boom".into()), Some(12.0));
        assert_eq!(d.as_deref(), Some("boom  ·  12ms"));
        // Null + no duration → nothing.
        assert_eq!(tool_detail(&serde_json::Value::Null, None), None);
        // Null + duration → just the duration.
        assert_eq!(
            tool_detail(&serde_json::Value::Null, Some(5.0)).as_deref(),
            Some("5ms")
        );
    }

    #[test]
    fn processing_event_routing_handles_steps() {
        let mut p = ProcessingState::new();
        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Step {
                turn_id: "t".into(),
                stage: "routing".into(),
                summary: "Routed to 2 concepts".into(),
                detail: Some("nextcloud, vpn".into()),
            },
        );
        let e = p
            .log
            .iter()
            .find(|e| e.text == "Routed to 2 concepts")
            .unwrap();
        assert_eq!(e.detail.as_deref(), Some("nextcloud, vpn"));
    }

    #[test]
    fn processing_event_routing_is_a_noop_without_a_state() {
        // Graceful degradation: a chunk arriving with no in-flight indicator
        // (e.g. a late frame after settle) must not panic.
        apply_processing_turn_event(
            None,
            &TurnChunk::Phase {
                turn_id: "t".into(),
                phase: "generating".into(),
            },
        );
        // Bubble-only variants are ignored by the processing router.
        let mut p = ProcessingState::new();
        apply_processing_turn_event(
            Some(&mut p),
            &TurnChunk::Token {
                turn_id: "t".into(),
                text: "hi".into(),
            },
        );
        assert!(p.log.is_empty());
        assert_eq!(p.token_meter(), None);
    }

    #[test]
    fn apply_turn_aborted_surfaces_reason_when_not_cancelled() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::TurnAborted {
                turn_id: "t".into(),
                reason: "error".into(),
                error: Some("model crashed".into()),
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert!(msg.content.contains("error"));
        assert!(msg.content.contains("model crashed"));
        assert!(!msg.streaming);
    }

    #[test]
    fn apply_turn_aborted_cancelled_leaves_empty_bubble() {
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        apply_turn_chunk(
            &mut msgs,
            &aid,
            &TurnChunk::TurnAborted {
                turn_id: "t".into(),
                reason: "cancelled".into(),
                error: None,
            },
        );
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert!(msg.content.is_empty());
        assert!(!msg.streaming);
    }

    #[test]
    fn flush_streaming_bubble_substitutes_fallback_when_empty() {
        // Unexpected stream close on an empty bubble → fallback shown,
        // streaming cleared (no perpetual typing indicator).
        let a = ChatMessage::assistant_streaming();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        flush_streaming_bubble(&mut msgs, &aid, "[stream ended]");
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "[stream ended]");
        assert!(!msg.streaming);
    }

    #[test]
    fn flush_streaming_bubble_keeps_partial_content() {
        // A bubble that already streamed text keeps it; only the flag
        // clears, so the fallback never clobbers real tokens.
        let mut a = ChatMessage::assistant_streaming();
        a.content = "partial answer".to_owned();
        let aid = a.id.clone();
        let mut msgs = vec![a];
        flush_streaming_bubble(&mut msgs, &aid, "[stream ended]");
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "partial answer");
        assert!(!msg.streaming);
    }

    #[test]
    fn flush_streaming_bubble_is_noop_on_settled_bubble() {
        // Already-flushed bubble (streaming == false) is untouched, so a
        // late recovery pass can't overwrite a completed turn.
        let mut a = ChatMessage::assistant_streaming();
        a.content = "done".to_owned();
        a.streaming = false;
        let aid = a.id.clone();
        let mut msgs = vec![a];
        flush_streaming_bubble(&mut msgs, &aid, "[stream ended]");
        let msg = msgs.iter().find(|m| m.id == aid).unwrap();
        assert_eq!(msg.content, "done");
        assert!(!msg.streaming);
    }

    #[test]
    fn consent_event_pending_inserts_into_map() {
        let mut map: BTreeMap<String, PendingConsent> = BTreeMap::new();
        let pending = PendingConsent {
            id: "p1".into(),
            tool: "fs.write_file".into(),
            summary: "write file.md".into(),
            default_action: "deny".into(),
        };
        map.insert(pending.id.clone(), pending);
        assert_eq!(map.len(), 1);
        map.remove("p1");
        assert!(map.is_empty());
    }

    #[test]
    fn consent_event_value_round_trip() {
        let wire = json!({
            "type": "pending",
            "id": "p1",
            "tool": "fs.write_file",
            "summary": "write README.md",
            "default_action": "deny",
            "awaiting_since": 1_700_000_000_i64,
        });
        match ConsentEvent::from_value(&wire) {
            Some(ConsentEvent::Pending(p)) => {
                assert_eq!(p.id, "p1");
                assert_eq!(p.tool, "fs.write_file");
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn message_role_user_renders_distinct_id_from_assistant() {
        let u = ChatMessage::user("hi".into());
        let a = ChatMessage::assistant_streaming();
        assert_ne!(u.id, a.id);
        assert_eq!(u.role, MessageRole::User);
        assert_eq!(a.role, MessageRole::Assistant);
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn tool_chunk_parses_dispatched() {
        let v = json!({
            "type": "tool_dispatched",
            "turn_id": "t",
            "call_id": "c1",
            "name": "memory.long_term.search",
            "args": {}
        });
        match ToolChunk::from_value(&v) {
            ToolChunk::Dispatched { name, .. } => {
                assert_eq!(name, "memory.long_term.search");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_chunk_parses_result() {
        let v = json!({
            "type": "tool_result",
            "turn_id": "t",
            "call_id": "c1",
            "name": "memory.long_term.search",
            "output": [],
            "duration_ms": 5.0,
        });
        assert!(matches!(
            ToolChunk::from_value(&v),
            ToolChunk::Result { .. }
        ));
    }

    #[test]
    fn tool_chunk_unknown_type_does_not_panic() {
        let v = json!({"type": "future_event"});
        assert!(matches!(ToolChunk::from_value(&v), ToolChunk::Unknown));
    }

    // ── Conversation switcher (Memory Slice B) ───────────────────────

    fn meta(id: &str, updated_at: i64) -> ConversationMeta {
        ConversationMeta {
            id: id.to_owned(),
            title: format!("Chat {id}"),
            created_at: updated_at,
            updated_at,
            message_count: 1,
            working_memory_count: 0,
            model: String::new(),
            workspace_id: String::new(),
        }
    }

    #[test]
    fn pick_next_active_takes_newest_remaining() {
        // List is newest-first (the harness sorts by updated_at desc), and
        // the deleted entry has already been removed, so the head is the
        // closest survivor to fall back to.
        let remaining = vec![meta("b", 300), meta("c", 100)];
        assert_eq!(
            pick_next_active(&remaining, "a").as_deref(),
            Some("b"),
            "newest remaining wins",
        );
    }

    #[test]
    fn pick_next_active_none_when_empty() {
        assert_eq!(pick_next_active(&[], "only"), None);
    }

    // ── C6: empty-state / default conversation on entering ───────────────
    // The enter flow can't be mounted (no gpui executor here, same constraint
    // as C1–C5), so the three enter cases are pinned on the pure selector the
    // flow delegates to. The live switch/mint/bind plumbing is exercised by the
    // production path (enter → send → thread in the scoped list), flagged owed.

    #[test]
    fn pick_enter_has_last_open_restores_the_pointer() {
        // The per-workspace pointer (C7) names a thread still in the list — it
        // wins over the most-recent fallback even when it isn't the newest.
        let scoped = vec![meta("newest", 300), meta("pinned", 100)];
        assert_eq!(
            pick_enter_conversation(Some("pinned"), &scoped),
            EnterTarget::Open("pinned".to_owned()),
            "a live last-open pointer is restored, not the most-recent",
        );
    }

    #[test]
    fn pick_enter_has_threads_opens_most_recent() {
        // No usable pointer → open the most-recent, i.e. the head of the
        // newest-first scoped list.
        let scoped = vec![meta("newest", 300), meta("older", 100)];
        assert_eq!(
            pick_enter_conversation(None, &scoped),
            EnterTarget::Open("newest".to_owned()),
            "no pointer → most-recent scoped thread",
        );
    }

    #[test]
    fn pick_enter_none_is_empty_state() {
        // A fresh workspace with no bound threads → the empty state; the flow
        // then mints a fileless thread and defers its bind to the first send.
        assert_eq!(
            pick_enter_conversation(None, &[]),
            EnterTarget::Empty,
            "no pointer + no threads → empty state",
        );
    }

    #[test]
    fn pick_enter_stale_pointer_falls_through() {
        // A pointer whose thread is gone (deleted / re-bound) must not strand
        // the dock on a missing id — it falls through to most-recent...
        let scoped = vec![meta("survivor", 200)];
        assert_eq!(
            pick_enter_conversation(Some("deleted"), &scoped),
            EnterTarget::Open("survivor".to_owned()),
            "a stale pointer falls through to most-recent",
        );
        // ...and to the empty state when nothing remains.
        assert_eq!(
            pick_enter_conversation(Some("deleted"), &[]),
            EnterTarget::Empty,
            "a stale pointer with an empty list falls through to empty",
        );
    }

    #[test]
    fn loaded_message_maps_role_and_clears_streaming() {
        let u = ChatMessage::loaded("user", "hello".into());
        assert_eq!(u.role, MessageRole::User);
        assert!(!u.streaming);
        assert_eq!(u.content, "hello");

        // Anything that isn't `user` renders as the assistant (system
        // messages never survive a save).
        let a = ChatMessage::loaded("assistant", "hi".into());
        assert_eq!(a.role, MessageRole::Assistant);
        let other = ChatMessage::loaded("tool", "x".into());
        assert_eq!(other.role, MessageRole::Assistant);
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(0), "—", "unset stamp");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_time(now - 5), "just now");
        assert_eq!(relative_time(now - 120), "2m");
        assert_eq!(relative_time(now - 7200), "2h");
        assert_eq!(relative_time(now - 172_800), "2d");
        // A stamp slightly in the future (clock skew) is "just now", not a
        // negative age.
        assert_eq!(relative_time(now + 100), "just now");
    }

    // ── InferenceBar thinking-tier pill ──────────────────────────────────
    // No gpui-executor harness in this crate (see the note above the C1
    // tests), so the pill is pinned at the state + label level: the field
    // the pill reads/writes and the exact string the pill renders.

    /// The confirmed default is Fast — never planning-by-default (the
    /// reasoning tax). This is the identity-invariant end of the ladder.
    #[test]
    fn reasoning_depth_defaults_to_fast() {
        assert_eq!(ReasoningDepth::default(), ReasoningDepth::Fast);
        assert_eq!(ReasoningDepth::Fast.as_str(), "fast");
    }

    /// The wire tokens the harness's `Depth::parse` accepts, one per tier.
    #[test]
    fn reasoning_depth_wire_tokens_match_harness() {
        assert_eq!(ReasoningDepth::Think.as_str(), "think");
        assert_eq!(ReasoningDepth::ThinkHarder.as_str(), "think_harder");
        assert_eq!(ReasoningDepth::Ultrathink.as_str(), "ultrathink");
    }

    /// Clicking cycles the full tier ladder and wraps back to Fast (the
    /// pill's whole contract).
    #[test]
    fn reasoning_depth_cycles_the_tier_ladder() {
        let d = ReasoningDepth::Fast;
        assert_eq!(d.toggled(), ReasoningDepth::Think);
        assert_eq!(d.toggled().toggled(), ReasoningDepth::ThinkHarder);
        assert_eq!(d.toggled().toggled().toggled(), ReasoningDepth::Ultrathink);
        assert_eq!(
            d.toggled().toggled().toggled().toggled(),
            ReasoningDepth::Fast,
            "wraps"
        );
    }

    /// The pill label the InferenceBar renders per tier, defaulting Fast.
    #[test]
    fn reasoning_depth_pill_label_matches_state() {
        assert_eq!(
            format!("reasoning · {}", ReasoningDepth::default().label()),
            "reasoning · fast"
        );
        assert_eq!(
            format!("reasoning · {}", ReasoningDepth::ThinkHarder.label()),
            "reasoning · think harder"
        );
        assert_eq!(
            format!("reasoning · {}", ReasoningDepth::Ultrathink.label()),
            "reasoning · ultrathink"
        );
    }

    #[test]
    fn reason_mode_defaults_to_single() {
        // The maintainer 2026-07-13: PLAN and EXECUTE on the same model ⇒ Single.
        assert_eq!(ReasonMode::default(), ReasonMode::Single);
        assert_eq!(ReasonMode::Single.as_str(), "single");
        assert_eq!(ReasonMode::Split.as_str(), "split");
    }

    #[test]
    fn reason_mode_toggles_both_ways() {
        let m = ReasonMode::Single;
        assert_eq!(m.toggled(), ReasonMode::Split);
        assert_eq!(m.toggled().toggled(), ReasonMode::Single);
    }

    #[test]
    fn reason_mode_parse_is_tolerant() {
        assert_eq!(ReasonMode::parse("split"), Some(ReasonMode::Split));
        assert_eq!(ReasonMode::parse("single"), Some(ReasonMode::Single));
        assert_eq!(
            ReasonMode::parse("sideways"),
            None,
            "unknown → caller default"
        );
    }

    #[test]
    fn reason_mode_pill_label_matches_state() {
        let single = format!("mode · {}", ReasonMode::default().as_str());
        assert_eq!(single, "mode · single");
        let split = format!("mode · {}", ReasonMode::Split.as_str());
        assert_eq!(split, "mode · split");
    }
}
