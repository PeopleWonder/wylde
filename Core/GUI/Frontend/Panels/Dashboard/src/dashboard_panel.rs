//! Dashboard panel View — at-a-glance state of the Wylde stack.
//!
//! Layout (top → bottom):
//!
//!   * Header — title + last-refreshed indicator + manual Refresh.
//!   * Service-health strip — one dot per service in
//!     `MONITORED_SERVICES`.  Click → request_nav("core/tools") so the
//!     Tools panel can show details.
//!   * Hardware card — CPU, RAM, GPU(s), NPU, disk free.  Degrades to
//!     "broker offline — last known: …" rather than disappearing.
//!   * Active model card — first row of `ollama.list_loaded`.  Empty
//!     when nothing is resident.
//!   * Recent activity card — N rows of recently-touched long-term
//!     memories with cross-panel nav into Memory.
//!
//! Auto-refresh: a long-lived task wakes every 5 s and re-fetches
//! everything, then writes the timestamp into `last_refreshed_at`.
//! The header shows "Updated 3s ago" so the user knows the dot
//! colours are fresh.

use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, FontWeight,
    IntoElement, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_LIGHT, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    probe_service, read_hardware_card, read_loaded_models, read_recent_memories, HardwareCard,
    HealthStatus, LoadedModel, RecentMemory, ServiceHealth, MONITORED_SERVICES,
};

/// Polling interval for the auto-refresh loop.  Matches the Svelte
/// alpha's Dashboard timer.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Max recent-activity rows.  Picked to fit a typical viewport without
/// scroll, in line with the Svelte alpha.
const RECENT_LIMIT: usize = 5;

/// Body preview length for a recent-memory row.
const RECENT_PREVIEW_CHARS: usize = 96;

pub struct DashboardPanel {
    pub service_health: Vec<(String, ServiceHealth)>,
    pub hardware: HardwareCard,
    /// `true` once we've successfully read the broker.  Lets the
    /// hardware card flip from "(loading…)" to "last known: …" if a
    /// subsequent refresh sees the broker go down.
    pub hardware_ever_read: bool,
    pub loaded_models: Vec<LoadedModel>,
    pub recent_memories: Vec<RecentMemory>,
    pub last_refreshed_at: Option<Instant>,
    pub refresh_generation: u64,
    pub initial_load_done: bool,
}

impl DashboardPanel {
    pub fn new() -> Self {
        let service_health = MONITORED_SERVICES
            .iter()
            .map(|s| ((*s).to_owned(), ServiceHealth::plain(HealthStatus::Unknown)))
            .collect();
        Self {
            service_health,
            hardware: HardwareCard::default(),
            hardware_ever_read: false,
            loaded_models: Vec::new(),
            recent_memories: Vec::new(),
            last_refreshed_at: None,
            refresh_generation: 0,
            initial_load_done: false,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            // Kick off the auto-refresh loop straight away.  The loop's
            // first iteration fires synchronously (no leading sleep) so
            // the user sees data within one round-trip rather than
            // after a 5 s gap.
            Self::spawn_refresh_loop(cx);
            panel
        })
        .into()
    }

    /// Spawn the long-lived refresh loop.  Each iteration fires every
    /// IPC in parallel via `cx.spawn` so a slow read on one card
    /// doesn't stall the others.
    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                Self::refresh_once(this.clone(), app_cx).await;
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

    /// One refresh cycle.  All four IPCs fire concurrently via
    /// `tokio::join!` so the user sees the strip / cards advance
    /// together rather than card-by-card.
    pub async fn refresh_once(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        // Bump the generation so a "Refresh" button click that lands
        // mid-sleep can compare against this.  Used for the visible
        // "Updated Xs ago" pill.
        let _ = this.update(app_cx, |panel, _| {
            panel.refresh_generation = panel.refresh_generation.wrapping_add(1);
        });

        let health_fut = async {
            let mut out: Vec<(String, ServiceHealth)> = Vec::new();
            for svc in MONITORED_SERVICES {
                let health = probe_service(svc).await;
                out.push(((*svc).to_owned(), health));
            }
            out
        };
        let hardware_fut = read_hardware_card();
        let loaded_fut = read_loaded_models();
        let recent_fut = read_recent_memories(RECENT_LIMIT);

        let (health, hardware, loaded, recent) =
            tokio::join!(health_fut, hardware_fut, loaded_fut, recent_fut);

        let _ = this.update(app_cx, |panel, cx| {
            panel.service_health = health;
            match hardware {
                Ok(hw) if !hw.is_unknown() => {
                    panel.hardware = hw;
                    panel.hardware_ever_read = true;
                }
                Ok(_) => {
                    // Broker answered but the snapshot is empty —
                    // treat as if the call failed.  Keep the prior
                    // snapshot so the user sees "last known: …".
                }
                Err(_) => { /* keep prior snapshot */ }
            }
            if let Ok(models) = loaded {
                panel.loaded_models = models;
            }
            if let Ok(rows) = recent {
                panel.recent_memories = rows;
            }
            panel.last_refreshed_at = Some(Instant::now());
            panel.initial_load_done = true;
            cx.notify();
        });
    }

    /// Manual-refresh handler bound to the header button.
    pub fn spawn_manual_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::refresh_once(this.clone(), app_cx).await;
        })
        .detach();
    }

    /// Stop a running service from the console — the deliberate operator action
    /// (decision 7: start/restart/stop from anywhere in the GUI). Drives the
    /// shared `service.stop` helper, then re-probes *just that service* so its
    /// chip flips to the stopped (red) state without waiting for the next 5 s
    /// refresh tick.
    ///
    /// There is deliberately **no error banner**: the Dashboard degrades
    /// per-card by design (it has no panel-level `error` field), so the outcome
    /// is read from the re-probe, not a wall of text. A stop that fails leaves
    /// the service running and the re-probe shows it still green — the honest
    /// signal, and the same "reflect reality" contract the auto-refresh loop
    /// already keeps. `wylde-lifecycle` is never offered a Stop button (see
    /// [`offers_stop`]), so this can't be asked to stop the daemon serving it.
    pub fn spawn_stop_service(&mut self, name: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Fire the stop. The outcome surfaces through the re-probe below,
            // not a banner — see the doc comment.
            let _ = wylde_gui_pipe::stop_service(&name).await;
            let health = probe_service(&name).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Some(entry) = panel.service_health.iter_mut().find(|(n, _)| n == &name) {
                    entry.1 = health;
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Default for DashboardPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = header_row(self, cx);
        // `w_full` pins a definite width on the column.  Without it the
        // column is shrink-to-fit (flex-basis auto), so the wrapping
        // service-health strip below has no stable container width to
        // wrap against: taffy's content-sizing pass measures it single-
        // line (short) while the layout pass measures it wrapped (tall),
        // and the two disagree every re-render — the panel ping-pongs
        // between "big and tall" and the correct height.  Pinning the
        // width makes the wrap deterministic and kills the oscillation.
        let mut column = div()
            .w_full()
            .max_w(px(860.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        column = column.child(section_title("Service health"));
        column = column.child(service_health_strip(self, cx));

        column = column.child(section_title("Hardware"));
        column = column.child(hardware_card(self));

        column = column.child(section_title("Active model"));
        column = column.child(active_model_card(self, cx));

        column = column.child(section_title("Recent activity"));
        column = column.child(recent_activity_card(self, cx));

        // Outer container is the scroll viewport: a definite-height
        // (`size_full`) flex column with `overflow_y_scroll`, so when the
        // stacked sections exceed the window the user can wheel-scroll to
        // the rest.  Mirrors the Chat panel's message-log idiom (`.id()`
        // + `.overflow_y_scroll()`).
        div()
            .id("dashboard-scroll")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(pack(SURFACE_900)))
            .overflow_y_scroll()
            .p_6()
            .child(column)
    }
}

fn header_row(panel: &DashboardPanel, cx: &mut Context<DashboardPanel>) -> gpui::Div {
    let refreshed = SharedString::from(refreshed_label(panel.last_refreshed_at));
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
                        .child(SharedString::from("Dashboard")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Live state of every Wylde service, your hardware envelope, \
                             what the broker is holding, and the memory the assistant just \
                             touched.",
                        )),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(refreshed),
                )
                .child(refresh_button(cx)),
        )
}

fn refresh_button(cx: &mut Context<DashboardPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("dashboard-refresh".into());
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
            cx.listener(|_this: &mut DashboardPanel, _event, _window, cx| {
                DashboardPanel::spawn_manual_refresh(cx);
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

fn service_health_strip(panel: &DashboardPanel, cx: &mut Context<DashboardPanel>) -> gpui::Div {
    let mut row = div().flex().flex_row().flex_wrap().gap_2();
    for (name, health) in &panel.service_health {
        row = row.child(service_chip(name, health, cx));
    }
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_3()
        .child(row)
}

/// Whether a service's chip offers a one-click Stop (decision 7, console side).
///
/// Two conditions, both about not painting a dead button:
///   * the service must actually be *running* — `Healthy` or `Degraded`. A
///     `Unhealthy`/`Unknown` service has nothing to stop.
///   * it must not be `wylde-lifecycle` itself. That daemon serves the
///     `service.stop` request and isn't in its own manageable set, so a stop
///     is an idempotent no-op on the backend — offering the button would be a
///     control that does nothing.
fn offers_stop(name: &str, status: HealthStatus) -> bool {
    name != "wylde-lifecycle" && matches!(status, HealthStatus::Healthy | HealthStatus::Degraded)
}

fn service_chip(
    name: &str,
    health: &ServiceHealth,
    cx: &mut Context<DashboardPanel>,
) -> Stateful<gpui::Div> {
    let label = SharedString::from(short_service_name(name));
    let colour = status_colour(health.status);
    let id: ElementId = ElementId::Name(format!("dashboard-svc::{name}").into());
    let mut chip = div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_900)))
        .cursor_pointer()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut DashboardPanel, _ev, _window, _cx| {
                // Service rows defer detail to the Tools panel.  If
                // no Shell is wired up (unit tests) the request is a
                // no-op, which is the right behaviour.
                let _ = wylde_gui_pipe::request_nav("core/tools");
            }),
        )
        .child(
            div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded(px(999.0))
                .bg(rgb(pack(colour))),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(label),
        );
    // One-click Stop for a running service (decision 7). Nested inside the
    // chip, so it needs `stop_propagation` in its handler — otherwise the
    // chip's own click (navigate to Tools) would also fire. Only rendered
    // where a stop is a live action (see `offers_stop`).
    if offers_stop(name, health.status) {
        let service = name.to_owned();
        chip = chip.child(
            div()
                .id(ElementId::Name(format!("dashboard-stop::{name}").into()))
                .px_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("Stop"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this: &mut DashboardPanel, _ev, _w, cx| {
                        cx.stop_propagation();
                        this.spawn_stop_service(service.clone(), cx);
                    }),
                ),
        );
    }
    // Degraded (and any future detail-bearing) tile gets a hover tooltip
    // explaining the specific degradation — "Ollama daemon unreachable…"
    // or "Slow response (>2s)".
    if let Some(detail) = &health.detail {
        let text = SharedString::from(detail.clone());
        chip = chip.tooltip(move |_window, cx| {
            let text = text.clone();
            cx.new(|_| ChipTooltip { text }).into()
        });
    }
    chip
}

/// Minimal hover tooltip for a degraded service chip. gpui core ships no
/// ready-made tooltip view (that lives in zed's `ui` crate, which Wylde
/// doesn't depend on), so this is a tiny styled bubble of its own.
struct ChipTooltip {
    text: SharedString,
}

impl Render for ChipTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .bg(rgb(pack(SURFACE_800)))
            .border_1()
            .border_color(rgb(pack(BORDER_DEFAULT)))
            .rounded(px(4.0))
            .px_2()
            .py_1()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(self.text.clone())
    }
}

fn hardware_card(panel: &DashboardPanel) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Probing hardware…");
    }
    if panel.hardware.is_unknown() {
        return placeholder_card(
            "VRAM broker offline — no hardware snapshot yet.  The card will appear once the \
             broker comes up.",
        );
    }
    let mut col = div().flex().flex_col().gap_2();
    if panel.hardware.is_unknown() && panel.hardware_ever_read {
        col = col.child(stale_label("Broker offline — showing last known snapshot."));
    }

    let cpu_line = SharedString::from(format!(
        "{} · {} cores",
        shorten_cpu(&panel.hardware.cpu_brand),
        panel.hardware.cpu_cores.max(1),
    ));
    let ram_line = SharedString::from(format!(
        "RAM · {} free / {} total",
        humanize_bytes(panel.hardware.ram_available_bytes),
        humanize_bytes(panel.hardware.ram_total_bytes),
    ));
    let mut accel_parts: Vec<String> = Vec::new();
    if panel.hardware.nvidia_count > 0 {
        accel_parts.push(format!(
            "NVIDIA × {} ({} used / {} VRAM)",
            panel.hardware.nvidia_count,
            humanize_bytes(panel.hardware.nvidia_vram_used_bytes),
            humanize_bytes(panel.hardware.nvidia_vram_bytes),
        ));
    }
    if panel.hardware.intel_count > 0 {
        accel_parts.push(format!("Intel × {}", panel.hardware.intel_count));
    }
    if panel.hardware.amd_count > 0 {
        accel_parts.push(format!("AMD × {}", panel.hardware.amd_count));
    }
    if panel.hardware.has_npu {
        accel_parts.push("NPU".to_owned());
    }
    let accel_line = SharedString::from(if accel_parts.is_empty() {
        "GPUs · none detected (CPU inference only)".to_owned()
    } else {
        format!("Accelerators · {}", accel_parts.join(" · "))
    });
    let disk_line = SharedString::from(format!(
        "Disk · {} free",
        humanize_bytes(panel.hardware.free_disk_bytes),
    ));

    col = col
        .child(card_line(cpu_line))
        .child(card_line(ram_line))
        .child(card_line(accel_line))
        .child(card_line(disk_line));

    card_shell(col)
}

fn active_model_card(panel: &DashboardPanel, cx: &mut Context<DashboardPanel>) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Probing Ollama…");
    }
    if panel.loaded_models.is_empty() {
        return placeholder_card_clickable(
            "No model in VRAM right now.  Open the Models panel to pull or load one.",
            "core/models",
            cx,
        );
    }
    let primary = &panel.loaded_models[0];
    let extra = if panel.loaded_models.len() > 1 {
        format!(" (+{} more resident)", panel.loaded_models.len() - 1)
    } else {
        String::new()
    };
    let label = SharedString::from(format!("{}{}", primary.name, extra));
    let meta = if primary.size_vram_bytes > 0 {
        format!("VRAM · {}", humanize_bytes(primary.size_vram_bytes))
    } else {
        "VRAM · unknown".to_owned()
    };
    let expires_line = if primary.expires_at.is_empty() {
        "Expires · n/a".to_owned()
    } else {
        format!("Expires · {}", primary.expires_at)
    };

    card_shell(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .child(label),
            )
            .child(card_line(SharedString::from(meta)))
            .child(card_line(SharedString::from(expires_line))),
    )
}

fn recent_activity_card(panel: &DashboardPanel, cx: &mut Context<DashboardPanel>) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Loading recent activity…");
    }
    if panel.recent_memories.is_empty() {
        return placeholder_card_clickable(
            "No recent activity yet.  Open the Chat panel to get started.",
            "core/chat",
            cx,
        );
    }
    let mut col = div().flex().flex_col().gap_2();
    for r in &panel.recent_memories {
        col = col.child(recent_row(r, cx));
    }
    card_shell(col)
}

fn recent_row(r: &RecentMemory, cx: &mut Context<DashboardPanel>) -> Stateful<gpui::Div> {
    let preview = SharedString::from(preview_body(&r.body));
    let meta = SharedString::from(format!(
        "★ {} · {} · {}",
        r.importance,
        if r.source.is_empty() {
            "(no source)".to_owned()
        } else {
            r.source.clone()
        },
        recency_label(r.last_used_at, r.created_at),
    ));
    let id: ElementId = ElementId::Name(format!("dashboard-recent::{}", r.id).into());
    div()
        .id(id)
        .border_b_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pb_2()
        .cursor_pointer()
        .flex()
        .flex_col()
        .gap_1()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut DashboardPanel, _ev, _window, _cx| {
                let _ = wylde_gui_pipe::request_nav("core/memory");
            }),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(preview),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(meta),
        )
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

fn card_line(label: SharedString) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(label)
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

fn placeholder_card_clickable(
    text: &str,
    nav_key: &'static str,
    cx: &mut Context<DashboardPanel>,
) -> gpui::Div {
    let body = div()
        .id(ElementId::Name(
            format!("dashboard-empty::{nav_key}").into(),
        ))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |_this: &mut DashboardPanel, _ev, _window, _cx| {
                let _ = wylde_gui_pipe::request_nav(nav_key);
            }),
        )
        .child(SharedString::from(text.to_owned()));
    // `card_shell` wraps a `Div`; the clickable body is `Stateful<Div>`.
    // Inline the wrapper here so the type cast isn't required.
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .child(body)
}

fn stale_label(text: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND_LIGHT)))
        .child(SharedString::from(text.to_owned()))
}

// ── Pure projections (testable) ──────────────────────────────────────

pub(crate) fn short_service_name(s: &str) -> String {
    s.strip_prefix("wylde-").unwrap_or(s).to_owned()
}

pub(crate) fn status_colour(status: HealthStatus) -> gpui::Rgba {
    match status {
        HealthStatus::Healthy => BRAND,
        // Amber for "pipe up but degraded" — mirrors the red literal
        // pattern below rather than a theme token (the theme has no
        // warning colour yet).
        HealthStatus::Degraded => gpui::rgb(0xf5_9e_0b),
        HealthStatus::Unhealthy => gpui::rgb(0xef_4444),
        HealthStatus::Unknown => TEXT_MUTED,
    }
}

pub(crate) fn refreshed_label(when: Option<Instant>) -> String {
    let Some(t) = when else {
        return "Updated never".to_owned();
    };
    let secs = t.elapsed().as_secs();
    match secs {
        0 => "Updated just now".to_owned(),
        1 => "Updated 1s ago".to_owned(),
        s if s < 60 => format!("Updated {s}s ago"),
        s if s < 3600 => format!("Updated {}m ago", s / 60),
        s => format!("Updated {}h ago", s / 3600),
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

pub(crate) fn preview_body(body: &str) -> String {
    let mut idx = body.len();
    for (count, (offset, _)) in body.char_indices().enumerate() {
        if count >= RECENT_PREVIEW_CHARS {
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

pub(crate) fn recency_label(last_used: f64, created: f64) -> String {
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

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<DashboardPanel>();
    }

    #[test]
    fn new_with_defaults_marks_every_service_unknown() {
        let p = DashboardPanel::new();
        assert_eq!(p.service_health.len(), MONITORED_SERVICES.len());
        for (_, health) in &p.service_health {
            assert_eq!(health.status, HealthStatus::Unknown);
            assert!(health.detail.is_none());
        }
        assert!(!p.initial_load_done);
        assert!(p.recent_memories.is_empty());
    }

    #[test]
    fn refresh_interval_is_five_seconds() {
        // Frozen so a future tweak surfaces in code review.
        assert_eq!(REFRESH_INTERVAL.as_secs(), 5);
    }

    #[test]
    fn refreshed_label_handles_every_bucket() {
        assert_eq!(refreshed_label(None), "Updated never");
        // Just-instantiated `Instant` reports 0 elapsed — we accept
        // either "just now" or "1s ago" depending on test timing.
        let l = refreshed_label(Some(Instant::now()));
        assert!(l == "Updated just now" || l == "Updated 1s ago", "got {l}",);
    }

    #[test]
    fn humanize_bytes_picks_units_per_magnitude() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(8 * 1024 * 1024 * 1024), "8.0 GB");
        assert_eq!(humanize_bytes(512 * 1024), "512 KB");
    }

    #[test]
    fn short_service_name_drops_wylde_prefix() {
        assert_eq!(short_service_name("wylde-harness"), "harness");
        assert_eq!(short_service_name("standalone"), "standalone");
    }

    #[test]
    fn offers_stop_only_for_running_non_daemon_services() {
        // A running service the daemon manages: Stop is a live action.
        assert!(offers_stop("wylde-gateway", HealthStatus::Healthy));
        assert!(offers_stop("wylde-ollama", HealthStatus::Degraded));
        // Nothing to stop when it isn't up.
        assert!(!offers_stop("wylde-gateway", HealthStatus::Unhealthy));
        assert!(!offers_stop("wylde-gateway", HealthStatus::Unknown));
        // Never the daemon serving the request — a backend no-op, so the
        // button would do nothing.
        assert!(!offers_stop("wylde-lifecycle", HealthStatus::Healthy));
    }

    #[test]
    fn status_colour_distinguishes_each_bucket() {
        let healthy = status_colour(HealthStatus::Healthy);
        let degraded = status_colour(HealthStatus::Degraded);
        let unhealthy = status_colour(HealthStatus::Unhealthy);
        let unknown = status_colour(HealthStatus::Unknown);
        // All four buckets must be visually distinct.
        let packed = [
            pack(healthy),
            pack(degraded),
            pack(unhealthy),
            pack(unknown),
        ];
        for i in 0..packed.len() {
            for j in (i + 1)..packed.len() {
                assert_ne!(packed[i], packed[j], "colours {i} and {j} collide");
            }
        }
    }

    #[test]
    fn preview_body_clips_and_ellipses_long_text() {
        let long = "x".repeat(500);
        let p = preview_body(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= RECENT_PREVIEW_CHARS + 1);
        assert_eq!(preview_body("short"), "short");
    }

    #[test]
    fn shorten_cpu_strips_frequency_and_handles_empty() {
        assert_eq!(shorten_cpu("Intel @ 3.0GHz"), "Intel");
        assert_eq!(shorten_cpu("  "), "CPU");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn recency_label_handles_zero_input() {
        assert_eq!(recency_label(0.0, 0.0), "Unknown");
        let recent = recency_label(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
            0.0,
        );
        assert!(recent == "Just now" || recent.ends_with(" ago"));
    }

    #[test]
    fn nav_request_does_not_panic_without_shell() {
        // The bus is process-wide; we can't reset the OnceLock, so
        // this assertion is just "the call returns" — which is the
        // contract a panel relies on when the Shell isn't wired up.
        let _ = wylde_gui_pipe::request_nav("core/tools");
    }
}
