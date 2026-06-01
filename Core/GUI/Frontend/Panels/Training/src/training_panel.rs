//! Training panel View.
//!
//! Layout (top → bottom):
//!
//!   * Header — title + manual Refresh button + service-degraded strip
//!     when the trainer pipe is unreachable.
//!   * Active runs strip — one card per running / queued job with
//!     progress + cancel.  Empty state when nothing's active.
//!   * Past runs list — completed / failed / stopped jobs.  Click a row
//!     to expand it inline: shows config metadata, loss-curve bar
//!     histogram (fed by per-job `status` fetch), and a "Resume from
//!     this" affordance that pre-fills the start form.
//!   * Start new training form — base model + dataset + LoRA + optim
//!     params + checkpoint path (when set by resume).  Submit calls
//!     `start_training`; the new run lands in the active strip on the
//!     next refresh tick.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Stateful, Subscription, Window,
};
use wylde_gpui_input::{InputEvent, SubmitMode, TextInput};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_LIGHT, SURFACE_700, SURFACE_800,
    SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    list_datasets, list_jobs, job_status, start_training, stop_training, DatasetRow, JobRow,
    JobStatus, LossPoint, TrainingConfig,
};

/// Cadence the panel re-polls `list_jobs`.  Matches the Svelte page's
/// 5 s tick; the trainer's status route is cheap and the loss history
/// changes meaningfully on this cadence during a run.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum bars rendered in the sparkline.  Loss histories on long runs
/// reach into the thousands; we down-sample to keep the row tight.
pub const SPARKLINE_MAX_BARS: usize = 64;

/// Form field identifiers — kept as an enum so `update_field` doesn't
/// take stringly-typed keys and a typo can't silently route to no field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    BaseModel,
    LoraRank,
    LoraAlpha,
    LoraDropout,
    BatchSize,
    GradAccum,
    LearningRate,
    NumEpochs,
    CutoffLen,
}

pub struct TrainingPanel {
    pub jobs: Vec<JobRow>,
    pub datasets: Vec<DatasetRow>,
    pub config: TrainingConfig,
    /// Expanded past-run row id; at most one row at a time.
    pub expanded: Option<String>,
    /// Per-job loss history, keyed by job id.  Lazily populated when
    /// the user expands a row.
    pub loss_history: HashMap<String, Vec<LossPoint>>,
    /// Per-job last-fetch error (for the expanded row's degraded
    /// strip).
    pub status_errors: HashMap<String, String>,
    pub last_error: Option<String>,
    pub initial_load_done: bool,
    pub submit_error: Option<String>,
    pub submit_busy: bool,
    pub last_started_id: Option<String>,
    pub show_dataset_dropdown: bool,
    pub base_model_input: Entity<TextInput>,
    pub lora_rank_input: Entity<TextInput>,
    pub lora_alpha_input: Entity<TextInput>,
    pub lora_dropout_input: Entity<TextInput>,
    pub batch_size_input: Entity<TextInput>,
    pub grad_accum_input: Entity<TextInput>,
    pub learning_rate_input: Entity<TextInput>,
    pub num_epochs_input: Entity<TextInput>,
    pub cutoff_len_input: Entity<TextInput>,
    _subs: Vec<Subscription>,
}

impl TrainingPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = TrainingConfig::default();

        let base_model_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("e.g. meta-llama/Llama-3.2-3B")
                .with_submit_mode(SubmitMode::Never)
                .with_min_height(32.0)
                .with_element_key("training-base-model")
        });
        let lora_rank_input = mk_numeric(cx, "training-lora-rank", &config.lora_rank.to_string());
        let lora_alpha_input =
            mk_numeric(cx, "training-lora-alpha", &config.lora_alpha.to_string());
        let lora_dropout_input = mk_numeric(
            cx,
            "training-lora-dropout",
            &format_float(config.lora_dropout),
        );
        let batch_size_input =
            mk_numeric(cx, "training-batch-size", &config.batch_size.to_string());
        let grad_accum_input =
            mk_numeric(cx, "training-grad-accum", &config.grad_accum.to_string());
        let learning_rate_input = mk_numeric(
            cx,
            "training-learning-rate",
            &format_float(config.learning_rate),
        );
        let num_epochs_input =
            mk_numeric(cx, "training-num-epochs", &format_float(config.num_epochs));
        let cutoff_len_input =
            mk_numeric(cx, "training-cutoff-len", &config.cutoff_len.to_string());

        let subs = [
            (base_model_input.clone(), Field::BaseModel),
            (lora_rank_input.clone(), Field::LoraRank),
            (lora_alpha_input.clone(), Field::LoraAlpha),
            (lora_dropout_input.clone(), Field::LoraDropout),
            (batch_size_input.clone(), Field::BatchSize),
            (grad_accum_input.clone(), Field::GradAccum),
            (learning_rate_input.clone(), Field::LearningRate),
            (num_epochs_input.clone(), Field::NumEpochs),
            (cutoff_len_input.clone(), Field::CutoffLen),
        ]
        .into_iter()
        .map(|(entity, field)| {
            cx.subscribe(
                &entity,
                move |this: &mut Self, _e, ev: &InputEvent, cx: &mut Context<Self>| {
                    if let InputEvent::Changed(text) = ev {
                        this.update_field(field, text, cx);
                    }
                },
            )
        })
        .collect();

        Self {
            jobs: Vec::new(),
            datasets: Vec::new(),
            config,
            expanded: None,
            loss_history: HashMap::new(),
            status_errors: HashMap::new(),
            last_error: None,
            initial_load_done: false,
            submit_error: None,
            submit_busy: false,
            last_started_id: None,
            show_dataset_dropdown: false,
            base_model_input,
            lora_rank_input,
            lora_alpha_input,
            lora_dropout_input,
            batch_size_input,
            grad_accum_input,
            learning_rate_input,
            num_epochs_input,
            cutoff_len_input,
            _subs: subs,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new(cx);
            Self::spawn_refresh_loop(cx);
            Self::spawn_load_datasets(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            Self::refresh_jobs(this.clone(), app_cx).await;
            // gpui executor has no tokio reactor — native timer.
            app_cx.background_executor().timer(REFRESH_INTERVAL).await;
            if this.update(app_cx, |_, _| {}).is_err() {
                return;
            }
        })
        .detach();
    }

    pub async fn refresh_jobs(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let result = list_jobs().await;
        let _ = this.update(app_cx, |panel, cx| {
            match result {
                Ok(rows) => {
                    panel.jobs = rows;
                    panel.last_error = None;
                    panel.initial_load_done = true;
                }
                Err(e) => {
                    panel.initial_load_done = true;
                    panel.last_error = Some(format!("trainer: {e}"));
                }
            }
            cx.notify();
        });
    }

    pub fn spawn_manual_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::refresh_jobs(this.clone(), app_cx).await;
        })
        .detach();
    }

    pub fn spawn_load_datasets(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = list_datasets().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Ok(rows) = result {
                    panel.datasets = rows;
                }
                // Failure here is non-fatal: dataset list stays empty
                // and the picker degrades to a "no datasets" hint that
                // still allows manual entry of a dataset name.
                cx.notify();
            });
        })
        .detach();
    }

    pub fn update_field(&mut self, field: Field, text: &str, cx: &mut Context<Self>) {
        let trimmed = text.trim();
        let mut maybe_err: Option<String> = None;
        match field {
            Field::BaseModel => {
                self.config.base_model = trimmed.to_owned();
            }
            Field::LoraRank => match trimmed.parse::<u32>() {
                Ok(n) => self.config.lora_rank = n,
                Err(_) => maybe_err = Some("LoRA rank must be a positive integer".into()),
            },
            Field::LoraAlpha => match trimmed.parse::<u32>() {
                Ok(n) => self.config.lora_alpha = n,
                Err(_) => maybe_err = Some("LoRA alpha must be a positive integer".into()),
            },
            Field::LoraDropout => match trimmed.parse::<f64>() {
                Ok(n) => self.config.lora_dropout = n,
                Err(_) => maybe_err = Some("LoRA dropout must be a decimal in [0, 0.9]".into()),
            },
            Field::BatchSize => match trimmed.parse::<u32>() {
                Ok(n) => self.config.batch_size = n,
                Err(_) => maybe_err = Some("Batch size must be a positive integer".into()),
            },
            Field::GradAccum => match trimmed.parse::<u32>() {
                Ok(n) => self.config.grad_accum = n,
                Err(_) => {
                    maybe_err = Some("Gradient accumulation must be a positive integer".into())
                }
            },
            Field::LearningRate => match trimmed.parse::<f64>() {
                Ok(n) => self.config.learning_rate = n,
                Err(_) => maybe_err = Some("Learning rate must be a decimal".into()),
            },
            Field::NumEpochs => match trimmed.parse::<f64>() {
                Ok(n) => self.config.num_epochs = n,
                Err(_) => maybe_err = Some("Epochs must be a decimal".into()),
            },
            Field::CutoffLen => match trimmed.parse::<u32>() {
                Ok(n) => self.config.cutoff_len = n,
                Err(_) => maybe_err = Some("Cutoff length must be a positive integer".into()),
            },
        }
        // Surface parse errors immediately but don't gate the rest of
        // the form on a single invalid field.
        if maybe_err.is_some() {
            self.submit_error = maybe_err;
        } else if self.submit_error.is_some() {
            // Clear stale parse errors when the offending field is
            // re-typed into a valid shape.
            self.submit_error = self.config.validate().err();
        }
        cx.notify();
    }

    pub fn pick_dataset(&mut self, name: String, cx: &mut Context<Self>) {
        self.config.dataset_name = name;
        self.show_dataset_dropdown = false;
        if self.submit_error.is_some() {
            self.submit_error = self.config.validate().err();
        }
        cx.notify();
    }

    pub fn toggle_dataset_dropdown(&mut self, cx: &mut Context<Self>) {
        self.show_dataset_dropdown = !self.show_dataset_dropdown;
        cx.notify();
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.submit_busy {
            return;
        }
        if let Err(e) = self.config.validate() {
            self.submit_error = Some(e);
            cx.notify();
            return;
        }
        self.submit_busy = true;
        self.submit_error = None;
        let cfg = self.config.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = start_training(&cfg).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.submit_busy = false;
                match outcome {
                    Ok(o) => {
                        panel.last_started_id = Some(o.job_id);
                        // Refresh immediately so the new row shows up
                        // without waiting for the 5s tick.
                        Self::spawn_manual_refresh(cx);
                    }
                    Err(e) => {
                        panel.submit_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn stop_job(&mut self, job_id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = stop_training(&job_id).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Err(e) = result {
                    panel.last_error = Some(format!("stop_training: {e}"));
                }
                Self::spawn_manual_refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_expand(&mut self, id: String, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(id.as_str()) {
            self.expanded = None;
            cx.notify();
            return;
        }
        self.expanded = Some(id.clone());
        // Lazily fetch the loss history on first expand.  Subsequent
        // expansions reuse the cached vector.
        if !self.loss_history.contains_key(&id) {
            self.spawn_fetch_status(id, cx);
        }
        cx.notify();
    }

    pub fn spawn_fetch_status(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = job_status(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match result {
                    Ok(st) => {
                        panel.loss_history.insert(id.clone(), st.loss_history);
                        panel.status_errors.remove(&id);
                    }
                    Err(e) => {
                        panel.status_errors.insert(id.clone(), e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pre-fill the form from a past run's config + checkpoint.  Also
    /// pushes the new strings into every TextInput so the user sees the
    /// effect immediately.
    pub fn resume_from(&mut self, row: &JobRow, cx: &mut Context<Self>) {
        let mut cfg = TrainingConfig::from_value(&row.config);
        if let Some(ckpt) = &row.checkpoint_path {
            cfg.resume_from_checkpoint = ckpt.clone();
        }
        self.config = cfg.clone();
        // Push the new values into the input buffers so the form
        // reflects them.
        self.base_model_input
            .update(cx, |input, cx| input.set_text(cfg.base_model.clone(), cx));
        self.lora_rank_input
            .update(cx, |input, cx| input.set_text(cfg.lora_rank.to_string(), cx));
        self.lora_alpha_input
            .update(cx, |input, cx| input.set_text(cfg.lora_alpha.to_string(), cx));
        self.lora_dropout_input
            .update(cx, |input, cx| input.set_text(format_float(cfg.lora_dropout), cx));
        self.batch_size_input
            .update(cx, |input, cx| input.set_text(cfg.batch_size.to_string(), cx));
        self.grad_accum_input
            .update(cx, |input, cx| input.set_text(cfg.grad_accum.to_string(), cx));
        self.learning_rate_input.update(cx, |input, cx| {
            input.set_text(format_float(cfg.learning_rate), cx)
        });
        self.num_epochs_input.update(cx, |input, cx| {
            input.set_text(format_float(cfg.num_epochs), cx)
        });
        self.cutoff_len_input
            .update(cx, |input, cx| input.set_text(cfg.cutoff_len.to_string(), cx));
        self.submit_error = self.config.validate().err();
        cx.notify();
    }

    pub fn active_rows(&self) -> Vec<&JobRow> {
        self.jobs
            .iter()
            .filter(|r| r.status_enum().is_active())
            .collect()
    }

    pub fn past_rows(&self) -> Vec<&JobRow> {
        self.jobs
            .iter()
            .filter(|r| r.status_enum().is_terminal())
            .collect()
    }
}

// ── Pure rendering helpers ───────────────────────────────────────────

fn mk_numeric(cx: &mut Context<TrainingPanel>, key: &'static str, seed: &str) -> Entity<TextInput> {
    cx.new(|input_cx| {
        TextInput::single_line(input_cx)
            .with_submit_mode(SubmitMode::Never)
            .with_min_height(32.0)
            .with_initial_text(seed.to_owned())
            .with_element_key(key)
    })
}

/// Format a float for display in the form: trims trailing zeros and
/// keeps scientific notation for very small numbers so the user can
/// tell `2e-4` from `0.000`.
pub fn format_float(n: f64) -> String {
    if n == 0.0 {
        return "0".to_owned();
    }
    let abs = n.abs();
    if !(1e-3..1e6).contains(&abs) {
        // Stay in scientific form so the value is legible.
        format!("{:e}", n)
    } else {
        let mut s = format!("{:.6}", n);
        // Trim trailing zeros but keep at least one decimal.
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        s
    }
}

/// Down-sample a loss history into N evenly-spaced buckets for the bar
/// histogram.  Pure function so the sparkline tests don't need a gpui
/// context.
pub fn downsample_losses(points: &[LossPoint], buckets: usize) -> Vec<f64> {
    if points.is_empty() || buckets == 0 {
        return Vec::new();
    }
    if points.len() <= buckets {
        return points.iter().map(|p| p.loss).collect();
    }
    let mut out = Vec::with_capacity(buckets);
    let len = points.len();
    for b in 0..buckets {
        let start = b * len / buckets;
        let end = ((b + 1) * len / buckets).max(start + 1).min(len);
        let slice = &points[start..end];
        let avg = slice.iter().map(|p| p.loss).sum::<f64>() / (slice.len() as f64);
        out.push(avg);
    }
    out
}

/// Normalise a slice of loss values to [0, 1] for bar-height mapping.
/// Pure function; returns an empty vector for empty input or constant
/// series so the renderer can branch on `is_empty()`.
pub fn normalise(values: &[f64]) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }
    let (min, max) = values
        .iter()
        .copied()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    let span = max - min;
    if span <= f64::EPSILON {
        // Constant series — render every bar at half height so the user
        // sees "there's data but it's flat" rather than no histogram.
        return values.iter().map(|_| 0.5).collect();
    }
    // Loss is "lower is better" — flip so the leftmost-tallest visual
    // matches the user expectation (tall bar = high loss = bad).
    values
        .iter()
        .map(|v| ((v - min) / span).clamp(0.0, 1.0))
        .collect()
}

/// Humanise a duration in seconds for the past-runs list metric strip.
pub fn fmt_duration_seconds(s: u64) -> String {
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m {}s", s / 60, s % 60);
    }
    let h = s / 3600;
    let m = (s % 3600) / 60;
    format!("{h}h {m}m")
}

/// Humanise an ETA in seconds for the active-runs strip.  "—" when zero
/// so a fresh run doesn't read "ETA 0s" while the trainer is warming up.
pub fn fmt_eta_seconds(s: u64) -> String {
    if s == 0 {
        "—".to_owned()
    } else {
        fmt_duration_seconds(s)
    }
}

/// Compact status colour mapping.  Active runs use the brand pill,
/// terminal failures use the emphasis border (the warm accent we already
/// use for error states across the GUI).
pub fn status_color(status: JobStatus) -> gpui::Rgba {
    match status {
        JobStatus::Running => BRAND_LIGHT,
        JobStatus::Queued => BRAND_DIM_TEXT,
        JobStatus::Completed => BRAND,
        JobStatus::Failed => BORDER_EMPHASIS,
        JobStatus::Stopped => TEXT_MUTED,
        JobStatus::Unknown => TEXT_MUTED,
    }
}

/// Hand-rolled dim text colour for queued runs.  Pulled out so the bar
/// renderer can also reach for it without round-tripping through the
/// theme module — there's no `BRAND_DIM_TEXT` in `wylde_theme` today and
/// adding one to the shared crate would be off-scope for this slice.
const BRAND_DIM_TEXT: gpui::Rgba = wylde_theme::colors::BRAND_DIM;

// ── Render ───────────────────────────────────────────────────────────

impl Render for TrainingPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col().gap_5().child(header_row(cx));
        if let Some(err) = &self.last_error {
            column = column.child(error_strip(err));
        }
        column = column.child(section_title("Active runs"));
        column = column.child(active_strip(self, cx));
        column = column.child(section_title("Past runs"));
        column = column.child(past_list(self, cx));
        column = column.child(section_title("Start a new run"));
        column = column.child(start_form(self, cx));

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

// ── Header ───────────────────────────────────────────────────────────

fn header_row(cx: &mut Context<TrainingPanel>) -> gpui::Div {
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
                        .child(SharedString::from("Training")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Fine-tune a model with LoRA.  Status polls every 5 s — \
                             expand a past run to see its loss history.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<TrainingPanel>) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name("training-refresh".into()))
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
            cx.listener(|_this: &mut TrainingPanel, _ev, _w, cx| {
                TrainingPanel::spawn_manual_refresh(cx);
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

// ── Active strip ─────────────────────────────────────────────────────

fn active_strip(panel: &TrainingPanel, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let rows = panel.active_rows();
    if !panel.initial_load_done {
        return placeholder_card("Reading the trainer pipe…");
    }
    if rows.is_empty() {
        return placeholder_card(
            "No active training.  Use the form below to start a new run.",
        );
    }
    let mut col = div().flex().flex_col().gap_3();
    for row in rows {
        col = col.child(active_card(row, cx));
    }
    col
}

fn active_card(row: &JobRow, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let epoch_line = SharedString::from(format!(
        "epoch {} / {} · loss {} · ETA {}",
        row.current_epoch
            .map(|n| format!("{:.2}", n))
            .unwrap_or_else(|| "—".to_owned()),
        row.total_epochs
            .map(|n| format!("{}", n))
            .unwrap_or_else(|| "—".to_owned()),
        row.final_loss
            .map(|n| format!("{:.4}", n))
            .unwrap_or_else(|| "—".to_owned()),
        fmt_eta_seconds(row.eta_seconds.unwrap_or(0)),
    ));
    let progress_pct = row.progress.clamp(0.0, 100.0);

    let header = active_card_header(row, cx);

    // Progress bar width: fixed 480 px total, matches Models' pull strip
    // sizing (gpui at `b3d93d44` doesn't expose `relative` percentages
    // on `width`, so the bar gets an absolute outer width).
    const PROGRESS_BAR_TOTAL_PX: f32 = 480.0;
    let fill_px = ((progress_pct as f32 / 100.0) * PROGRESS_BAR_TOTAL_PX).max(2.0);
    let progress_bar = div()
        .w(px(PROGRESS_BAR_TOTAL_PX))
        .h(px(6.0))
        .rounded(px(3.0))
        .bg(rgb(pack(SURFACE_700)))
        .child(
            div()
                .h(px(6.0))
                .rounded(px(3.0))
                .bg(rgb(pack(BRAND_LIGHT)))
                .w(px(fill_px)),
        );

    let footer = div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(epoch_line);

    card_shell(
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(header)
            .child(progress_bar)
            .child(footer),
    )
}

/// Top line of an active-run card: job id + base/dataset on the left, a
/// "Stop" pill on the right.
fn active_card_header(row: &JobRow, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let id_for_stop = row.job_id.clone();
    let job_label = SharedString::from(short_id(&row.job_id));
    let base = SharedString::from(format!(
        "base · {}",
        row.base_model().unwrap_or("(unknown)"),
    ));
    let dataset = SharedString::from(format!(
        "dataset · {}",
        row.dataset_name().unwrap_or("(unknown)"),
    ));
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(job_label),
                )
                .child(meta_line_inline(&base))
                .child(meta_line_inline(&dataset)),
        )
        .child(pill_button(
            ElementId::Name(format!("training-stop::{}", row.job_id).into()),
            SharedString::from("Stop"),
            cx.listener(move |this: &mut TrainingPanel, _ev, _w, cx| {
                this.stop_job(id_for_stop.clone(), cx);
            }),
        ))
}

// ── Past list ────────────────────────────────────────────────────────

fn past_list(panel: &TrainingPanel, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let rows = panel.past_rows();
    if !panel.initial_load_done {
        return placeholder_card("Reading the trainer pipe…");
    }
    if rows.is_empty() {
        return placeholder_card(
            "No training runs yet.  Start one to teach Wylde something new.",
        );
    }
    let mut col = div().flex().flex_col().gap_3();
    for row in rows {
        col = col.child(past_card(panel, row, cx));
    }
    col
}

fn past_card(
    panel: &TrainingPanel,
    row: &JobRow,
    cx: &mut Context<TrainingPanel>,
) -> gpui::Div {
    let expanded = panel.expanded.as_deref() == Some(row.job_id.as_str());
    let id_for_resume = row.job_id.clone();
    let id_for_nav = row.base_model().map(|s| s.to_owned());

    let mut card = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(past_card_header(row, cx));

    if let Some(err) = &row.error {
        card = card.child(
            div()
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .rounded(px(4.0))
                .px_3()
                .py_2()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(err.clone())),
        );
    }

    if expanded {
        card = card.child(expand_body(panel, row, id_for_resume, id_for_nav, cx));
    }

    card_shell(card)
}

/// Clickable header of a past-run card: job id + base/dataset/metric
/// summary on the left, the status badge on the right.  Clicking it
/// toggles the expanded body.
fn past_card_header(row: &JobRow, cx: &mut Context<TrainingPanel>) -> Stateful<gpui::Div> {
    let id_for_click = row.job_id.clone();
    let status = row.status_enum();
    let status_label = SharedString::from(status.label().to_uppercase());
    let title = SharedString::from(short_id(&row.job_id));
    let base = SharedString::from(format!(
        "base · {}",
        row.base_model().unwrap_or("(unknown)"),
    ));
    let dataset = SharedString::from(format!(
        "dataset · {}",
        row.dataset_name().unwrap_or("(unknown)"),
    ));
    let metric_line = SharedString::from(format!(
        "final loss {} · epoch {} · duration {}",
        row.final_loss
            .map(|n| format!("{:.4}", n))
            .unwrap_or_else(|| "—".to_owned()),
        row.current_epoch
            .map(|n| format!("{:.2}", n))
            .unwrap_or_else(|| "—".to_owned()),
        row.duration_seconds
            .map(fmt_duration_seconds)
            .unwrap_or_else(|| "—".to_owned()),
    ));

    div()
        .id(ElementId::Name(format!("training-past-row::{}", row.job_id).into()))
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_2()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut TrainingPanel, _ev, _w, cx| {
                this.toggle_expand(id_for_click.clone(), cx);
            }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(title),
                )
                .child(meta_line_inline(&base))
                .child(meta_line_inline(&dataset))
                .child(meta_line_inline(&metric_line)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(status_color(status))))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(status_label),
        )
}

fn expand_body(
    panel: &TrainingPanel,
    row: &JobRow,
    id_for_resume: String,
    nav_model: Option<String>,
    cx: &mut Context<TrainingPanel>,
) -> gpui::Div {
    let losses = panel.loss_history.get(&row.job_id).cloned().unwrap_or_default();
    let fetch_err = panel.status_errors.get(&row.job_id).cloned();
    let resume_label = SharedString::from(if row.checkpoint_path.is_some() {
        "Resume from this run"
    } else {
        "Reuse config (no checkpoint)"
    });

    let mut col = div().flex().flex_col().gap_3();
    col = col.child(sparkline(&losses, cx));
    if let Some(e) = fetch_err {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BORDER_EMPHASIS)))
                .child(SharedString::from(format!("status fetch failed · {e}"))),
        );
    }
    let action_row = {
        let resume_button = pill_button(
            ElementId::Name(format!("training-resume::{}", row.job_id).into()),
            resume_label,
            cx.listener(move |this: &mut TrainingPanel, _ev, _w, cx| {
                // Look the row back up — `expand_body` borrows panel
                // immutably while building, so the resume closure can't
                // capture the row by reference.
                if let Some(target) = this
                    .jobs
                    .iter()
                    .find(|r| r.job_id == id_for_resume)
                    .cloned()
                {
                    this.resume_from(&target, cx);
                }
            }),
        );
        let mut bar = div().flex().flex_row().gap_2().child(resume_button);
        if let Some(model) = nav_model {
            let label = SharedString::from(format!("base · {model}  ›  open Models"));
            bar = bar.child(pill_button(
                ElementId::Name(format!("training-nav-model::{}", row.job_id).into()),
                label,
                cx.listener(|_this: &mut TrainingPanel, _ev, _w, _cx| {
                    let _ = wylde_gui_pipe::request_nav("core/models");
                }),
            ));
        }
        bar
    };
    col = col.child(action_row);
    col
}

fn sparkline(losses: &[LossPoint], _cx: &mut Context<TrainingPanel>) -> gpui::Div {
    if losses.is_empty() {
        return div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(
                "Loss history not available yet — waiting on the trainer status verb.",
            ));
    }
    if losses.len() == 1 {
        // Single point → textual fallback to honour the "documented
        // fallback" path called out in the slice spec.
        let p = &losses[0];
        return div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(format!(
                "Only one loss point recorded · step {}, loss {:.4} (sparkline needs ≥2 points)",
                p.step, p.loss
            )));
    }
    let buckets = SPARKLINE_MAX_BARS.min(losses.len());
    let downsampled = downsample_losses(losses, buckets);
    let normalised = normalise(&downsampled);
    let first = losses.first().map(|p| p.loss).unwrap_or_default();
    let last = losses.last().map(|p| p.loss).unwrap_or_default();
    let delta = last - first;
    let delta_label = SharedString::from(format!(
        "{} points · first {:.4} → last {:.4} ({}{:.4})",
        losses.len(),
        first,
        last,
        if delta < 0.0 { "" } else { "+" },
        delta,
    ));

    // Fixed-size bar histogram.  gpui at `b3d93d44` has no polyline
    // primitive on a plain `div`, and no `relative`-on-width support
    // either — see the lib.rs externality note.  We render each bar as
    // an absolute-width / absolute-height div instead.
    const SPARK_TOTAL_WIDTH_PX: f32 = 600.0;
    const SPARK_BAR_HEIGHT_PX: f32 = 56.0;
    const SPARK_GAP_PX: f32 = 2.0;
    let n_bars = normalised.len() as f32;
    let raw_bar_width = (SPARK_TOTAL_WIDTH_PX / n_bars) - SPARK_GAP_PX;
    let bar_width = raw_bar_width.max(2.0);
    let mut bars = div()
        .flex()
        .flex_row()
        .items_end()
        .gap(px(SPARK_GAP_PX))
        .h(px(SPARK_BAR_HEIGHT_PX + 16.0))
        .w(px(SPARK_TOTAL_WIDTH_PX + 16.0))
        .bg(rgb(pack(SURFACE_700)))
        .rounded(px(4.0))
        .p_2();
    for (i, n) in normalised.iter().enumerate() {
        let height_frac = (*n as f32).clamp(0.0, 1.0).max(0.05);
        let bar_px = (height_frac * SPARK_BAR_HEIGHT_PX).max(2.0);
        let bar = div()
            .id(ElementId::Name(format!("training-spark-bar::{i}").into()))
            .h(px(bar_px))
            .w(px(bar_width))
            .bg(rgb(pack(BRAND_LIGHT)))
            .rounded(px(1.0));
        bars = bars.child(bar);
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(bars)
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(delta_label),
        )
}

// ── Start form ───────────────────────────────────────────────────────

fn start_form(panel: &TrainingPanel, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let dataset_label = SharedString::from(if panel.config.dataset_name.is_empty() {
        "dataset · select…".to_owned()
    } else {
        format!("dataset · {}", panel.config.dataset_name)
    });

    let mut col = div().flex().flex_col().gap_3();
    col = col.child(form_row(
        "Base model",
        panel.base_model_input.clone().into_any_element(),
    ));
    col = col.child(form_row("Dataset", dataset_picker(panel, dataset_label, cx)));
    if panel.show_dataset_dropdown {
        col = col.child(dataset_dropdown(panel, cx));
    }
    col = col.child(form_row(
        "LoRA rank",
        panel.lora_rank_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "LoRA alpha",
        panel.lora_alpha_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "LoRA dropout",
        panel.lora_dropout_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "Batch size",
        panel.batch_size_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "Gradient accumulation",
        panel.grad_accum_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "Learning rate",
        panel.learning_rate_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "Epochs",
        panel.num_epochs_input.clone().into_any_element(),
    ));
    col = col.child(form_row(
        "Cutoff length",
        panel.cutoff_len_input.clone().into_any_element(),
    ));
    col = col.child(start_form_status(panel));

    let submit_label = SharedString::from(if panel.submit_busy {
        "Starting…"
    } else {
        "Start training"
    });
    let submit = pill_button(
        ElementId::Name("training-submit".into()),
        submit_label,
        cx.listener(|this: &mut TrainingPanel, _ev, _w, cx| {
            this.submit(cx);
        }),
    );
    col = col.child(submit);

    card_shell(col)
}

/// Trailing status lines under the start form: the "resume from
/// checkpoint" hint, the submit error box, and the "last started"
/// breadcrumb.  Each is conditional, so this returns an empty column
/// when the form is in its clean initial state.
fn start_form_status(panel: &TrainingPanel) -> gpui::Div {
    let checkpoint_line = if panel.config.resume_from_checkpoint.is_empty() {
        None
    } else {
        Some(SharedString::from(format!(
            "resume from · {}",
            panel.config.resume_from_checkpoint
        )))
    };

    let mut col = div().flex().flex_col().gap_3();
    if let Some(line) = checkpoint_line {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .child(line),
        );
    }
    if let Some(err) = &panel.submit_error {
        col = col.child(
            div()
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .rounded(px(4.0))
                .px_3()
                .py_2()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(err.clone())),
        );
    }
    if let Some(id) = &panel.last_started_id {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(format!(
                    "Last started · {} (will appear in the active strip on the next refresh)",
                    short_id(id),
                ))),
        );
    }
    col
}

fn dataset_picker(
    _panel: &TrainingPanel,
    label: SharedString,
    cx: &mut Context<TrainingPanel>,
) -> gpui::AnyElement {
    pill_button(
        ElementId::Name("training-dataset-toggle".into()),
        label,
        cx.listener(|this: &mut TrainingPanel, _ev, _w, cx| {
            this.toggle_dataset_dropdown(cx);
        }),
    )
    .into_any_element()
}

fn dataset_dropdown(panel: &TrainingPanel, cx: &mut Context<TrainingPanel>) -> gpui::Div {
    let mut col = dropdown_shell();
    if panel.datasets.is_empty() {
        col = col.child(empty_dropdown_row(
            "No datasets discovered.  Type a dataset name in the base-model field if you have one provisioned out-of-band.",
        ));
        return col;
    }
    for ds in &panel.datasets {
        let name_for_pick = ds.name.clone();
        let label = SharedString::from(if ds.sample_count > 0 {
            format!("{} ({} samples)", ds.display_name, ds.sample_count)
        } else {
            ds.display_name.clone()
        });
        col = col.child(dropdown_row(
            ElementId::Name(format!("training-dataset-pick::{}", ds.name).into()),
            label,
            cx.listener(move |this: &mut TrainingPanel, _ev, _w, cx| {
                this.pick_dataset(name_for_pick.clone(), cx);
            }),
        ));
    }
    col
}

fn form_row(label: &str, body: gpui::AnyElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(160.0))
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(label.to_owned())),
        )
        .child(div().flex_1().child(body))
}

// ── Shared widgets (mirror Images / Devices) ─────────────────────────

fn pill_button<F>(
    id: ElementId,
    label: SharedString,
    listener: F,
) -> Stateful<gpui::Div>
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

fn meta_line_inline(text: &SharedString) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(text.clone())
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

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

pub(crate) fn short_id(s: &str) -> String {
    if s.len() <= 12 {
        s.to_owned()
    } else {
        format!("{}…", &s[..12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(step: u64, loss: f64) -> LossPoint {
        LossPoint {
            step,
            loss,
            epoch: step as f64 / 100.0,
            lr: 2e-4,
        }
    }

    #[test]
    fn downsample_handles_short_input() {
        let losses = vec![point(1, 2.0), point(2, 1.5)];
        let out = downsample_losses(&losses, 4);
        assert_eq!(out.len(), 2);
        assert_eq!(out, vec![2.0, 1.5]);
    }

    #[test]
    fn downsample_handles_empty() {
        let losses: Vec<LossPoint> = Vec::new();
        assert!(downsample_losses(&losses, 8).is_empty());
        assert!(downsample_losses(&[point(1, 1.0)], 0).is_empty());
    }

    #[test]
    fn downsample_averages_buckets() {
        // 8 points → 4 buckets → each bucket is 2 points averaged.
        let losses: Vec<LossPoint> = (0..8).map(|i| point(i, (i + 1) as f64)).collect();
        let out = downsample_losses(&losses, 4);
        assert_eq!(out.len(), 4);
        // Means of (1,2), (3,4), (5,6), (7,8) = 1.5, 3.5, 5.5, 7.5.
        assert!((out[0] - 1.5).abs() < 1e-9);
        assert!((out[3] - 7.5).abs() < 1e-9);
    }

    #[test]
    fn normalise_flat_series_returns_half_height() {
        let v = vec![1.0, 1.0, 1.0];
        let norm = normalise(&v);
        assert_eq!(norm.len(), 3);
        for n in norm {
            assert!((n - 0.5).abs() < 1e-9);
        }
    }

    #[test]
    fn normalise_scales_to_unit_interval() {
        let v = vec![0.0, 1.0, 0.5];
        let norm = normalise(&v);
        assert!((norm[0] - 0.0).abs() < 1e-9);
        assert!((norm[1] - 1.0).abs() < 1e-9);
        assert!((norm[2] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn normalise_handles_empty() {
        let v: Vec<f64> = Vec::new();
        assert!(normalise(&v).is_empty());
    }

    #[test]
    fn fmt_duration_seconds_pretty() {
        assert_eq!(fmt_duration_seconds(0), "0s");
        assert_eq!(fmt_duration_seconds(45), "45s");
        assert_eq!(fmt_duration_seconds(125), "2m 5s");
        assert_eq!(fmt_duration_seconds(3 * 3600 + 30 * 60 + 12), "3h 30m");
    }

    #[test]
    fn fmt_eta_handles_zero() {
        assert_eq!(fmt_eta_seconds(0), "—");
        assert_eq!(fmt_eta_seconds(90), "1m 30s");
    }

    #[test]
    fn format_float_round_trips_common_values() {
        assert_eq!(format_float(0.0), "0");
        assert_eq!(format_float(3.0), "3.0");
        assert_eq!(format_float(0.05), "0.05");
        // Very small numbers stay in scientific form.
        assert!(format_float(2e-4).contains('e'));
        assert!(format_float(1e-10).contains('e'));
    }

    #[test]
    fn short_id_truncates_long_strings() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("0123456789ab"), "0123456789ab");
        let long = "0123456789abcdef";
        let out = short_id(long);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("0123456789ab"));
    }

    #[test]
    fn status_color_maps_known_statuses() {
        // Pure-structural check.  We compare the full `Rgba` (R + G + B
        // + A) rather than the packed RGB triplet because
        // `BORDER_EMPHASIS` shares its RGB with `BRAND` and only differs
        // in alpha — `pack` would conflate them.
        fn key(c: gpui::Rgba) -> (u32, u32) {
            let rgb = pack(c);
            let a = (c.a.clamp(0.0, 1.0) * 255.0).round() as u32;
            (rgb, a)
        }
        let run = key(status_color(JobStatus::Running));
        let fail = key(status_color(JobStatus::Failed));
        let done = key(status_color(JobStatus::Completed));
        let queued = key(status_color(JobStatus::Queued));
        let stopped = key(status_color(JobStatus::Stopped));
        assert!(run != fail, "Running and Failed must look different");
        assert!(done != fail, "Completed and Failed must look different");
        assert!(queued != done, "Queued and Completed must look different");
        assert!(stopped != run, "Stopped and Running must look different");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<TrainingPanel>();
    }

    // ── Pure projections: active vs past partition ────────────────────

    fn job(id: &str, status: &str) -> JobRow {
        JobRow {
            job_id: id.into(),
            status: status.into(),
            ..JobRow::default()
        }
    }

    #[test]
    fn active_past_partition_matches_status() {
        let jobs = [
            job("a", "running"),
            job("b", "queued"),
            job("c", "completed"),
            job("d", "failed"),
            job("e", "stopped"),
        ];
        let active: Vec<&JobRow> = jobs.iter().filter(|r| r.status_enum().is_active()).collect();
        let past: Vec<&JobRow> = jobs.iter().filter(|r| r.status_enum().is_terminal()).collect();
        assert_eq!(active.len(), 2);
        assert_eq!(past.len(), 3);
    }
}
