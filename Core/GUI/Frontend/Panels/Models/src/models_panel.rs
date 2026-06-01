//! Models panel View — install / pull / delete / set-default surface
//! over the local Ollama daemon.
//!
//! State:
//!   * `installed`             — last `ollama.list_models` reply.
//!   * `loaded`                — `ollama.list_loaded` names; sources
//!     the "in use" pill.
//!   * `pull_input`            — `Entity<TextInput>`; "name to pull".
//!   * `active_pull`           — `Some(PullState)` while a stream is
//!     open.  Dropping the stream cancels the upstream pull (the
//!     harness wires client-disconnect → abandon).
//!   * `confirm_delete`        — the row id currently asking for an
//!     inline "Confirm delete?" Yes/Cancel.  Mutually exclusive — only
//!     one row at a time.
//!   * `session_default`       — per-session preference for the
//!     "default model" star.  Persistent storage waits on a
//!     `models.set_default` pipe verb the harness doesn't ship yet.
//!   * `hardware`              — `system.inventory` snapshot used by
//!     `recommend::pick`.  Empty / unknown is fine.
//!   * `error`                 — last surfaced pipe error.
//!   * `loading_*`             — per-section busy flags.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Stateful, Subscription, Window,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    delete_installed_model, list_installed_models, list_loaded_model_names, pull_model,
    read_hardware, HardwareSnapshot, InstalledModel, PullProgress,
};
use crate::recommend::{pick as pick_recommendations, Recommendation};

/// How often the panel re-polls `ollama.list_loaded` so the "in use"
/// pill tracks what the broker is actually holding.  Same 5 s cadence
/// the Svelte Models page uses.
const LOADED_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct ModelsPanel {
    pub installed: Vec<InstalledModel>,
    pub loaded: Vec<String>,
    pub pull_input: Entity<TextInput>,
    /// Single-line input owning the live filter query.  Pure
    /// presentation — the backend always serves the full installed list;
    /// this box only narrows what the View renders.  Empty means "show
    /// everything".
    pub search_input: Entity<TextInput>,
    /// Mirror of `search_input`'s text, updated on every `Changed` so the
    /// filter is a plain field read in render (no reaching into the input
    /// handle per row).
    pub search_query: String,
    pub active_pull: Option<PullState>,
    pub confirm_delete: Option<String>,
    pub session_default: Option<String>,
    pub hardware: HardwareSnapshot,
    pub error: Option<String>,
    pub loading_installed: bool,
    pub loading_hardware: bool,
    _input_sub: Subscription,
    _search_sub: Subscription,
}

/// In-flight pull bookkeeping.  The `stream` field holds the
/// `PipeStream`; dropping it cancels the upstream pull.
pub struct PullState {
    pub model_name: String,
    pub latest: PullProgress,
    pub stream: Option<wylde_gui_pipe::PipeStream>,
}

impl ModelsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let pull_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Model to pull (e.g. qwen2.5:1.5b)")
                .with_submit_mode(SubmitMode::EnterSubmits)
                .with_min_height(32.0)
                .with_element_key("models-pull-input")
        });
        let pull_input_for_sub = pull_input.clone();
        let input_sub = cx.subscribe(
            &pull_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Submit(_text) = event {
                    let name = pull_input_for_sub.read(cx).text().trim().to_owned();
                    if !name.is_empty() {
                        pull_input_for_sub.update(cx, |i, cx| i.clear(cx));
                        this.start_pull(name, cx);
                    }
                }
            },
        );

        let search_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Search models…")
                .with_submit_mode(SubmitMode::Never)
                .with_min_height(32.0)
                .with_element_key("models-search")
        });
        let search_sub = cx.subscribe(
            &search_input,
            move |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Changed(text) = event {
                    this.search_query = text.clone();
                    cx.notify();
                }
            },
        );

        Self {
            installed: Vec::new(),
            loaded: Vec::new(),
            pull_input,
            search_input,
            search_query: String::new(),
            active_pull: None,
            confirm_delete: None,
            session_default: None,
            hardware: HardwareSnapshot::default(),
            error: None,
            loading_installed: true,
            loading_hardware: true,
            _input_sub: input_sub,
            _search_sub: search_sub,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            Self::spawn_refresh_installed(cx);
            Self::spawn_refresh_hardware(cx);
            Self::spawn_loaded_poll(cx);
            Self::spawn_load_default(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh_installed(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_installed_models().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(rows) => {
                        panel.error = None;
                        panel.installed = rows;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                    }
                }
                panel.loading_installed = false;
                cx.notify();
            });
        })
        .detach();
    }

    pub fn spawn_refresh_hardware(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = read_hardware().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(hw) = outcome {
                    panel.hardware = hw;
                }
                panel.loading_hardware = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Long-lived task — every `LOADED_POLL_INTERVAL` re-reads
    /// `ollama.list_loaded` and merges the names into `loaded` so the
    /// per-row "in use" pill stays in sync with what the daemon
    /// actually has resident.
    pub fn spawn_loaded_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                let outcome = list_loaded_model_names().await;
                let still_alive = this
                    .update(app_cx, |panel, cx| {
                        if let Ok(names) = outcome {
                            panel.loaded = names;
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !still_alive {
                    return;
                }
                // gpui executor has no tokio reactor — native timer.
                app_cx
                    .background_executor()
                    .timer(LOADED_POLL_INTERVAL)
                    .await;
            }
        })
        .detach();
    }

    /// Kick off a pull.  The async task opens the streaming verb,
    /// drains chunks into `active_pull.latest`, and clears the slot
    /// on success / error / cancel.
    pub fn start_pull(&mut self, name: String, cx: &mut Context<Self>) {
        if self.active_pull.is_some() {
            return;
        }
        let stream = match pull_model(&name) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("pull start: {e}"));
                cx.notify();
                return;
            }
        };
        self.active_pull = Some(PullState {
            model_name: name.clone(),
            latest: PullProgress {
                status: "starting…".to_owned(),
                ..PullProgress::default()
            },
            stream: Some(stream),
        });
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                // Take the stream out so we can borrow it mutably for
                // recv; stash it back after to keep `cancel_pull`'s
                // drop-to-cancel path live.
                let next = match this.update(app_cx, |panel, _| {
                    panel.active_pull
                        .as_mut()
                        .and_then(|p| p.stream.take())
                }) {
                    Ok(Some(mut s)) => {
                        let frame = s.recv().await;
                        let _ = this.update(app_cx, |panel, _| {
                            if let Some(p) = panel.active_pull.as_mut() {
                                if p.stream.is_none() {
                                    p.stream = Some(s);
                                }
                            }
                        });
                        frame
                    }
                    _ => return,
                };
                match next {
                    Some(Ok(v)) => {
                        let progress = PullProgress::from_value(&v);
                        let done = progress.is_success();
                        let _ = this.update(app_cx, |panel, cx| {
                            if let Some(p) = panel.active_pull.as_mut() {
                                p.latest = progress;
                            }
                            cx.notify();
                        });
                        if done {
                            let _ = this.update(app_cx, |panel, cx| {
                                panel.active_pull = None;
                                cx.notify();
                                Self::spawn_refresh_installed(cx);
                            });
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = this.update(app_cx, |panel, cx| {
                            panel.error = Some(format!("pull '{name}': {e}"));
                            panel.active_pull = None;
                            cx.notify();
                        });
                        return;
                    }
                    None => {
                        // Stream ended without a success frame.
                        let _ = this.update(app_cx, |panel, cx| {
                            if panel.active_pull.is_some() {
                                panel.error = Some(format!(
                                    "pull '{name}': stream ended unexpectedly",
                                ));
                            }
                            panel.active_pull = None;
                            cx.notify();
                            Self::spawn_refresh_installed(cx);
                        });
                        return;
                    }
                }
            }
        })
        .detach();
    }

    /// Cancel-in-progress button — drops the stream, the harness wires
    /// the disconnect to "abandon".
    pub fn cancel_pull(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.active_pull.take() {
            // Dropping `p.stream` cancels.
            drop(p);
            cx.notify();
        }
    }

    /// Reset the filter — clears both the mirrored query and the input
    /// buffer.  Wired to the inline ✕ affordance and to Escape while the
    /// search box holds focus.  `input.clear` re-emits `Changed("")`,
    /// which the subscription folds back into `search_query`; setting it
    /// here too keeps the field correct even if that event is deferred.
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_query.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Inline confirm-delete — first click stages the confirmation,
    /// second confirms.  Cancel clears the staged state.
    pub fn request_delete(&mut self, name: String, cx: &mut Context<Self>) {
        self.confirm_delete = Some(name);
        cx.notify();
    }

    pub fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.confirm_delete.take() else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = delete_installed_model(&name).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Err(e) = outcome {
                    panel.error = Some(format!("delete '{name}': {e}"));
                } else if panel.session_default.as_deref() == Some(name.as_str()) {
                    panel.session_default = None;
                }
                cx.notify();
                Self::spawn_refresh_installed(cx);
            });
        })
        .detach();
    }

    pub fn set_default(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        // Optimistic mirror — flip the star instantly, then persist
        // through the harness so the choice survives a restart.  A write
        // failure only logs into the error strip; the in-session star
        // stays put so the UI doesn't flicker back.
        self.session_default = name.clone();
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = crate::ipc::set_default(name.as_deref()).await;
            if let Err(e) = outcome {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(format!("set_default: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Read the persisted default-model star on panel open and pre-check
    /// the matching row.  Soft-fails: a down harness leaves the star
    /// un-filled rather than surfacing an error toast.
    pub fn spawn_load_default(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Ok(Some(model)) = crate::ipc::get_default().await {
                let _ = this.update(app_cx, |panel, cx| {
                    // Don't clobber an explicit in-session choice the user
                    // made before the reply landed.
                    if panel.session_default.is_none() {
                        panel.session_default = Some(model);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }
}

impl Render for ModelsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = header_row(cx);
        let mut column = div()
            .max_w(px(860.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
        }

        column = column.child(section_title("Pull a model"));
        column = column.child(pull_section(self, cx));

        column = column.child(section_title("Installed models"));
        if self.loading_installed {
            column = column.child(loading_row());
        } else if self.installed.is_empty() {
            column = column.child(empty_installed_state());
        } else {
            column = column.child(search_strip(self, cx));
            let ranked = fuzzy_rank(&self.installed, &self.search_query);
            if ranked.is_empty() {
                column = column.child(no_match_state(self.search_query.trim()));
            } else {
                for m in ranked {
                    column = column.child(installed_row(self, m, cx));
                }
            }
        }

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<ModelsPanel>) -> gpui::Div {
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
                        .child(SharedString::from("Models")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Local LLMs Ollama is serving. Pull new tags, drop ones you no \
                             longer need, star a session default.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("models-refresh".into());
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
            cx.listener(|_this: &mut ModelsPanel, _event, _window, cx| {
                ModelsPanel::spawn_refresh_installed(cx);
                ModelsPanel::spawn_refresh_hardware(cx);
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

fn pull_section(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_3();

    let mut input_row = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_end()
        .child(div().flex_1().child(panel.pull_input.clone()));

    if panel.active_pull.is_some() {
        input_row = input_row.child(cancel_pull_button(cx));
    } else {
        input_row = input_row.child(pull_submit_button(panel.pull_input.clone(), cx));
    }
    col = col.child(input_row);

    if let Some(pull) = &panel.active_pull {
        col = col.child(pull_progress_strip(pull));
    } else {
        col = col.child(recommendations_strip(panel, cx));
    }

    col
}

fn pull_submit_button(
    input: Entity<TextInput>,
    cx: &mut Context<ModelsPanel>,
) -> Stateful<gpui::Div> {
    let listener_input = input.clone();
    div()
        .id(ElementId::Name("models-pull-submit".into()))
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
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                let name = listener_input.read(cx).text().trim().to_owned();
                if !name.is_empty() {
                    listener_input.update(cx, |i, cx| i.clear(cx));
                    this.start_pull(name, cx);
                }
            }),
        )
        .child(SharedString::from("Pull"))
}

fn cancel_pull_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name("models-pull-cancel".into()))
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
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.cancel_pull(cx);
            }),
        )
        .child(SharedString::from("Cancel"))
}

fn pull_progress_strip(pull: &PullState) -> gpui::Div {
    let pct_label = pull
        .latest
        .ratio()
        .map(|r| format!("{}%", (r * 100.0).round() as i32))
        .unwrap_or_else(|| "…".to_owned());
    let bytes_label = if pull.latest.total > 0 {
        format!(
            " — {} / {}",
            humanize_bytes(pull.latest.completed),
            humanize_bytes(pull.latest.total),
        )
    } else {
        String::new()
    };
    let label = SharedString::from(format!(
        "Pulling {} · {} · {}{}",
        pull.model_name, pull.latest.status, pct_label, bytes_label,
    ));
    let bar_width = pull.latest.ratio().unwrap_or(0.0);
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(label),
        )
        .child(
            div()
                .w_full()
                .h(px(4.0))
                .bg(rgb(pack(SURFACE_900)))
                .rounded(px(2.0))
                .child(
                    div()
                        .h(px(4.0))
                        .w(px((bar_width * 740.0).max(2.0)))
                        .bg(rgb(pack(BRAND)))
                        .rounded(px(2.0)),
                ),
        )
}

fn recommendations_strip(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> gpui::Div {
    let recs = pick_recommendations(&panel.hardware);
    let hint = SharedString::from(if panel.loading_hardware {
        "Probing hardware for recommendations…".to_owned()
    } else if panel.hardware.is_unknown() {
        "VRAM broker offline — recommendation is the universal starter.".to_owned()
    } else {
        format!(
            "Recommendations for {} · {} GB RAM{}",
            shorten_cpu(&panel.hardware.cpu_brand),
            (panel.hardware.ram_total_bytes / 1024 / 1024 / 1024).max(1),
            if panel.hardware.nvidia_count > 0 {
                format!(
                    " · {} GB VRAM",
                    (panel.hardware.nvidia_vram_bytes / 1024 / 1024 / 1024).max(1),
                )
            } else {
                String::new()
            },
        )
    });

    let mut row = div().flex().flex_row().flex_wrap().gap_2().mt_1();
    for r in recs {
        row = row.child(recommendation_chip(r, cx));
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(hint),
        )
        .child(row)
}

fn recommendation_chip(
    rec: Recommendation,
    cx: &mut Context<ModelsPanel>,
) -> Stateful<gpui::Div> {
    let name_for_click = rec.name.clone();
    let id: ElementId = ElementId::Name(format!("models-rec::{}", rec.name).into());
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_800)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.start_pull(name_for_click.clone(), cx);
            }),
        )
        .child(SharedString::from(rec.name))
}

fn installed_row(
    panel: &ModelsPanel,
    m: &InstalledModel,
    cx: &mut Context<ModelsPanel>,
) -> gpui::Div {
    let is_default = panel.session_default.as_deref() == Some(m.name.as_str());
    let is_loaded = panel.loaded.iter().any(|n| n == &m.name);
    let confirming = panel.confirm_delete.as_deref() == Some(m.name.as_str());

    let border = if is_default {
        BORDER_EMPHASIS
    } else {
        BORDER_SUBTLE
    };

    let mut row = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(border)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_2();

    row = row.child(installed_row_head(m, is_default, is_loaded, confirming, cx));

    if confirming {
        row = row.child(confirm_strip(cx));
    }

    row
}

/// Top line of an installed-model card: the default-star toggle, the
/// name + metadata column, then the conditional "loaded" pill and the
/// delete button (hidden while a delete confirmation is armed for this
/// model).
fn installed_row_head(
    m: &InstalledModel,
    is_default: bool,
    is_loaded: bool,
    confirming: bool,
    cx: &mut Context<ModelsPanel>,
) -> gpui::Div {
    let label = SharedString::from(m.name.clone());
    let meta = SharedString::from(model_meta(m));
    let name_for_default = m.name.clone();
    let name_for_request = m.name.clone();

    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(default_star(
            is_default,
            ElementId::Name(format!("models-default::{}", m.name).into()),
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                if this.session_default.as_deref() == Some(name_for_default.as_str()) {
                    this.set_default(None, cx);
                } else {
                    this.set_default(Some(name_for_default.clone()), cx);
                }
            }),
        ))
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
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(label),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(meta),
                ),
        );

    if is_loaded {
        head = head.child(loaded_pill());
    }
    if !confirming {
        head = head.child(delete_button(
            ElementId::Name(format!("models-del::{}", m.name).into()),
            cx.listener(move |this: &mut ModelsPanel, _ev, _window, cx| {
                this.request_delete(name_for_request.clone(), cx);
            }),
        ));
    }

    head
}

fn confirm_strip(cx: &mut Context<ModelsPanel>) -> gpui::Div {
    div()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pt_2()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(
                    "Confirm delete? This drops the model from local disk.",
                )),
        )
        .child(
            div()
                .id(ElementId::Name("models-confirm-yes".into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(BRAND_DIM)))
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                        this.confirm_delete(cx);
                    }),
                )
                .child(SharedString::from("Yes, delete")),
        )
        .child(
            div()
                .id(ElementId::Name("models-confirm-no".into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                        this.cancel_delete(cx);
                    }),
                )
                .child(SharedString::from("Cancel")),
        )
}

fn default_star<F>(is_default: bool, id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let (label, colour) = if is_default {
        ("★", BRAND)
    } else {
        ("☆", TEXT_MUTED)
    };
    div()
        .id(id)
        .w(px(24.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(colour)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from(label))
}

fn loaded_pill() -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND)))
        .child(SharedString::from("in use"))
}

fn delete_button<F>(id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from("Delete"))
}

/// Live filter box above the installed list.  The input owns its own
/// chrome; this wrapper lays it out beside the conditional ✕ clear
/// affordance and catches Escape (bubbled up from the focused input) to
/// reset the query — matching the keyboard behaviour of the other
/// panels' inputs without piercing the shared `TextInput`.
fn search_strip(panel: &ModelsPanel, cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    let mut row = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(div().flex_1().child(panel.search_input.clone()));

    if !panel.search_query.trim().is_empty() {
        row = row.child(clear_search_button(cx));
    }

    div()
        .id(ElementId::Name("models-search-strip".into()))
        .on_key_down(cx.listener(
            |this: &mut ModelsPanel, ev: &gpui::KeyDownEvent, _window, cx| {
                if ev.keystroke.key.as_str() == "escape" && !this.search_query.is_empty() {
                    this.clear_search(cx);
                }
            },
        ))
        .child(row)
}

fn clear_search_button(cx: &mut Context<ModelsPanel>) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name("models-search-clear".into()))
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut ModelsPanel, _ev, _window, cx| {
                this.clear_search(cx);
            }),
        )
        .child(SharedString::from("✕"))
}

/// In-list empty state when the filter excludes every installed model.
/// Subtle (same card as the no-models state) rather than a full-panel
/// takeover — the search box stays visible above it so the user can
/// adjust the query.
fn no_match_state(query: &str) -> gpui::Div {
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
                .child(SharedString::from(format!("No models match “{query}”."))),
        )
}

fn empty_installed_state() -> gpui::Div {
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
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("No models installed yet")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Pull one of the recommendations above to get started.",
                )),
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
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

// ── Pure projections (unit-testable) ─────────────────────────────────

/// The text a model is matched against: its name plus the same
/// metadata the row surfaces (family, parameter size, quantization),
/// space-joined.  Folding them into one haystack lets a query like
/// "qwen", "7b", or "q4" hit whichever field carries it.
pub(crate) fn searchable_text(m: &InstalledModel) -> String {
    let mut s = m.name.clone();
    for extra in [&m.family, &m.param_size, &m.quantization] {
        if !extra.is_empty() {
            s.push(' ');
            s.push_str(extra);
        }
    }
    s
}

/// Fuzzy-rank installed models against `query` for the search box.
///
///   * Empty / whitespace query → every model, original order preserved.
///   * Otherwise → only the models whose `searchable_text` fuzzily
///     matches, sorted by descending nucleo relevance score (best first).
///     Equal scores fall back to name order so the list is deterministic
///     across renders instead of jittering.
///
/// Fuzzy (subsequence) matching gives the UX Aaron asked for: "qwen"
/// matches every qwen tag, the typo "lama" still hits "llama3.2" (l-a-m-a
/// is a subsequence), and a fragment like "32b" narrows to the 32b tags —
/// all ranked rather than an unordered substring set.
pub(crate) fn fuzzy_rank<'a>(
    models: &'a [InstalledModel],
    query: &str,
) -> Vec<&'a InstalledModel> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return models.iter().collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(trimmed, CaseMatching::Smart, Normalization::Smart);
    // One scratch buffer reused across haystacks — `Utf32Str::new` clears
    // it per call (and skips it entirely for ASCII), so reuse is safe.
    let mut buf: Vec<char> = Vec::new();

    let mut scored: Vec<(u32, &InstalledModel)> = models
        .iter()
        .filter_map(|m| {
            let haystack = searchable_text(m);
            let utf = Utf32Str::new(&haystack, &mut buf);
            pattern.score(utf, &mut matcher).map(|score| (score, m))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, m)| m).collect()
}

pub(crate) fn model_meta(m: &InstalledModel) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !m.family.is_empty() {
        parts.push(m.family.clone());
    }
    if !m.param_size.is_empty() {
        parts.push(m.param_size.clone());
    }
    if !m.quantization.is_empty() {
        parts.push(m.quantization.clone());
    }
    if m.size_bytes > 0 {
        parts.push(humanize_bytes(m.size_bytes));
    }
    if parts.is_empty() {
        m.name.clone()
    } else {
        parts.join(" · ")
    }
}

pub(crate) fn humanize_bytes(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = b as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

pub(crate) fn shorten_cpu(brand: &str) -> String {
    // Strip frequency suffixes like "@ 3.50GHz" the broker frequently
    // bundles into the CPU brand on Windows so the hint chip stays
    // readable.
    let mut s = brand.to_owned();
    if let Some(at) = s.find('@') {
        s.truncate(at);
    }
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "CPU".to_owned()
    } else {
        trimmed.to_owned()
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
        assert_render::<ModelsPanel>();
    }

    #[test]
    fn humanize_bytes_picks_units_per_magnitude() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(2048), "2 KB");
        assert_eq!(humanize_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(humanize_bytes(7_500_000_000_u64), "7.0 GB");
    }

    #[test]
    fn model_meta_concatenates_details_when_present() {
        let m = InstalledModel {
            name: "qwen2.5:1.5b".into(),
            family: "qwen2.5".into(),
            param_size: "1.5B".into(),
            quantization: "Q4_K_M".into(),
            size_bytes: 1_500_000_000,
            ..Default::default()
        };
        assert_eq!(model_meta(&m), "qwen2.5 · 1.5B · Q4_K_M · 1.4 GB");
    }

    fn model(name: &str, family: &str, param: &str, quant: &str) -> InstalledModel {
        InstalledModel {
            name: name.into(),
            family: family.into(),
            param_size: param.into(),
            quantization: quant.into(),
            ..Default::default()
        }
    }

    fn names(ranked: &[&InstalledModel]) -> Vec<String> {
        ranked.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn fuzzy_rank_empty_query_preserves_original_order() {
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        // Empty and whitespace-only both short-circuit to "show all".
        assert_eq!(names(&fuzzy_rank(&models, "")), names(&models.iter().collect::<Vec<_>>()));
        assert_eq!(names(&fuzzy_rank(&models, "   ")), names(&models.iter().collect::<Vec<_>>()));
    }

    #[test]
    fn fuzzy_rank_clean_prefix_keeps_only_matching_family() {
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "qwen"));
        assert!(got.iter().all(|n| n.contains("qwen")), "only qwen tags: {got:?}");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn fuzzy_rank_tolerates_typo() {
        // "lama" is a subsequence of "llama3.2" (l-a-m-a) but matches no
        // qwen tag — the dropped 'l' is forgiven without false positives.
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("llama3.2", "llama", "", ""),
        ];
        let got = names(&fuzzy_rank(&models, "lama"));
        assert_eq!(got, vec!["llama3.2".to_owned()]);
    }

    #[test]
    fn fuzzy_rank_fragment_matches_quant_and_param() {
        // "32b" narrows to the 32b tag; the 7b tag has no '3' to match.
        let models = vec![
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
            model("qwen3:32b", "qwen3", "32B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "32b"));
        assert_eq!(got, vec!["qwen3:32b".to_owned()]);
    }

    #[test]
    fn fuzzy_rank_sorts_descending_by_score() {
        // Both match "qwen", but a leading-prefix match outscores one
        // buried mid-name. Input order is the reverse of the expected
        // ranking, so a pass proves the sort reorders by score.
        let models = vec![
            model("zzz-qwen-custom", "", "", ""),
            model("qwen2.5:7b", "qwen2.5", "7B", "Q4_K_M"),
        ];
        let got = names(&fuzzy_rank(&models, "qwen"));
        assert_eq!(got, vec!["qwen2.5:7b".to_owned(), "zzz-qwen-custom".to_owned()]);
    }

    #[test]
    fn model_meta_falls_back_to_name_when_no_details() {
        let m = InstalledModel {
            name: "mystery:latest".into(),
            ..Default::default()
        };
        assert_eq!(model_meta(&m), "mystery:latest");
    }

    #[test]
    fn shorten_cpu_strips_frequency_suffix() {
        assert_eq!(shorten_cpu("Intel Core i7-9750H @ 2.60GHz"), "Intel Core i7-9750H");
        assert_eq!(shorten_cpu("  AMD Ryzen 9 7900X "), "AMD Ryzen 9 7900X");
    }

    #[test]
    fn shorten_cpu_defaults_when_empty() {
        assert_eq!(shorten_cpu(""), "CPU");
        assert_eq!(shorten_cpu("  @ 2.0GHz"), "CPU");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn pull_progress_value_round_trip_through_view_field() {
        // Exercises the from_value → store-on-panel path the View uses.
        let v = json!({
            "status": "downloading",
            "completed": 5_u64,
            "total": 20_u64,
            "digest": "sha256:abcd",
        });
        let progress = PullProgress::from_value(&v);
        assert_eq!(progress.ratio(), Some(0.25));
    }
}
