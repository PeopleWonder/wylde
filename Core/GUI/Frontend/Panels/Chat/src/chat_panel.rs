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
//!   * `tool_activity`          — current in-flight `chat.stream_tools`
//!     event the activity strip renders.  Disjoint from the bubble log
//!     (the `wylde_inference_bar_scope` rule says tool calls don't
//!     become bubbles).
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FocusHandle, Focusable, FontWeight, IntoElement, Render, SharedString, Stateful, Subscription,
    Window,
};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_700, SURFACE_800,
    SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    activate_workspace, cancel_turn, clear_working_memory, delete_conversation, eject_model,
    fetch_conversation_messages, fetch_working_memory, get_active_conversation, list_conversations,
    list_models, new_conversation, recent_workspaces, respond_consent, set_active_conversation,
    set_active_model, start_turn_with_model, stream_consent_pending, stream_tools, stream_turn,
    ConsentEvent,
    ConversationMeta, PendingConsent, ToolChunk, TurnChunk, WorkingMemoryEntry, WorkspaceSummary,
};
use crate::markdown;

const WORKSPACE_MRU_LIMIT: u32 = 5;

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
        }
    }

    fn assistant_streaming() -> Self {
        Self {
            id: new_message_id(),
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            streaming: true,
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
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// One active tool call surfaced from `chat.stream_tools`.  The strip
/// renders the most recently dispatched name; tool_result / tool_error
/// for the same call_id clears it.
#[derive(Debug, Clone)]
pub struct ToolActivity {
    pub call_id: String,
    pub name: String,
    pub since: Instant,
}

/// Root Chat panel.
pub struct ChatPanel {
    pub focus_handle: FocusHandle,
    pub messages: Vec<ChatMessage>,
    pub active_turn_id: Option<String>,
    pub conversation_id: String,
    /// Conversation switcher (Memory Slice B): the saved-chat list for the
    /// rail, whether the rail is open, and — when the user clicks a row's
    /// delete affordance — the id awaiting an inline delete confirmation.
    pub conversations: Vec<ConversationMeta>,
    pub show_conversations: bool,
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
    pub show_ws_dropdown: bool,
    pub models: Vec<String>,
    pub active_model: Option<String>,
    pub show_model_dropdown: bool,
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
    pub tool_activity: Option<ToolActivity>,
    pub prompt_input: Entity<TextInput>,
    /// Held to keep the input → panel subscription alive for the
    /// lifetime of the panel.
    _input_sub: Subscription,
}

impl ChatPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let prompt_input = cx.new(|input_cx| {
            TextInput::multi_line(input_cx)
                .with_placeholder("Send a message  ·  Enter to send, Shift+Enter for newline")
                .with_submit_mode(SubmitMode::EnterSubmits)
                .with_min_height(60.0)
                .with_max_height(180.0)
                .with_element_key("chat-prompt")
        });
        let prompt_input_for_submit = prompt_input.clone();
        let input_sub = cx.subscribe(
            &prompt_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Submit(text) = event {
                    this.submit_text(text.clone(), &prompt_input_for_submit, cx);
                }
            },
        );

        Self {
            focus_handle: cx.focus_handle(),
            messages: Vec::new(),
            active_turn_id: None,
            conversation_id: "default".to_owned(),
            conversations: Vec::new(),
            show_conversations: false,
            confirm_delete: None,
            working_memory: Vec::new(),
            show_working_memory: false,
            workspaces: Vec::new(),
            active_workspace_id: None,
            show_ws_dropdown: false,
            models: Vec::new(),
            active_model: None,
            show_model_dropdown: false,
            ejecting: false,
            pending_consents: BTreeMap::new(),
            error: None,
            starting: false,
            active_stream: None,
            active_tool_stream: None,
            consent_stream: None,
            tool_activity: None,
            prompt_input,
            _input_sub: input_sub,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            // Announce the conversation this panel owns on the cross-panel
            // bus so a sibling that's already mounted (e.g. the Memory
            // panel's short-term view) reflects it immediately.  Until a
            // turn adopts a harness-minted id this is the "default"
            // conversation the working-memory pill already queries.
            wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
            Self::spawn_load_workspaces(cx);
            Self::spawn_load_models(cx);
            // Restore the persisted active conversation (Slice B), then load
            // its working-memory buffer + the switcher list — sequenced in
            // one task so the WM load reads the *restored* id, not "default".
            Self::spawn_restore_session(cx);
            Self::spawn_consent_subscription(cx);
            panel
        })
        .into()
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

    /// Re-fetch the saved-chat list and replace the rail's copy. Soft-fail
    /// (a transport error leaves the existing list untouched).
    async fn reload_conversations(this: &gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        if let Ok(rows) = list_conversations().await {
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
            wylde_gui_pipe::publish_active_conversation(&self.conversation_id);
            // Optimistically clear the old conversation's view; the reloads
            // below reconcile against the harness.
            self.messages.clear();
            self.working_memory.clear();
        }
        self.show_conversations = false;
        self.confirm_delete = None;
        self.focus_prompt(window, cx);
        cx.notify();
        if !switched {
            return;
        }
        let cid = self.conversation_id.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = set_active_conversation(&cid).await;
            Self::reload_conversation_messages(&this, app_cx).await;
            Self::reload_working_memory(&this, app_cx).await;
        })
        .detach();
    }

    /// "+ New": mint a fresh conversation id, switch to it (blank log),
    /// persist it as active, and refresh the rail + cross-panel bus.
    pub fn spawn_new_conversation(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let Ok(id) = new_conversation().await else {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some("Couldn't start a new conversation".to_owned());
                    cx.notify();
                });
                return;
            };
            let _ = this.update(app_cx, |panel, cx| {
                panel.conversation_id = id.clone();
                panel.messages.clear();
                panel.working_memory.clear();
                panel.show_conversations = false;
                panel.confirm_delete = None;
                wylde_gui_pipe::publish_active_conversation(&panel.conversation_id);
                cx.notify();
            });
            let _ = set_active_conversation(&id).await;
            // A brand-new conversation has no file until its first turn, so
            // it won't appear in the list yet — but announce the list change
            // for forward-compat and refresh our own copy.
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
            let _ = delete_conversation(&id).await;
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

    fn submit_text(
        &mut self,
        text: String,
        input: &Entity<TextInput>,
        cx: &mut Context<Self>,
    ) {
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

    pub fn send_user_message(&mut self, text: String, cx: &mut Context<Self>) {
        self.error = None;
        self.messages.push(ChatMessage::user(text.clone()));
        let assistant = ChatMessage::assistant_streaming();
        let assistant_id = assistant.id.clone();
        self.messages.push(assistant);
        // Latch synchronously — see the `starting` field doc.  Cleared
        // the moment `active_turn_id` is published (or on any failure).
        self.starting = true;
        cx.notify();

        let conversation_id = self.conversation_id.clone();
        let workspace_id = self.active_workspace_id.clone();
        let model = self.active_model.clone();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let start = start_turn_with_model(
                &text,
                &conversation_id,
                workspace_id.as_deref(),
                model.as_deref(),
            )
            .await;
            let (turn_id, reply_conversation_id) = match start {
                Ok(r) => (r.turn_id, r.conversation_id),
                Err(e) => {
                    let _ = this.update(app_cx, |panel, cx| {
                        let msg = format!("[Failed to start turn: {e}]");
                        if let Some(last) = panel
                            .messages
                            .iter_mut()
                            .find(|m| m.id == assistant_id)
                        {
                            last.content = msg.clone();
                            last.streaming = false;
                        }
                        panel.active_turn_id = None;
                        panel.starting = false;
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
                        if let Some(m) = panel
                            .messages
                            .iter_mut()
                            .find(|m| m.id == assistant_id)
                        {
                            m.content = format!("[stream error: {e}]");
                            m.streaming = false;
                        }
                        panel.active_turn_id = None;
                        panel.starting = false;
                        panel.active_tool_stream = None;
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
                                if panel.active_turn_id.as_deref()
                                    != Some(turn_id.as_str())
                                {
                                    done = true;
                                    return;
                                }
                                // A user-facing token clears any stale tool
                                // activity strip — the assistant is talking.
                                if matches!(event, TurnChunk::Token { .. }) {
                                    panel.tool_activity = None;
                                }
                                apply_turn_chunk(&mut panel.messages, &assistant_id, &event);
                                if matches!(
                                    event,
                                    TurnChunk::TurnComplete { .. }
                                        | TurnChunk::TurnAborted { .. }
                                ) {
                                    done = true;
                                    panel.active_turn_id = None;
                                    panel.active_stream = None;
                                    panel.active_tool_stream = None;
                                    panel.tool_activity = None;
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
                            panel.active_turn_id = None;
                            panel.active_stream = None;
                            panel.active_tool_stream = None;
                            panel.tool_activity = None;
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
                    panel.active_turn_id = None;
                    panel.active_stream = None;
                    panel.active_tool_stream = None;
                    panel.tool_activity = None;
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

    /// Pump events off the active tool stream into `tool_activity`.
    /// Runs until the stream ends (turn complete, drop, or transport
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
                    let next = match this.update(app_cx, |panel, _| {
                        panel.active_tool_stream.take()
                    }) {
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
                        let _ = this.update(app_cx, |panel, cx| {
                            panel.active_tool_stream = None;
                            panel.tool_activity = None;
                            cx.notify();
                        });
                        return;
                    };
                    let value = match chunk {
                        Ok(v) => v,
                        Err(_) => {
                            // Transport hiccup; let it ride.  The
                            // user-facing stream will surface a louder
                            // error if the whole turn died.
                            let _ = this.update(app_cx, |panel, cx| {
                                panel.tool_activity = None;
                                cx.notify();
                            });
                            return;
                        }
                    };
                    let event = ToolChunk::from_value(&value);
                    let _ = this.update(app_cx, |panel, cx| {
                        match event {
                            ToolChunk::Dispatched {
                                call_id, name, ..
                            } => {
                                panel.tool_activity = Some(ToolActivity {
                                    call_id,
                                    name,
                                    since: Instant::now(),
                                });
                            }
                            ToolChunk::Result { call_id, .. }
                            | ToolChunk::Error { call_id, .. } => {
                                if panel
                                    .tool_activity
                                    .as_ref()
                                    .map(|a| a.call_id == call_id)
                                    .unwrap_or(false)
                                {
                                    panel.tool_activity = None;
                                }
                            }
                            ToolChunk::MemoryWritten
                            | ToolChunk::Warning
                            | ToolChunk::Unknown => {}
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
        self.tool_activity = None;
        // Abandon any consent cards belonging to this turn.  Only one
        // turn is ever in flight (sends are blocked while active), so
        // every pending card belongs to the turn being cancelled.
        self.pending_consents.clear();
        // Flush the in-flight assistant bubble's streaming flag.
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
        self.active_workspace_id = Some(id.to_owned());
        self.show_ws_dropdown = false;
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

    pub fn spawn_pick_workspace(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Runs on gpui's executor (no tokio reactor) — `tokio::task::
            // spawn_blocking` would panic. Hop onto the bridge runtime.
            let picked: Option<PathBuf> =
                wylde_gui_pipe::bridged_spawn_blocking(pick_folder).await;
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
                    panel.active_workspace_id = Some(ws.id.clone());
                }
            });
            let mru = recent_workspaces(WORKSPACE_MRU_LIMIT).await.unwrap_or_default();
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

/// Apply a single `chat.stream_turn` chunk to the assistant bubble.
fn apply_turn_chunk(
    messages: &mut [ChatMessage],
    assistant_id: &str,
    event: &TurnChunk,
) {
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

impl Render for ChatPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let inference_bar = inference_bar(self, cx);
        let log = message_log(self, cx);
        let consent_strip = consent_card_strip(self, cx);
        let tool_strip = tool_activity_strip(self);

        let mut body = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(pack(SURFACE_900)))
            .child(log)
            .child(consent_strip)
            .child(tool_strip);

        if let Some(err) = &self.error {
            body = body.child(error_strip(err));
        }
        body.child(inference_bar)
    }
}

fn message_log(panel: &ChatPanel, _cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let mut log = div()
        .id(ElementId::Name("chat-log".into()))
        .flex_1()
        .flex()
        .flex_col()
        .gap_3()
        .p_5()
        .overflow_y_scroll();

    if panel.messages.is_empty() {
        log = log.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("How can I help?")),
        );
        return log;
    }

    for m in &panel.messages {
        log = log.child(bubble(m));
    }
    log
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

    // While streaming and still empty, show a typing indicator.  Once
    // the first token arrives we switch to markdown rendering so the
    // partial reply already starts formatting.
    if m.streaming && m.content.is_empty() {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("…")),
        );
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
    div()
        .id(id)
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

fn tool_activity_strip(panel: &ChatPanel) -> gpui::Div {
    let mut strip = div().flex().flex_col();
    if let Some(activity) = &panel.tool_activity {
        let label = SharedString::from(format!("Wylde is consulting {}…", activity.name));
        strip = strip.px_5().pb_2().child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .px_3()
                .py_1()
                .rounded(px(12.0))
                .bg(rgb(pack(SURFACE_800)))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from("·"))
                .child(label),
        );
    }
    strip
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
    if panel.show_ws_dropdown {
        bar = bar.child(workspace_dropdown(panel, cx));
    }
    if panel.show_model_dropdown {
        bar = bar.child(model_dropdown(panel, cx));
    }
    if panel.show_working_memory {
        bar = bar.child(working_memory_panel(panel, cx));
    }

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
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(conversations_pill(panel, cx))
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
        ))
        .child(pill_button(
            ElementId::Name("chat-model-toggle".into()),
            model_label,
            cx.listener(|this: &mut ChatPanel, _ev, _window, cx| {
                this.toggle_model_dropdown(cx);
            }),
        ))
        .child(eject_button(panel, cx))
        .child(working_memory_pill(panel, cx))
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
            div()
                .id(ElementId::Name("chat-wm-clear".into()))
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
                .id(ElementId::Name("chat-conversation-new".into()))
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
        );
    col = col.child(header);

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
    let select_block = div()
        .id(ElementId::Name(format!("chat-conversation-pick::{}", meta.id).into()))
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

    if confirming {
        row = row.child(delete_confirm_controls(&meta.id, cx));
    } else {
        row = row.child(delete_request_button(&meta.id, cx));
    }
    row
}

/// The "×" affordance that arms the inline delete confirm for `id`.
fn delete_request_button(id: &str, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let id_for_click = id.to_owned();
    div()
        .id(ElementId::Name(format!("chat-conversation-del::{id}").into()))
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
            div()
                .id(ElementId::Name(format!("chat-conversation-del-yes::{id}").into()))
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
            div()
                .id(ElementId::Name(format!("chat-conversation-del-no::{id}").into()))
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
    let mut btn = div()
        .id(ElementId::Name("chat-model-eject".into()))
        .px_3()
        .py_1()
        .rounded(px(12.0))
        .border_1()
        .border_color(border_color)
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(text_color)
        // ⏏ eject glyph; trailing "…" while the round-trip is in flight.
        .child(SharedString::from(if panel.ejecting { "⏏ …" } else { "⏏" }));
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
    div()
        .id(id)
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
        col = col.child(
            dropdown_row(
                ElementId::Name(format!("chat-ws-pick::{}", ws.id).into()),
                SharedString::from(format!("{}  ·  {}", ws.id, ws.path)),
                cx.listener(move |this: &mut ChatPanel, _ev, window, cx| {
                    this.select_workspace(&id_for_select, window, cx);
                }),
            ),
        );
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
        col = col.child(empty_dropdown_row(
            "Ollama offline — no models discovered",
        ));
        return col;
    }
    for m in &panel.models {
        let m_for_select = m.clone();
        col = col.child(
            dropdown_row(
                ElementId::Name(format!("chat-model-pick::{m}").into()),
                SharedString::from(m.clone()),
                cx.listener(move |this: &mut ChatPanel, _ev, window, cx| {
                    this.select_model(Some(m_for_select.clone()), window, cx);
                }),
            ),
        );
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
    div()
        .id(id)
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

    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_end()
        .child(div().flex_1().child(panel.prompt_input.clone()))
        .child(button)
}

fn send_button(input: Entity<TextInput>, cx: &mut Context<ChatPanel>) -> Stateful<gpui::Div> {
    let listener_input = input.clone();
    div()
        .id(ElementId::Name("chat-send".into()))
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
    div()
        .id(ElementId::Name("chat-stop".into()))
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
}
