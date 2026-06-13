//! Images panel View.
//!
//! Layout (top → bottom):
//!
//!   * Header — title + manual Refresh button.
//!   * Error / degraded-state strip when the gateway pipe is down.
//!   * Generate-new bar — multi-line prompt + model pill + Generate.
//!     While a generation is in flight, the bar shows a Stop button
//!     that aborts the underlying task and the gallery refreshes once
//!     the future resolves (success or cancel).
//!   * Filter chips — date range (All / Week / Month), workspace,
//!     model.  Selection drives a pure local re-projection of the
//!     loaded library; we never round-trip a filter to the server.
//!   * Two-column body — gallery grid on the left, metadata pane on
//!     the right when a row is selected.  Clicking a thumbnail toggles
//!     selection.  Clicking outside (the gallery's empty space) leaves
//!     selection alone — the metadata pane has an explicit Close
//!     affordance.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, Image, ImageFormat, ImageSource, IntoElement, Render, SharedString, Stateful,
    Subscription, Task, Window,
};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_LIGHT, SURFACE_700, SURFACE_800,
    SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    delete_image, generate, read_image, read_library, read_models, GenerateRequest, ImageEntry,
    ImageModel,
};

/// Max number of thumbnails decoded on the initial load.  Beyond this,
/// the "Load more" affordance fetches the next page.  Keeps the pipe
/// from doing N inline-base64 round-trips on first paint of a large
/// library.
pub const THUMBNAIL_PAGE: usize = 48;

/// Auto-refresh cadence.  The library lives on disk; it changes only
/// when a generation or import lands, so we don't poll aggressively.
/// 30 s matches what Dashboard's slowest cards use.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Date-range filter chip.  All-time is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateFilter {
    All,
    Week,
    Month,
}

impl DateFilter {
    pub fn label(self) -> &'static str {
        match self {
            DateFilter::All => "All time",
            DateFilter::Week => "This week",
            DateFilter::Month => "This month",
        }
    }

    /// True when the given Unix-seconds timestamp falls inside this
    /// filter's window.  Pure function so the filter chip tests don't
    /// need a clock.
    pub fn matches(self, created_at: f64, now: f64) -> bool {
        match self {
            DateFilter::All => true,
            DateFilter::Week => (now - created_at) <= 7.0 * 86_400.0,
            DateFilter::Month => (now - created_at) <= 30.0 * 86_400.0,
        }
    }
}

/// Per-thumbnail cache slot.  We hold the decoded `Arc<Image>` so gpui's
/// `use_asset` skips the decode after the first paint.  Public so the
/// containing field can stay `pub` for the panel's other field-style
/// tests + integration callers.
#[derive(Clone)]
pub enum ThumbState {
    Pending,
    Decoded(Arc<Image>),
    Failed(String),
}

pub struct ImagesPanel {
    pub library: Vec<ImageEntry>,
    pub thumbnails: HashMap<String, ThumbState>,
    pub thumbnail_quota: usize,
    pub models: Vec<ImageModel>,
    pub selected_id: Option<String>,
    pub date_filter: DateFilter,
    pub workspace_filter: Option<String>,
    pub model_filter: Option<String>,
    pub show_workspace_dropdown: bool,
    pub show_model_dropdown: bool,
    pub last_error: Option<String>,
    pub initial_load_done: bool,
    pub confirm_delete: Option<String>,
    pub generate_running: bool,
    pub generate_error: Option<String>,
    pub generate_last_id: Option<String>,
    /// Stop was clicked but the gateway/ComfyUI job can't actually be
    /// cancelled mid-run (no cancel verb), so it keeps running in the
    /// background. We keep `generate_running` true (no overlapping GPU
    /// jobs) and show an honest "finishing in the background" notice until
    /// the original future resolves.
    pub generate_detached: bool,
    pub prompt_input: Entity<TextInput>,
    pub selected_model: Option<String>,
    /// In-flight generate task.  Dropping the gpui `Task` only aborts the
    /// gpui-side await — the work was dispatched onto the tokio runtime via
    /// the Pipe bridge, whose `JoinHandle` is detached (not aborted) on
    /// drop, so the gateway request keeps running. We therefore hold the
    /// task to completion rather than dropping it on Stop.
    pub generate_task: Option<Task<()>>,
    _input_sub: Subscription,
}

impl ImagesPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let prompt_input = cx.new(|input_cx| {
            TextInput::multi_line(input_cx)
                .with_placeholder("Describe an image to generate  ·  Ctrl+Enter to submit")
                .with_submit_mode(SubmitMode::ModEnterSubmits)
                .with_min_height(56.0)
                .with_max_height(140.0)
                .with_element_key("images-prompt")
        });
        let input_sub = cx.subscribe(
            &prompt_input,
            |this: &mut Self, _entity, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::Submit(text) = event {
                    this.submit_generate(text.clone(), cx);
                }
            },
        );

        Self {
            library: Vec::new(),
            thumbnails: HashMap::new(),
            thumbnail_quota: THUMBNAIL_PAGE,
            models: Vec::new(),
            selected_id: None,
            date_filter: DateFilter::All,
            workspace_filter: None,
            model_filter: None,
            show_workspace_dropdown: false,
            show_model_dropdown: false,
            last_error: None,
            initial_load_done: false,
            confirm_delete: None,
            generate_running: false,
            generate_error: None,
            generate_last_id: None,
            generate_detached: false,
            prompt_input,
            selected_model: None,
            generate_task: None,
            _input_sub: input_sub,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            Self::spawn_refresh_loop(cx);
            Self::spawn_load_models(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                Self::refresh_library(this.clone(), app_cx).await;
                // gpui executor has no tokio reactor — native timer.
                app_cx.background_executor().timer(REFRESH_INTERVAL).await;
                let still_alive = this.update(app_cx, |_, _| {}).is_ok();
                if !still_alive {
                    return;
                }
            }
        })
        .detach();
    }

    pub async fn refresh_library(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let result = read_library().await;
        let _ = this.update(app_cx, |panel, cx| {
            match result {
                Ok(rows) => {
                    panel.library = rows;
                    panel.last_error = None;
                    panel.initial_load_done = true;
                    // Prune stale thumbnail cache entries.
                    let live: BTreeSet<String> =
                        panel.library.iter().map(|r| r.id.clone()).collect();
                    panel.thumbnails.retain(|k, _| live.contains(k));
                }
                Err(e) => {
                    panel.initial_load_done = true;
                    panel.last_error = Some(format!("gateway: {e}"));
                }
            }
            // Kick off any new thumbnail fetches within the current
            // quota.  Pending decoded entries stay cached.
            panel.kick_thumbnail_fetches(cx);
            cx.notify();
        });
    }

    pub fn spawn_load_models(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = read_models().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(rows) = result {
                    panel.models = rows;
                    if panel.selected_model.is_none() {
                        panel.selected_model = panel.models.first().map(|m| m.name.clone());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn spawn_manual_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::refresh_library(this.clone(), app_cx).await;
        })
        .detach();
    }

    /// Schedule a fetch+decode for every visible entry inside the
    /// current quota that doesn't yet have a `Decoded` slot.
    pub fn kick_thumbnail_fetches(&mut self, cx: &mut Context<Self>) {
        let mut to_fetch: Vec<String> = Vec::new();
        for entry in self.library.iter().take(self.thumbnail_quota) {
            if !self.thumbnails.contains_key(&entry.id) {
                self.thumbnails
                    .insert(entry.id.clone(), ThumbState::Pending);
                to_fetch.push(entry.id.clone());
            }
        }
        for id in to_fetch {
            cx.spawn({
                let id = id.clone();
                async move |this, app_cx: &mut AsyncApp| {
                    let result = read_image(&id).await;
                    let _ = this.update(app_cx, |panel, cx| {
                        match result {
                            Ok(bytes) => {
                                let format = format_for_mime(&bytes.mime);
                                let image = Arc::new(Image::from_bytes(format, bytes.bytes));
                                panel
                                    .thumbnails
                                    .insert(id.clone(), ThumbState::Decoded(image));
                            }
                            Err(e) => {
                                panel.thumbnails.insert(id.clone(), ThumbState::Failed(e));
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    pub fn load_more(&mut self, cx: &mut Context<Self>) {
        self.thumbnail_quota = self.thumbnail_quota.saturating_add(THUMBNAIL_PAGE);
        self.kick_thumbnail_fetches(cx);
    }

    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        // Toggle off when clicking the already-selected row.
        if self.selected_id.as_deref() == Some(id) {
            self.selected_id = None;
            self.confirm_delete = None;
        } else {
            self.selected_id = Some(id.to_owned());
            // Reset the inline confirm strip when selection changes.
            self.confirm_delete = None;
        }
        cx.notify();
    }

    pub fn close_pane(&mut self, cx: &mut Context<Self>) {
        self.selected_id = None;
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn set_date_filter(&mut self, f: DateFilter, cx: &mut Context<Self>) {
        self.date_filter = f;
        cx.notify();
    }

    pub fn set_workspace_filter(&mut self, ws: Option<String>, cx: &mut Context<Self>) {
        self.workspace_filter = ws;
        self.show_workspace_dropdown = false;
        cx.notify();
    }

    pub fn set_model_filter(&mut self, m: Option<String>, cx: &mut Context<Self>) {
        self.model_filter = m;
        self.show_model_dropdown = false;
        cx.notify();
    }

    pub fn toggle_workspace_dropdown(&mut self, cx: &mut Context<Self>) {
        self.show_workspace_dropdown = !self.show_workspace_dropdown;
        if self.show_workspace_dropdown {
            self.show_model_dropdown = false;
        }
        cx.notify();
    }

    pub fn toggle_model_dropdown(&mut self, cx: &mut Context<Self>) {
        self.show_model_dropdown = !self.show_model_dropdown;
        if self.show_model_dropdown {
            self.show_workspace_dropdown = false;
        }
        cx.notify();
    }

    pub fn arm_delete(&mut self, id: &str, cx: &mut Context<Self>) {
        self.confirm_delete = Some(id.to_owned());
        cx.notify();
    }

    pub fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        cx.notify();
    }

    pub fn spawn_confirm_delete(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = delete_image(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match result {
                    Ok(_removed) => {
                        panel.library.retain(|r| r.id != id);
                        panel.thumbnails.remove(&id);
                        if panel.selected_id.as_deref() == Some(&id) {
                            panel.selected_id = None;
                        }
                        panel.confirm_delete = None;
                    }
                    Err(e) => {
                        panel.last_error = Some(format!("delete: {e}"));
                        panel.confirm_delete = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn select_model(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.selected_model = name;
        self.show_model_dropdown = false;
        cx.notify();
    }

    pub fn submit_generate(&mut self, text: String, cx: &mut Context<Self>) {
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() || self.generate_running {
            return;
        }
        self.generate_running = true;
        self.generate_error = None;
        self.generate_last_id = None;
        self.generate_detached = false;
        // Clear the input now so the user sees the panel taking over.
        let input = self.prompt_input.clone();
        input.update(cx, |i, cx| i.clear(cx));

        let req = GenerateRequest {
            prompt: trimmed,
            model: self.selected_model.clone(),
            workspace_id: None,
        };

        let task = cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = generate(req).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.generate_running = false;
                panel.generate_detached = false;
                panel.generate_task = None;
                match outcome {
                    Ok(out) => {
                        panel.generate_last_id = out.id.clone();
                        // Library refresh so the new image shows up; the
                        // gateway library route reads the directory each
                        // call, so this is the right hook.
                        Self::spawn_manual_refresh(cx);
                    }
                    Err(e) => {
                        panel.generate_error = Some(e);
                    }
                }
                cx.notify();
            });
        });
        self.generate_task = Some(task);
        cx.notify();
    }

    pub fn cancel_generate(&mut self, cx: &mut Context<Self>) {
        // A ComfyUI generate can't be cancelled mid-run (no cancel verb),
        // and the request was dispatched onto tokio via the Pipe bridge —
        // dropping the gpui task would only detach it, leaving the GPU job
        // running while re-enabling Generate (→ overlapping jobs). Instead
        // we keep the task to completion and just mark it detached: the
        // submit guard stays engaged and the notice goes honest. The
        // original future clears this state when the job actually finishes.
        if !self.generate_running || self.generate_detached {
            return;
        }
        self.generate_detached = true;
        cx.notify();
    }

    /// Pure projection — every filter applied in order.  Returned in
    /// the same descending-time order the gateway hands us.
    pub fn visible_rows(&self, now: f64) -> Vec<&ImageEntry> {
        filter_rows(
            &self.library,
            self.date_filter,
            self.workspace_filter.as_deref(),
            self.model_filter.as_deref(),
            now,
        )
    }

    /// Distinct workspace ids found across the loaded library.
    pub fn workspaces_in_library(&self) -> Vec<String> {
        workspaces_in(&self.library)
    }

    /// Distinct model names found across the loaded library.
    pub fn models_in_library(&self) -> Vec<String> {
        models_in(&self.library)
    }
}

/// Pure filter pipeline.  Free function so the tests below can exercise
/// it without instantiating a panel (the panel needs a gpui `Context`
/// for its `Entity<TextInput>` field).
pub fn filter_rows<'a>(
    library: &'a [ImageEntry],
    date_filter: DateFilter,
    workspace_filter: Option<&str>,
    model_filter: Option<&str>,
    now: f64,
) -> Vec<&'a ImageEntry> {
    library
        .iter()
        .filter(|e| date_filter.matches(e.created_at, now))
        .filter(|e| match workspace_filter {
            None => true,
            Some(ws) => e.workspace_id().map(|w| w == ws).unwrap_or(false),
        })
        .filter(|e| match model_filter {
            None => true,
            Some(m) => e.model().map(|x| x == m).unwrap_or(false),
        })
        .collect()
}

/// Distinct workspace ids across the library (sorted, lex order).  Free
/// function for the same reason as `filter_rows`.
pub fn workspaces_in(library: &[ImageEntry]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for e in library {
        if let Some(ws) = e.workspace_id() {
            set.insert(ws.to_owned());
        }
    }
    set.into_iter().collect()
}

/// Distinct model names across the library (sorted, lex order).
pub fn models_in(library: &[ImageEntry]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for e in library {
        if let Some(m) = e.model() {
            set.insert(m.to_owned());
        }
    }
    set.into_iter().collect()
}

fn format_for_mime(mime: &str) -> ImageFormat {
    match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::Webp,
        "image/gif" => ImageFormat::Gif,
        "image/bmp" => ImageFormat::Bmp,
        "image/tiff" | "image/tif" => ImageFormat::Tiff,
        _ => ImageFormat::Png,
    }
}

impl Render for ImagesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = unix_now();
        let mut column = div().flex().flex_col().gap_5().child(header_row(cx));

        if let Some(err) = &self.last_error {
            column = column.child(error_strip(err));
        }

        column = column.child(section_title("Generate new"));
        column = column.child(generate_bar(self, cx));

        column = column.child(section_title("Filters"));
        column = column.child(filter_row(self, cx));
        if self.show_workspace_dropdown {
            column = column.child(workspace_dropdown(self, cx));
        }
        if self.show_model_dropdown {
            column = column.child(model_dropdown(self, cx));
        }

        column = column.child(section_title("Library"));
        column = column.child(body_split(self, now, cx));

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

// ── Header ───────────────────────────────────────────────────────────

fn header_row(cx: &mut Context<ImagesPanel>) -> gpui::Div {
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
                        .child(SharedString::from("Images")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "ComfyUI library + generate-new bar.  The gateway proxies all \
                             image-gen traffic through wylde-gateway:8005 → ComfyUI:8014.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<ImagesPanel>) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name("images-refresh".into()))
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
            cx.listener(|_this: &mut ImagesPanel, _ev, _w, cx| {
                ImagesPanel::spawn_manual_refresh(cx);
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

// ── Generate bar ─────────────────────────────────────────────────────

fn generate_bar(panel: &ImagesPanel, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let model_label = match &panel.selected_model {
        Some(name) => SharedString::from(format!("model · {name}")),
        None => SharedString::from("model · auto"),
    };
    let prompt_input = panel.prompt_input.clone();

    let action_button = if panel.generate_detached {
        // Stop was clicked but the job is still finishing — no re-stop, and
        // Generate stays guarded. A non-interactive "Finishing…" pill.
        pill_button(
            ElementId::Name("images-generate-finishing".into()),
            SharedString::from("Finishing…"),
            cx.listener(|_this: &mut ImagesPanel, _ev, _w, _cx| {}),
        )
    } else if panel.generate_running {
        pill_button(
            ElementId::Name("images-generate-stop".into()),
            SharedString::from("Stop"),
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.cancel_generate(cx);
            }),
        )
    } else {
        pill_button(
            ElementId::Name("images-generate-submit".into()),
            SharedString::from("Generate"),
            cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                let text = this.prompt_input.read(cx).text().to_owned();
                this.submit_generate(text, cx);
            }),
        )
    };

    let mut col = div().flex().flex_col().gap_2();
    col = col.child(prompt_input);
    col = col.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .child(pill_button(
                ElementId::Name("images-model-toggle".into()),
                model_label,
                cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                    this.toggle_model_dropdown(cx);
                }),
            ))
            .child(action_button),
    );

    if panel.generate_running {
        let notice = if panel.generate_detached {
            "Can't cancel a ComfyUI job mid-run — it's finishing in the background, then it'll \
             appear in the library."
        } else {
            "Generation in flight — the ComfyUI proxy may take up to 10 min on a slow GPU."
        };
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .child(SharedString::from(notice)),
        );
    }
    if let Some(err) = &panel.generate_error {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BORDER_EMPHASIS)))
                .child(SharedString::from(format!("Generate failed · {err}"))),
        );
    }
    if let Some(id) = &panel.generate_last_id {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(format!(
                    "Last generated · {id} (refresh picked it up)",
                ))),
        );
    }
    card_shell(col)
}

// ── Filter row ───────────────────────────────────────────────────────

fn filter_row(panel: &ImagesPanel, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let workspace_label = SharedString::from(match &panel.workspace_filter {
        Some(ws) => format!("workspace · {ws}"),
        None => "workspace · all".to_owned(),
    });
    let model_filter_label = SharedString::from(match &panel.model_filter {
        Some(m) => format!("model · {m}"),
        None => "model · all".to_owned(),
    });
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_2()
        .items_center()
        .child(filter_chip(
            "All time",
            "images-filter-all",
            panel.date_filter == DateFilter::All,
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.set_date_filter(DateFilter::All, cx);
            }),
        ))
        .child(filter_chip(
            "This week",
            "images-filter-week",
            panel.date_filter == DateFilter::Week,
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.set_date_filter(DateFilter::Week, cx);
            }),
        ))
        .child(filter_chip(
            "This month",
            "images-filter-month",
            panel.date_filter == DateFilter::Month,
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.set_date_filter(DateFilter::Month, cx);
            }),
        ))
        .child(pill_button(
            ElementId::Name("images-workspace-toggle".into()),
            workspace_label,
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.toggle_workspace_dropdown(cx);
            }),
        ))
        .child(pill_button(
            ElementId::Name("images-modelfilter-toggle".into()),
            model_filter_label,
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.toggle_model_dropdown(cx);
            }),
        ))
}

fn filter_chip<F>(
    label: &'static str,
    id_str: &'static str,
    active: bool,
    listener: F,
) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let (border, text) = if active {
        (BORDER_EMPHASIS, BRAND_LIGHT)
    } else {
        (BORDER_SUBTLE, TEXT_SECONDARY)
    };
    div()
        .id(ElementId::Name(id_str.into()))
        .px_3()
        .py_1()
        .rounded(px(12.0))
        .border_1()
        .border_color(rgb(pack(border)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(text)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from(label))
}

fn workspace_dropdown(panel: &ImagesPanel, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let mut col = dropdown_shell();
    let live = panel.workspaces_in_library();
    col = col.child(dropdown_row(
        ElementId::Name("images-workspace-all".into()),
        SharedString::from("All workspaces"),
        cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
            this.set_workspace_filter(None, cx);
        }),
    ));
    if live.is_empty() {
        col = col.child(empty_dropdown_row(
            "No workspaces recorded in this library yet",
        ));
        return col;
    }
    for ws in live {
        let ws_for_select = ws.clone();
        col = col.child(dropdown_row(
            ElementId::Name(format!("images-workspace-pick::{}", ws).into()),
            SharedString::from(ws),
            cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                this.set_workspace_filter(Some(ws_for_select.clone()), cx);
            }),
        ));
    }
    col
}

fn model_dropdown(panel: &ImagesPanel, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let mut col = dropdown_shell();
    let library_models = panel.models_in_library();
    let mut all_models = panel
        .models
        .iter()
        .map(|m| m.name.clone())
        .collect::<BTreeSet<_>>();
    all_models.extend(library_models.iter().cloned());

    // "All models" resets the filter.
    col = col.child(dropdown_row(
        ElementId::Name("images-model-all".into()),
        SharedString::from("All models"),
        cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
            this.set_model_filter(None, cx);
        }),
    ));
    if all_models.is_empty() {
        col = col.child(empty_dropdown_row("No models known yet"));
        return col;
    }
    for name in all_models {
        let name_for_select = name.clone();
        let name_for_pick = name.clone();
        col = col.child(dropdown_row(
            ElementId::Name(format!("images-model-pick::{}", name).into()),
            SharedString::from(name),
            cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                // Picker chooses both the active generation model
                // AND filters the gallery to that model.  Keeps the
                // surface area small without losing utility.
                this.set_model_filter(Some(name_for_select.clone()), cx);
                this.select_model(Some(name_for_pick.clone()), cx);
            }),
        ));
    }
    col
}

// ── Body split: gallery + metadata pane ──────────────────────────────

fn body_split(panel: &ImagesPanel, now: f64, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let rows = panel.visible_rows(now);
    let total = rows.len();
    let gallery = gallery_grid(panel, &rows, cx);
    let pane = panel.selected_id.as_ref().and_then(|id| {
        panel
            .library
            .iter()
            .find(|e| &e.id == id)
            .map(|e| metadata_pane(panel, e, cx))
    });

    let mut row = div().flex().flex_row().gap_4().w_full();
    row = row.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_grow()
            .child(gallery)
            .child(footer_strip(panel, total, cx)),
    );
    if let Some(p) = pane {
        row = row.child(p);
    }
    row
}

fn gallery_grid(
    panel: &ImagesPanel,
    rows: &[&ImageEntry],
    cx: &mut Context<ImagesPanel>,
) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Reading the image library…");
    }
    if rows.is_empty() {
        return placeholder_card(
            "No images match these filters yet.  Generate one with the bar above, \
             or relax the date / workspace / model filters.",
        );
    }
    let mut grid = div().flex().flex_row().flex_wrap().gap_3();
    for entry in rows {
        let selected = panel.selected_id.as_deref() == Some(entry.id.as_str());
        grid = grid.child(thumbnail_tile(panel, entry, selected, cx));
    }
    grid
}

fn thumbnail_tile(
    panel: &ImagesPanel,
    entry: &ImageEntry,
    selected: bool,
    cx: &mut Context<ImagesPanel>,
) -> Stateful<gpui::Div> {
    let state = panel.thumbnails.get(&entry.id).cloned();
    let label = SharedString::from(short_label(entry));
    let id_for_click = entry.id.clone();
    let id: ElementId = ElementId::Name(format!("images-tile::{}", entry.id).into());
    let border = if selected { BRAND } else { BORDER_SUBTLE };

    let inner: gpui::AnyElement = match state {
        Some(ThumbState::Decoded(image)) => img(ImageSource::Image(image))
            .w(px(132.0))
            .h(px(132.0))
            .rounded(px(4.0))
            .into_any_element(),
        Some(ThumbState::Failed(_)) => broken_thumb_inner("decode failed").into_any_element(),
        Some(ThumbState::Pending) | None => shimmer_inner().into_any_element(),
    };

    div()
        .id(id)
        .w(px(148.0))
        .h(px(172.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(pack(border)))
        .bg(rgb(pack(SURFACE_800)))
        .cursor_pointer()
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                this.select(&id_for_click, cx);
            }),
        )
        .child(inner)
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(label),
        )
}

fn shimmer_inner() -> gpui::Div {
    div()
        .w(px(132.0))
        .h(px(132.0))
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_700)))
}

fn broken_thumb_inner(label: &str) -> gpui::Div {
    div()
        .w(px(132.0))
        .h(px(132.0))
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_700)))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(label.to_owned())),
        )
}

fn footer_strip(
    panel: &ImagesPanel,
    visible_total: usize,
    cx: &mut Context<ImagesPanel>,
) -> gpui::Div {
    let loaded = panel.library.iter().take(panel.thumbnail_quota).count();
    let total = panel.library.len();
    let line = SharedString::from(format!(
        "Showing {visible_total} of {total} · thumbnails decoded: {loaded}",
    ));
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .pt_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(line),
        );
    if loaded < total {
        row = row.child(pill_button(
            ElementId::Name("images-load-more".into()),
            SharedString::from("Load more"),
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.load_more(cx);
            }),
        ));
    }
    row
}

// ── Metadata pane ────────────────────────────────────────────────────

fn metadata_pane(
    panel: &ImagesPanel,
    entry: &ImageEntry,
    cx: &mut Context<ImagesPanel>,
) -> gpui::Div {
    let id_for_jump = entry.workspace_id().map(|s| s.to_owned());
    let armed = panel.confirm_delete.as_deref() == Some(entry.id.as_str());

    let mut col = div().flex().flex_col().gap_3();
    col = col.child(metadata_header(entry, cx));

    // Big preview reuses the same decoded image as the thumbnail.
    let preview = panel.thumbnails.get(&entry.id).cloned();
    let preview_el: gpui::AnyElement = match preview {
        Some(ThumbState::Decoded(image)) => img(ImageSource::Image(image))
            .w(px(280.0))
            .h(px(280.0))
            .rounded(px(4.0))
            .into_any_element(),
        Some(ThumbState::Failed(e)) => broken_thumb_inner(&e).into_any_element(),
        _ => shimmer_inner().into_any_element(),
    };
    col = col.child(preview_el);

    col = col.child(meta_line(
        "Prompt",
        entry.prompt().unwrap_or("(no prompt recorded)"),
    ));
    if let Some(s) = entry.seed() {
        col = col.child(meta_line("Seed", &s));
    }
    if let Some(m) = entry.model() {
        col = col.child(meta_line("Model", m));
    }
    if let (Some(w), Some(h)) = (entry.width(), entry.height()) {
        col = col.child(meta_line("Size", &format!("{w}×{h}")));
    }
    col = col.child(meta_line("File size", &humanize_bytes(entry.size_bytes)));
    col = col.child(meta_line("Created", &fmt_relative(entry.created_at)));
    col = col.child(meta_line("Source", entry.source()));
    if let Some(ws) = id_for_jump.clone() {
        col = col.child(workspace_jump_row(&entry.id, &ws, cx));
    }

    col = col.child(delete_controls(entry, armed, cx));

    div()
        .w(px(320.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .child(col)
}

/// Header of the metadata pane: the filename on the left, a `×` close
/// pill on the right.
fn metadata_header(entry: &ImageEntry, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from(entry.filename.clone())),
        )
        .child(pill_button(
            ElementId::Name(format!("images-pane-close::{}", entry.id).into()),
            SharedString::from("×"),
            cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                this.close_pane(cx);
            }),
        ))
}

/// Delete affordance at the bottom of the metadata pane.  When `armed`
/// (the entry is the one queued for deletion) this is the inline
/// "Delete permanently?" Yes/Cancel strip; otherwise a single "Delete"
/// pill that arms it.
fn delete_controls(
    entry: &ImageEntry,
    armed: bool,
    cx: &mut Context<ImagesPanel>,
) -> gpui::AnyElement {
    let id_for_delete = entry.id.clone();
    if armed {
        let id_for_yes = id_for_delete.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pt_2()
            .child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from("Delete this image permanently?")),
            )
            .child(pill_button(
                ElementId::Name(format!("images-delete-yes::{}", entry.id).into()),
                SharedString::from("Yes"),
                cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                    this.spawn_confirm_delete(id_for_yes.clone(), cx);
                }),
            ))
            .child(pill_button(
                ElementId::Name(format!("images-delete-cancel::{}", entry.id).into()),
                SharedString::from("Cancel"),
                cx.listener(|this: &mut ImagesPanel, _ev, _w, cx| {
                    this.cancel_delete(cx);
                }),
            ))
            .into_any_element()
    } else {
        pill_button(
            ElementId::Name(format!("images-delete-arm::{}", entry.id).into()),
            SharedString::from("Delete"),
            cx.listener(move |this: &mut ImagesPanel, _ev, _w, cx| {
                let id = id_for_delete.clone();
                this.arm_delete(&id, cx);
            }),
        )
        .into_any_element()
    }
}

fn meta_line(label: &str, value: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from(label.to_ascii_uppercase())),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(value.to_owned())),
        )
}

fn workspace_jump_row(entry_id: &str, ws: &str, cx: &mut Context<ImagesPanel>) -> gpui::Div {
    let id: ElementId = ElementId::Name(format!("images-ws-jump::{entry_id}::{ws}").into());
    let label = SharedString::from(format!("workspace · {ws}  ›  open"));
    div().flex().flex_row().items_center().gap_2().child(
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
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_this: &mut ImagesPanel, _ev, _w, _cx| {
                    let _ = wylde_gui_pipe::request_nav("core/workspaces");
                }),
            )
            .child(label),
    )
}

// ── Shared widgets (mirrors RemoteAccess) ────────────────────────────

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

fn dropdown_shell() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_2()
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
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(label)
}

fn empty_dropdown_row(label: &str) -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(label.to_owned()))
}

fn card_shell(body: gpui::Div) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .child(body)
}

fn placeholder_card(text: &str) -> gpui::Div {
    card_shell(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(text.to_owned())),
    )
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

// ── Pure projections (no gpui) ───────────────────────────────────────

pub(crate) fn short_label(entry: &ImageEntry) -> String {
    if let Some(prompt) = entry.prompt() {
        truncate(prompt, 28)
    } else if entry.filename.is_empty() {
        entry.id.clone()
    } else {
        truncate(&entry.filename, 28)
    }
}

pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
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
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

pub(crate) fn fmt_relative(epoch: f64) -> String {
    let now = unix_now();
    let secs = (now - epoch).max(0.0);
    if epoch <= 0.0 {
        return "—".to_owned();
    }
    if secs < 60.0 {
        "just now".to_owned()
    } else if secs < 3_600.0 {
        format!("{}m ago", (secs / 60.0).round() as i64)
    } else if secs < 86_400.0 {
        format!("{}h ago", (secs / 3_600.0).round() as i64)
    } else if secs < 30.0 * 86_400.0 {
        format!("{}d ago", (secs / 86_400.0).round() as i64)
    } else {
        format!("{}mo ago", (secs / (30.0 * 86_400.0)).round() as i64)
    }
}

fn unix_now() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

    fn entry(id: &str, ts: f64, ws: Option<&str>, model: Option<&str>) -> ImageEntry {
        let mut meta = serde_json::Map::new();
        if let Some(w) = ws {
            meta.insert("workspace_id".into(), serde_json::Value::from(w.to_owned()));
        }
        if let Some(m) = model {
            meta.insert("model".into(), serde_json::Value::from(m.to_owned()));
        }
        ImageEntry {
            id: id.to_owned(),
            filename: format!("{id}.png"),
            size_bytes: 1024,
            created_at: ts,
            metadata: serde_json::Value::Object(meta),
        }
    }

    #[test]
    fn date_filter_window_math() {
        let now = 1_780_000_000.0;
        let week_ago = now - 6.5 * 86_400.0;
        let month_ago = now - 25.0 * 86_400.0;
        let year_ago = now - 365.0 * 86_400.0;
        assert!(DateFilter::All.matches(year_ago, now));
        assert!(DateFilter::Week.matches(week_ago, now));
        assert!(!DateFilter::Week.matches(month_ago, now));
        assert!(DateFilter::Month.matches(month_ago, now));
        assert!(!DateFilter::Month.matches(year_ago, now));
    }

    #[test]
    fn date_filter_labels_are_distinct() {
        assert_ne!(DateFilter::All.label(), DateFilter::Week.label());
        assert_ne!(DateFilter::Week.label(), DateFilter::Month.label());
    }

    #[test]
    fn filter_rows_applies_every_filter_in_order() {
        let now = 1_780_000_000.0;
        let library = vec![
            entry("a", now, Some("ws-1"), Some("sdxl")),
            entry("b", now, Some("ws-2"), Some("sdxl")),
            entry("c", now, Some("ws-1"), Some("flux")),
            // Older than a month — date filter should drop this.
            entry("d", now - 35.0 * 86_400.0, Some("ws-1"), Some("flux")),
        ];
        let rows = filter_rows(&library, DateFilter::All, Some("ws-1"), None, now);
        assert_eq!(rows.len(), 3);
        let rows = filter_rows(&library, DateFilter::Month, Some("ws-1"), None, now);
        assert_eq!(rows.len(), 2);
        let rows = filter_rows(&library, DateFilter::Month, Some("ws-1"), Some("flux"), now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "c");
    }

    #[test]
    fn workspaces_in_dedupes_and_sorts_lex() {
        let library = vec![
            entry("a", 0.0, Some("ws-2"), None),
            entry("b", 0.0, Some("ws-1"), None),
            entry("c", 0.0, Some("ws-1"), None),
            entry("d", 0.0, None, None),
        ];
        assert_eq!(
            workspaces_in(&library),
            vec!["ws-1".to_owned(), "ws-2".to_owned()],
        );
    }

    #[test]
    fn models_in_dedupes_and_sorts_lex() {
        let library = vec![
            entry("a", 0.0, None, Some("sdxl")),
            entry("b", 0.0, None, Some("flux")),
            entry("c", 0.0, None, Some("sdxl")),
            entry("d", 0.0, None, None),
        ];
        assert_eq!(
            models_in(&library),
            vec!["flux".to_owned(), "sdxl".to_owned()],
        );
    }

    #[test]
    fn short_label_truncates_to_28_chars() {
        let e = ImageEntry {
            id: "x".into(),
            filename: "x.png".into(),
            size_bytes: 0,
            created_at: 0.0,
            metadata: json!({"prompt": "this prompt is exactly thirty chars long here"}),
        };
        let label = short_label(&e);
        assert!(label.ends_with('…'));
        // 28 chars + the trailing ellipsis.
        assert!(label.chars().count() <= 29);
    }

    #[test]
    fn short_label_falls_back_to_filename() {
        let e = ImageEntry {
            id: "id1".into(),
            filename: "snapshot.png".into(),
            size_bytes: 0,
            created_at: 0.0,
            metadata: json!({}),
        };
        assert_eq!(short_label(&e), "snapshot.png");
    }

    #[test]
    fn short_label_falls_back_to_id_when_filename_blank() {
        let e = ImageEntry {
            id: "deadbeef".into(),
            filename: String::new(),
            size_bytes: 0,
            created_at: 0.0,
            metadata: json!({}),
        };
        assert_eq!(short_label(&e), "deadbeef");
    }

    #[test]
    fn truncate_passes_short_through() {
        assert_eq!(truncate("hi", 10), "hi");
        let long = "abcdefghijklmnop";
        let out = truncate(long, 5);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 6);
    }

    #[test]
    fn humanize_bytes_picks_units_per_magnitude() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(2048), "2 KB");
        assert_eq!(humanize_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(humanize_bytes(7_500_000_000_u64), "7.0 GB");
    }

    #[test]
    fn fmt_relative_handles_zero_epoch() {
        assert_eq!(fmt_relative(0.0), "—");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn format_for_mime_handles_known_and_unknown() {
        assert_eq!(format_for_mime("image/png"), ImageFormat::Png);
        assert_eq!(format_for_mime("image/jpeg"), ImageFormat::Jpeg);
        assert_eq!(format_for_mime("image/jpg"), ImageFormat::Jpeg);
        assert_eq!(format_for_mime("image/webp"), ImageFormat::Webp);
        assert_eq!(format_for_mime("image/gif"), ImageFormat::Gif);
        assert_eq!(format_for_mime("image/bmp"), ImageFormat::Bmp);
        // Unknown defaults to PNG (most common image-gen output).
        assert_eq!(format_for_mime("image/unknown"), ImageFormat::Png);
    }

    #[test]
    fn thumbnail_page_constants_are_sane() {
        // Locks the wiring constants so a future tweak to make them
        // zero-or-negative is caught here rather than at the user.
        const _: () = assert!(THUMBNAIL_PAGE >= 1);
        assert!(REFRESH_INTERVAL.as_secs() >= 1);
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<ImagesPanel>();
    }
}
