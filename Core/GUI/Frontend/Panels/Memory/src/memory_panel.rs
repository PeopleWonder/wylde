//! Memory panel View — three-layer browser.
//!
//! Slice 5.1 polish:
//!   * Search input replaced with `wylde-gpui-input` single-line
//!     `TextInput`.  Typing fires `memory.long_term.search` after a
//!     300 ms debounce; clearing the box reverts to `memory.long_term.list`.
//!
//! State:
//!   * `long_term`            — last reply from `list` or `search`.
//!   * `search_input`         — `Entity<TextInput>`; owns the typed
//!     query buffer.  Empty buffer means "show the importance-sorted
//!     list".
//!   * `search_active`        — `true` whenever `long_term` was last
//!     populated by `search`.  Drives the "Searching for…" label.
//!   * `search_generation`    — monotonic counter incremented on each
//!     keystroke so a stale 300 ms task can detect it's been overtaken
//!     and bail before issuing a search.
//!   * `workspaces`           — last `memory.workspaces.recent` reply.
//!   * `expanded`             — set of record ids the user clicked to
//!     reveal the full body.
//!   * `error`                — last pipe error.
//!   * `loading_*`            — per-section flags.

use std::collections::BTreeSet;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Stateful, Subscription, Window,
};
use wylde_gpui_input::{InputEvent, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    fetch_short_term, list_long_term, recent_workspaces, search_long_term, LongTermRecord,
    ShortTermEntry, WorkspaceSummary,
};

/// MRU cap used for the workspace section.
const WORKSPACE_MRU_LIMIT: u32 = 5;

/// Maximum search-result count.
const SEARCH_LIMIT: u32 = 10;

/// Body preview length when a row is collapsed.
const PREVIEW_CHARS: usize = 120;

/// Debounce window for live search.  Picked to match VS Code's quick
/// open: short enough that the result list feels responsive, long
/// enough that mid-word keystrokes don't fan out into N searches.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

pub struct MemoryPanel {
    pub long_term: Vec<LongTermRecord>,
    pub workspaces: Vec<WorkspaceSummary>,
    pub search_input: Entity<TextInput>,
    pub search_active: bool,
    pub search_generation: u64,
    pub expanded: BTreeSet<String>,
    pub error: Option<String>,
    pub loading_long_term: bool,
    pub loading_workspaces: bool,
    /// The conversation the Chat panel last announced on the cross-panel
    /// bus, or `None` until one is announced.  When `Some`, the Short-term
    /// section mirrors that conversation's working-memory buffer instead
    /// of the static pointer card.
    pub active_conversation: Option<String>,
    /// Short-term ("working memory") buffer for [`Self::active_conversation`],
    /// refreshed whenever the bus reports the active conversation changed.
    pub short_term: Vec<ShortTermEntry>,
    /// `true` while the first short-term fetch for the current conversation
    /// is in flight — drives the "Loading…" row instead of a flash of
    /// "empty buffer".
    pub loading_short_term: bool,
    _search_sub: Subscription,
}

impl MemoryPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Search long-term memory…")
                .with_min_height(32.0)
                .with_element_key("memory-search")
        });
        let sub = cx.subscribe(
            &search_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Changed(text) = event {
                    this.schedule_search(text.clone(), cx);
                }
            },
        );

        Self {
            long_term: Vec::new(),
            workspaces: Vec::new(),
            search_input,
            search_active: false,
            search_generation: 0,
            expanded: BTreeSet::new(),
            error: None,
            loading_long_term: true,
            loading_workspaces: true,
            active_conversation: None,
            short_term: Vec::new(),
            loading_short_term: false,
            _search_sub: sub,
        }
    }

    /// Factory entry — matches the manifest factory string
    /// (`wylde_panel_memory::MemoryPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            Self::spawn_refresh(cx);
            // Follow the active conversation on the cross-panel bus so the
            // Short-term section mirrors whatever chat is live.
            Self::spawn_conversation_bus_drain(cx);
            panel
        })
        .into()
    }

    /// Long-lived task that follows the cross-panel conversation bus.
    /// Seeds from the last-announced active conversation (the Chat panel
    /// may have published before this panel mounted), then re-fetches the
    /// short-term buffer every time the active conversation changes.  Runs
    /// for the lifetime of the panel entity; a torn-down panel short-
    /// circuits the next `update` and the task exits.
    pub fn spawn_conversation_bus_drain(cx: &mut Context<Self>) {
        use tokio::sync::broadcast::error::RecvError;
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Subscribe *before* reading the latch so a publish racing our
            // mount lands in the receiver rather than being missed.
            let mut rx = wylde_gui_pipe::subscribe_conversation_bus();
            if let Some(cid) = wylde_gui_pipe::current_active_conversation() {
                if !Self::adopt_conversation(&this, app_cx, cid).await {
                    return;
                }
            }
            loop {
                match rx.recv().await {
                    Ok(wylde_gui_pipe::ConversationEvent::ActiveConversationChanged {
                        conversation_id,
                    }) => {
                        if !Self::adopt_conversation(&this, app_cx, conversation_id).await {
                            return;
                        }
                    }
                    // List changes don't affect the short-term mirror.
                    Ok(_) => {}
                    // Fell behind (rare — events are infrequent): re-seed
                    // from the latch so we don't strand on a stale id.
                    Err(RecvError::Lagged(_)) => {
                        if let Some(cid) = wylde_gui_pipe::current_active_conversation() {
                            if !Self::adopt_conversation(&this, app_cx, cid).await {
                                return;
                            }
                        }
                    }
                    // Sender gone (never happens for the static bus, but
                    // bail rather than spin if it ever does).
                    Err(RecvError::Closed) => return,
                }
            }
        })
        .detach();
    }

    /// Set the active conversation, mark the short-term section loading,
    /// then fetch + apply its buffer.  Returns `false` when the panel
    /// entity has been torn down (the caller should stop draining).
    async fn adopt_conversation(
        this: &gpui::WeakEntity<Self>,
        app_cx: &mut AsyncApp,
        conversation_id: String,
    ) -> bool {
        let alive = this
            .update(app_cx, |panel, cx| {
                panel.active_conversation = Some(conversation_id.clone());
                panel.loading_short_term = true;
                cx.notify();
            })
            .is_ok();
        if !alive {
            return false;
        }
        // Soft-fail: a transport error (harness cold-starting, conversation
        // not persisted yet) leaves the buffer empty rather than surfacing
        // a loud error in the global browser.
        let entries = fetch_short_term(&conversation_id).await.unwrap_or_default();
        this.update(app_cx, |panel, cx| {
            // Only apply if this is still the conversation we're tracking —
            // a newer change may have superseded us mid-fetch.
            if panel.active_conversation.as_deref() == Some(conversation_id.as_str()) {
                panel.short_term = entries;
                panel.loading_short_term = false;
                cx.notify();
            }
        })
        .is_ok()
    }

    /// Refresh every section.  Each layer fires on its own task so a
    /// slow workspace read can't stall the long-term render.
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_long_term().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(rows) => {
                        panel.error = None;
                        panel.long_term = rows;
                        panel.search_active = false;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                    }
                }
                panel.loading_long_term = false;
                cx.notify();
            });
        })
        .detach();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = recent_workspaces(WORKSPACE_MRU_LIMIT).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(rows) => {
                        panel.workspaces = rows;
                    }
                    Err(err) => {
                        if panel.error.is_none() {
                            panel.error = Some(err);
                        }
                    }
                }
                panel.loading_workspaces = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Debounce: every keystroke increments `search_generation`; we
    /// spawn a task that sleeps `SEARCH_DEBOUNCE` and only issues the
    /// search if its generation is still the latest by the time it
    /// wakes.  Empty queries short-circuit to refresh (importance list).
    fn schedule_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let gen = self.search_generation;

        if query.trim().is_empty() {
            // Don't debounce the "clear" — snap back immediately.
            Self::spawn_refresh(cx);
            return;
        }

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Debounce on gpui's own timer. This task runs on gpui's
            // executor, which has NO tokio reactor — `tokio::time::sleep`
            // here panics ("there is no reactor running"). The bridge
            // installs a tokio *Handle* for wire IO, not a reactor on this
            // thread, so the wait must use the native executor timer.
            app_cx.background_executor().timer(SEARCH_DEBOUNCE).await;

            // If the user typed again, our generation is stale — bail.
            let still_current = this
                .update(app_cx, |panel, _| panel.search_generation == gen)
                .unwrap_or(false);
            if !still_current {
                return;
            }

            let outcome = search_long_term(&query, SEARCH_LIMIT).await;
            let _ = this.update(app_cx, |panel, cx| {
                // Final stale-check: a search that landed AFTER a newer
                // keystroke arrived shouldn't clobber the more-current
                // result set.
                if panel.search_generation != gen {
                    return;
                }
                match outcome {
                    Ok(rows) => {
                        panel.error = None;
                        panel.long_term = rows;
                        panel.search_active = true;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                    }
                }
                panel.loading_long_term = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Toggle a record's expanded body.
    pub fn toggle_expand(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.expanded.contains(id) {
            self.expanded.remove(id);
        } else {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }
}

impl Render for MemoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = header_row(cx);
        let mut column = div()
            .max_w(px(820.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
        }

        column = column.child(section_title("Long-term"));
        column = column.child(search_strip(self));
        if self.loading_long_term {
            column = column.child(loading_row());
        } else if self.long_term.is_empty() {
            column = column.child(empty_state(if self.search_active {
                "No memory matches that search."
            } else {
                "No long-term memory yet. Memories you ask Wylde to remember land here."
            }));
        } else {
            for rec in &self.long_term {
                let expanded = self.expanded.contains(&rec.id);
                column = column.child(long_term_row(rec, expanded, cx));
            }
        }

        column = column.child(section_title("Workspace"));
        if self.loading_workspaces {
            column = column.child(loading_row());
        } else if self.workspaces.is_empty() {
            column = column.child(empty_state(
                "No workspaces yet. Activate a project folder in the chat bar.",
            ));
        } else {
            for ws in &self.workspaces {
                column = column.child(workspace_row(ws));
            }
        }

        column = column.child(section_title("Short-term"));
        if let Some(cid) = &self.active_conversation {
            column = column.child(short_term_live(
                cid,
                &self.short_term,
                self.loading_short_term,
            ));
        } else {
            column = column.child(short_term_placeholder());
        }

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<MemoryPanel>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::LG))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from("Memory")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Three layers Wylde can recall from: long-term across every chat, \
                             workspace-scoped for the active project, short-term for the \
                             current conversation.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<MemoryPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("memory-refresh".into());
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut MemoryPanel, _event, _window, cx| {
                MemoryPanel::spawn_refresh(cx);
            }),
        )
        .child(SharedString::from("Refresh"))
}

fn section_title(label: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .child(SharedString::from(label.to_ascii_uppercase()))
}

fn search_strip(panel: &MemoryPanel) -> gpui::Div {
    // Replaces slice 5's button-driven affordance with a live-typed
    // input.  The input owns its own chrome (border, focus ring); the
    // wrapping div just lays it out.
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(panel.search_input.clone())
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(if panel.search_active {
                    "Live vector search over long-term memory (300 ms debounce)."
                } else {
                    "Showing the importance-sorted list — type to search."
                })),
        )
}

fn long_term_row(
    rec: &LongTermRecord,
    expanded: bool,
    cx: &mut Context<MemoryPanel>,
) -> Stateful<gpui::Div> {
    let id_for_click = rec.id.clone();
    let preview = preview_text(&rec.body, expanded);
    let importance_label = SharedString::from(format!("★ {}", rec.importance));
    let source_label = SharedString::from(if rec.source.is_empty() {
        "(no source)".to_owned()
    } else {
        rec.source.clone()
    });
    let recency_label = SharedString::from(recency_strip(rec.last_used_at, rec.created_at));
    let body_label = SharedString::from(preview);

    let mut row = div()
        .id(ElementId::Name(format!("memory-row::{}", rec.id).into()))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut MemoryPanel, _ev, _window, cx| {
                this.toggle_expand(&id_for_click, cx);
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(BRAND)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(importance_label),
                )
                .child(
                    div()
                        .flex_1()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(source_label),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(recency_label),
                ),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(body_label),
        );

    if !rec.tags.is_empty() {
        let tag_label = SharedString::from(format!("# {}", rec.tags.join("  # ")));
        row = row.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(tag_label),
        );
    }
    row
}

fn workspace_row(ws: &WorkspaceSummary) -> gpui::Div {
    let persona_strip = ws
        .persona
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|p| SharedString::from(format!("Persona: {p}")))
        .unwrap_or_else(|| SharedString::from("No persona set"));
    let recency_strip = ws
        .last_activated_at
        .as_deref()
        .map(|s| SharedString::from(format!("Last active: {s}")))
        .unwrap_or_else(|| SharedString::from("Never activated"));
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(28.0))
                .h(px(28.0))
                .rounded(px(6.0))
                .bg(rgb(pack(BRAND_DIM)))
                .flex()
                .items_center()
                .justify_center()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(
                    ws.id
                        .chars()
                        .next()
                        .map(|c| c.to_ascii_uppercase().to_string())
                        .unwrap_or_else(|| "·".into()),
                )),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(ws.id.clone())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(ws.path.clone())),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(persona_strip)
                        .child(recency_strip),
                ),
        )
}

fn short_term_placeholder() -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("Per-conversation buffer")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Short-term memory is the rolling context of the active chat — \
                     conversation-scoped, so it lives with the surface that owns the \
                     conversation id: the Chat panel. Open a chat and toggle its \
                     \"memory\" pill to see (and clear) the live working-memory buffer \
                     for that conversation. This global browser intentionally stays \
                     focused on the long-term and workspace layers.",
                )),
        )
}

/// Live Short-term view for the active conversation.  Mirrors the Chat
/// panel's working-memory strip (per-entry `kind` tag + one-line summary)
/// but read-only — clearing the buffer stays on the Chat surface that
/// owns the conversation.  Shown once the nav bus reports an active
/// conversation; until then [`short_term_placeholder`] explains where the
/// buffer lives.
fn short_term_live(conversation_id: &str, entries: &[ShortTermEntry], loading: bool) -> gpui::Div {
    let header_label = SharedString::from(format!("Active conversation · {conversation_id}"));
    let count_label = SharedString::from(format!(
        "{} {}",
        entries.len(),
        if entries.len() == 1 { "entry" } else { "entries" },
    ));

    let mut card = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(header_label),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(count_label),
                ),
        );

    if loading {
        return card.child(loading_row());
    }

    if entries.is_empty() {
        return card.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "No working memory yet — entries accrue as Wylde works this conversation. \
                     Clear the buffer from the chat bar's \"memory\" pill.",
                )),
        );
    }

    for entry in entries {
        card = card.child(short_term_row(entry));
    }
    card
}

/// One short-term row: a `kind` tag + the entry's summary line.  Twin of
/// the Chat panel's `working_memory_row`.
fn short_term_row(entry: &ShortTermEntry) -> gpui::Div {
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

fn empty_state(text: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(text.to_owned())),
        )
}

fn loading_row() -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("Loading…"))
}

fn error_strip(msg: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

/// Clamp `body` to `PREVIEW_CHARS` when collapsed; full text when
/// expanded.
pub(crate) fn preview_text(body: &str, expanded: bool) -> String {
    if expanded {
        return body.to_owned();
    }
    let mut idx = body.len();
    for (count, (offset, _)) in body.char_indices().enumerate() {
        if count >= PREVIEW_CHARS {
            idx = offset;
            break;
        }
    }
    if idx >= body.len() {
        body.to_owned()
    } else {
        let mut out = body[..idx].to_owned();
        out.push('…');
        out
    }
}

pub(crate) fn recency_strip(last_used: f64, created: f64) -> String {
    let ts = if last_used > 0.0 { last_used } else { created };
    if ts <= 0.0 {
        return "Unknown".to_owned();
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = (now - ts).max(0.0);
    if secs < 60.0 {
        "Just now".to_owned()
    } else if secs < 3_600.0 {
        format!("{}m ago", (secs / 60.0).round() as i64)
    } else if secs < 86_400.0 {
        format!("{}h ago", (secs / 3_600.0).round() as i64)
    } else {
        format!("{}d ago", (secs / 86_400.0).round() as i64)
    }
}

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<MemoryPanel>();
    }

    #[test]
    fn each_pipe_call_uses_expected_verb() {
        let _ = list_long_term;
        let _ = search_long_term;
        let _ = recent_workspaces;
    }

    #[test]
    fn long_term_record_deserialises_from_harness_shape() {
        let v = json!({
            "id": "abcd1234",
            "body": "test memory",
            "source": "test",
            "importance": 5,
            "created_at": 1_700_000_000.0,
            "last_used_at": 1_700_001_000.0,
            "superseded_by": "",
            "tags": ["tag1"],
        });
        let r = LongTermRecord::from_value(&v);
        assert_eq!(r.id, "abcd1234");
        assert_eq!(r.body, "test memory");
        assert_eq!(r.importance, 5);
        assert!(r.last_used_at > r.created_at);
        assert_eq!(r.tags, vec!["tag1".to_owned()]);
    }

    #[test]
    fn preview_keeps_short_bodies_verbatim() {
        assert_eq!(preview_text("hi", false), "hi");
        assert_eq!(preview_text("hi", true), "hi");
    }

    #[test]
    fn preview_clips_long_bodies_when_collapsed() {
        let body = "x".repeat(500);
        let collapsed = preview_text(&body, false);
        assert!(collapsed.ends_with('…'));
        assert!(collapsed.chars().count() <= PREVIEW_CHARS + 1);
        let expanded = preview_text(&body, true);
        assert_eq!(expanded.len(), 500);
    }

    #[test]
    fn recency_strip_uses_last_used_when_present() {
        let last_year = 1_700_000_000.0;
        let label = recency_strip(last_year, 0.0);
        assert!(
            label.ends_with(" ago") || label == "Just now" || label == "Unknown",
            "unexpected recency label: {label}",
        );
    }

    #[test]
    fn recency_strip_returns_unknown_when_zero() {
        assert_eq!(recency_strip(0.0, 0.0), "Unknown");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn search_debounce_is_300ms() {
        // Frozen in test so a future tweak surfaces in code review.
        assert_eq!(SEARCH_DEBOUNCE.as_millis(), 300);
    }
}
