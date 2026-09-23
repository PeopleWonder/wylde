//! Workflows panel View.
//!
//! State (held inline on the View):
//!   * `state`   — whether the n8n engine answered the last `n8n.health`.
//!   * `url`     — the editor URL `wylde-n8n` reported (the loopback engine).
//!   * `reloads` — how many times the user asked for a fresh editor load.
//!
//! While the engine is [`EngineState::Ready`] the panel keeps an embed request
//! on [`embed_bus`] and paints a `canvas` in its content region that reports
//! the region's window-space bounds, which is where the Shell places the
//! WebView. Any other state withdraws the request, so the Shell drops the
//! WebView and the region shows why it is empty.

use std::time::Duration;

use gpui::{
    canvas, div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Bounds, Context,
    FontWeight, IntoElement, Pixels, Render, SharedString, Stateful, Window,
};
use serde_json::{json, Value};
use wylde_gui_controls::control;
use wylde_gui_pipe::embed_bus::{self, EmbedRect};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND_LIGHT, DANGER, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY, WARNING,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

/// The manifest id — the key this panel's embed request is filed under.
pub const PANEL_ID: &str = "n8n";

/// The service fronting the engine, on `\\.\pipe\wylde-n8n`.
pub const SVC_N8N: &str = "wylde-n8n";

/// The shared-auth hook the Shell calls before creating the WebView: its
/// `init_js` logs the editor in as the Wylde-owned n8n owner.
pub const AUTH_BOOTSTRAP: &str = "wylde-n8n:n8n.editor_bootstrap";

/// The editor URL when `wylde-n8n` does not report one — n8n's own default.
pub const DEFAULT_URL: &str = "http://127.0.0.1:5678";

/// How often the panel re-checks the engine, so an engine that comes up (or
/// goes away) after the panel opened is reflected without a Reload click.
pub const PANEL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What the last engine check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// No answer yet.
    Checking,
    /// The engine answers; the editor is hosted in the content region.
    Ready,
    /// The engine (or `wylde-n8n`) is not answering — why, in words.
    Unreachable(String),
}

/// Root Workflows panel.
pub struct N8nPanel {
    pub state: EngineState,
    pub url: String,
    pub reloads: u64,
}

impl N8nPanel {
    pub fn new() -> Self {
        Self {
            state: EngineState::Checking,
            url: DEFAULT_URL.to_owned(),
            reloads: 0,
        }
    }

    /// Factory entry — matches the manifest `factory:` string
    /// (`wylde_panel_n8n::N8nPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            // The loop's first iteration fires synchronously, so this also
            // covers the initial check.
            Self::spawn_refresh_loop(cx);
            panel
        })
        .into()
    }

    /// Long-lived poll: re-check the engine every [`PANEL_POLL_INTERVAL`] for
    /// the panel's lifetime. Same shape as the Tools panel's loop — leading
    /// iteration with no sleep, gpui's timer, exit once the entity is gone.
    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            let alive = this
                .update(app_cx, |_panel, cx| Self::spawn_refresh(cx))
                .is_ok();
            if !alive {
                return;
            }
            app_cx
                .background_executor()
                .timer(PANEL_POLL_INTERVAL)
                .await;
        })
        .detach();
    }

    /// One `n8n.health` check, applied to the panel when it lands.
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = wylde_gui_pipe::call(
                SVC_N8N,
                "POST",
                "/__action__",
                Some(json!({ "action": "n8n.health", "payload": {} })),
            )
            .await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.apply_health(outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fold an `n8n.health` reply (`{reachable, url, auth_configured}`) into
    /// the panel, and file or withdraw the embed request to match.
    pub fn apply_health(&mut self, outcome: Result<Value, String>) {
        match outcome {
            Ok(reply) => {
                if let Some(url) = reply
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                {
                    self.url = url.to_owned();
                }
                if reply.get("reachable").and_then(Value::as_bool) == Some(true) {
                    self.state = EngineState::Ready;
                    embed_bus::request_embed(PANEL_ID, &self.url, Some(AUTH_BOOTSTRAP));
                } else {
                    self.state = EngineState::Unreachable(format!(
                        "The n8n engine is not answering at {}. It starts with Wylde \
                         when Node.js and n8n are installed (npm install -g n8n).",
                        self.url
                    ));
                    embed_bus::withdraw_embed(PANEL_ID);
                }
            }
            Err(e) => {
                self.state = EngineState::Unreachable(format!(
                    "The workflow service (wylde-n8n) is not responding: {e}"
                ));
                embed_bus::withdraw_embed(PANEL_ID);
            }
        }
    }

    /// Reload: a fresh editor load (and shared-auth fetch) plus a re-check.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reloads += 1;
        embed_bus::reload_embed(PANEL_ID);
        Self::spawn_refresh(cx);
        cx.notify();
    }
}

impl Default for N8nPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for N8nPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host_error = embed_bus::embed_request(PANEL_ID).and_then(|r| r.host_error);
        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(header_row(self, cx))
            .child(content_region(&self.state, host_error))
    }
}

fn header_row(panel: &N8nPanel, cx: &mut Context<N8nPanel>) -> gpui::Div {
    let (status, status_color) = match &panel.state {
        EngineState::Checking => ("Checking the n8n engine…".to_owned(), TEXT_MUTED),
        EngineState::Ready => (format!("Running at {}", panel.url), BRAND_LIGHT),
        EngineState::Unreachable(_) => ("Not running".to_owned(), WARNING),
    };
    let mut actions = div().flex().flex_row().gap_2().child(reload_button(cx));
    if panel.state == EngineState::Ready {
        actions = actions.child(open_button(cx));
    }
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
                        .child(SharedString::from("Workflows")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(status_color)))
                        .child(SharedString::from(status)),
                ),
        )
        .child(actions)
}

fn reload_button(cx: &mut Context<N8nPanel>) -> Stateful<gpui::Div> {
    button_style(control(div(), "n8n-reload"), "Reload").on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(|this: &mut N8nPanel, _event, _window, cx| this.reload(cx)),
    )
}

fn open_button(cx: &mut Context<N8nPanel>) -> Stateful<gpui::Div> {
    button_style(control(div(), "n8n-open-browser"), "Open in browser").on_mouse_down(
        gpui::MouseButton::Left,
        cx.listener(|this: &mut N8nPanel, _event, _window, _cx| {
            wylde_gui_pipe::open_url(&this.url);
        }),
    )
}

/// The shared look of a header button.
fn button_style(el: Stateful<gpui::Div>, label: &'static str) -> Stateful<gpui::Div> {
    el.px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(label))
}

/// The region the editor is hosted over. Ready: a `canvas` reports the
/// region's bounds to the Shell (the WebView covers the placeholder once
/// mounted). Otherwise: why there is no editor.
fn content_region(state: &EngineState, host_error: Option<String>) -> gpui::Div {
    let (message, color) = match (state, &host_error) {
        (EngineState::Ready, Some(err)) => {
            (format!("The editor could not be shown: {err}"), DANGER)
        }
        (EngineState::Ready, None) => ("Opening the n8n editor…".to_owned(), TEXT_MUTED),
        (EngineState::Checking, _) => ("Checking the n8n engine…".to_owned(), TEXT_MUTED),
        (EngineState::Unreachable(why), _) => (why.clone(), TEXT_SECONDARY),
    };
    let mut region = div()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .max_w(px(520.0))
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(color)))
                .child(SharedString::from(message)),
        );
    if *state == EngineState::Ready {
        region = region.child(
            canvas(
                |bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App| {
                    let rect = EmbedRect {
                        x: f64::from(f32::from(bounds.origin.x)),
                        y: f64::from(f32::from(bounds.origin.y)),
                        width: f64::from(f32::from(bounds.size.width)),
                        height: f64::from(f32::from(bounds.size.height)),
                    };
                    // One more frame when the region moved, so the Shell
                    // (which reads the rect in its own render) catches up
                    // without waiting for unrelated input.
                    if embed_bus::set_embed_rect(PANEL_ID, rect) {
                        window.request_animation_frame();
                    }
                },
                |_bounds, (), _window, _cx| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
    }
    region
}

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts. Same shim
/// every panel keeps locally.
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
    fn a_reachable_engine_is_ready_at_the_reported_url() {
        let mut p = N8nPanel::new();
        p.apply_health(Ok(
            json!({ "reachable": true, "url": "http://127.0.0.1:5999" }),
        ));
        assert_eq!(p.state, EngineState::Ready);
        assert_eq!(p.url, "http://127.0.0.1:5999");
    }

    #[test]
    fn an_unreachable_engine_says_where_it_looked() {
        let mut p = N8nPanel::new();
        p.apply_health(Ok(
            json!({ "reachable": false, "url": "http://127.0.0.1:5678" }),
        ));
        match p.state {
            EngineState::Unreachable(why) => assert!(why.contains("127.0.0.1:5678"), "{why}"),
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_silent_service_is_unreachable_not_stuck_checking() {
        let mut p = N8nPanel::new();
        p.apply_health(Err("pipe_unavailable".into()));
        assert!(matches!(p.state, EngineState::Unreachable(_)));
        assert_eq!(p.url, DEFAULT_URL, "no reply keeps the default URL");
    }
}
