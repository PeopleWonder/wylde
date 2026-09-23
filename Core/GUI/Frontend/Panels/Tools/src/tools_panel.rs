//! Tools panel View — extension manager.
//!
//! State (held inline on the View):
//!   * `extensions`         — last-read `ext.list` reply.
//!   * `panels`             — last-read `extensions.list_panels` reply.
//!   * `pending_toggle`     — set of extension names with an in-flight
//!     enable/disable; rows in this set render their toggle in a
//!     disabled "Working…" state.
//!   * `error`              — last pipe error from a load or toggle.
//!     Surfaced as a strip above the row list so the user knows the
//!     panel is stale rather than silently empty.
//!   * `loading`            — `true` until the first `ext.list` reply
//!     arrives; the View paints a "Loading…" row in the interim.
//!
//! IPC reads use `cx.spawn` — same pattern Settings + Workspaces adopt.

use std::collections::BTreeSet;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, FontWeight, IntoElement,
    Render, SharedString, Stateful, Window,
};
use wylde_gui_controls::control;
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, BRAND_LIGHT, DANGER, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY, WARNING,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    disable_extension, enable_extension, list_extension_panels, list_extensions, ExtensionPanel,
    ExtensionStatus, PanelAvailability,
};

/// How often the panel re-reads the extension catalog + panel
/// availability.
///
/// This is what makes the surface *react* rather than merely be correct
/// once (#239): a service that dies, or an extension folder that is
/// deleted, changes the cards within one tick — no restart, no Refresh
/// click, no manual file surgery. The Shell caches a panel's View for
/// the process lifetime, so without a poll a Tools tab opened at launch
/// would show launch-time truth forever.
///
/// 5 s matches the Dashboard's refresh cadence; the reads behind it are
/// a cached directory stat plus TTL-cached loopback probes.
pub const PANEL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Root Tools panel.
pub struct ToolsPanel {
    pub extensions: Vec<ExtensionStatus>,
    pub panels: Vec<ExtensionPanel>,
    pub pending_toggle: BTreeSet<String>,
    pub error: Option<String>,
    pub loading: bool,
}

impl ToolsPanel {
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            panels: Vec::new(),
            pending_toggle: BTreeSet::new(),
            error: None,
            loading: true,
        }
    }

    /// Factory entry — matches the manifest `factory:` string
    /// (`wylde_panel_tools::ToolsPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            // The loop's first iteration fires synchronously, so this
            // also covers the initial load.
            Self::spawn_refresh_loop(cx);
            panel
        })
        .into()
    }

    /// Long-lived poll: re-read the catalog every
    /// [`PANEL_POLL_INTERVAL`] for the panel's lifetime.
    ///
    /// Same shape as the Dashboard's refresh loop — leading iteration
    /// with no sleep, gpui's native timer (the executor has no tokio
    /// reactor), and exit as soon as the entity is gone.
    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            let alive = this
                .update(app_cx, |_panel, cx| {
                    Self::spawn_refresh(cx);
                })
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

    /// Reload extensions + declared panels from the bridge.  Two async
    /// tasks; if either call fails we stash the error on the View so
    /// the user sees the pipe is broken.
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_extensions().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(exts) => {
                        panel.error = None;
                        panel.extensions = exts;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                    }
                }
                panel.loading = false;
                cx.notify();
            });
        })
        .detach();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // A failure on `extensions.list_panels` is not fatal — the
            // section just falls back to empty.  Surfacing the error
            // here would crowd a UI most users don't need to debug.
            let panels = list_extension_panels().await.unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                panel.panels = panels;
                cx.notify();
            });
        })
        .detach();
    }

    /// Per-row toggle handler.  Fires `ext.enable` or `ext.disable`
    /// based on the requested target state; the row sits in the
    /// `pending_toggle` set for the duration of the call so the user
    /// doesn't get a chance to double-click.
    ///
    /// Takes `&mut self` so the listener can mark the row pending in
    /// the same frame the click lands, before the async wire call
    /// kicks off.  Without that pre-flight stash the user would see a
    /// frame of "Enable / Disable" → "Working…" flicker.
    pub fn spawn_toggle(&mut self, name: String, target_enabled: bool, cx: &mut Context<Self>) {
        self.pending_toggle.insert(name.clone());
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = if target_enabled {
                enable_extension(&name).await
            } else {
                disable_extension(&name).await
            };
            let _ = this.update(app_cx, |panel, _cx| {
                panel.pending_toggle.remove(&name);
                if let Err(e) = &outcome {
                    panel.error = Some(e.clone());
                }
                if let Ok(updated) = outcome {
                    if let Some(row) = panel.extensions.iter_mut().find(|r| r.name == updated.name)
                    {
                        *row = updated;
                    }
                }
            });
            // After a toggle the declared-panel list may shift (an
            // enabled extension now contributes its panels; a disabled
            // one still surfaces them per the bridge's semantics, but
            // the surface stays consistent with a fresh read).
            let panels = list_extension_panels().await.unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                panel.panels = panels;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Default for ToolsPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for ToolsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = header_row(cx);

        let mut column = div()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
        }

        if self.loading {
            column = column.child(loading_row());
        } else if self.extensions.is_empty() {
            column = column.child(empty_state());
        } else {
            column = column.child(section_title("Installed extensions"));
            for ext in &self.extensions {
                let pending = self.pending_toggle.contains(&ext.name);
                column = column.child(extension_row(ext, pending, cx));
            }
        }

        if !self.panels.is_empty() {
            column = column.child(section_title("Declared panels"));
            for p in &self.panels {
                column = column.child(panel_row(p));
            }
        }

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<ToolsPanel>) -> gpui::Div {
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
                        .child(SharedString::from("Tools")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Installed MCP extensions and their declared UI panels. \
                             Toggle an extension off to stop its host process; toggle on \
                             to start it and surface its tools to chat.",
                        )),
                ),
        )
        .child(refresh_button(cx))
}

fn refresh_button(cx: &mut Context<ToolsPanel>) -> Stateful<gpui::Div> {
    control(div(), "tools-refresh")
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
            cx.listener(|_this: &mut ToolsPanel, _event, _window, cx| {
                ToolsPanel::spawn_refresh(cx);
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

fn extension_row(ext: &ExtensionStatus, pending: bool, cx: &mut Context<ToolsPanel>) -> gpui::Div {
    let name_for_toggle = ext.name.clone();
    let target_enabled = !ext.enabled;
    let toggle_label = if pending {
        "Working…"
    } else if ext.enabled {
        "Disable"
    } else {
        "Enable"
    };
    let status_color = match ext.status.as_str() {
        "running" => TEXT_PRIMARY,
        "starting" => TEXT_SECONDARY,
        "unhealthy" | "crashed" | "broken" => BRAND,
        _ => TEXT_MUTED,
    };
    let title = SharedString::from(ext.name.clone());
    let subtitle = SharedString::from(format!("v{}  ·  {}", ext.version, ext.status));

    let mut row = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_1()
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
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(status_color)))
                        .child(subtitle),
                ),
        );

    if let Some(err) = &ext.last_error {
        let err_label = SharedString::from(err.clone());
        row = row.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .max_w(px(220.0))
                .child(err_label),
        );
    }

    let mut button = control(div(), format!("ext-toggle::{}", ext.name))
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(SharedString::from(toggle_label));
    if !pending {
        button = button.cursor_pointer().on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut ToolsPanel, _ev, _window, cx| {
                this.spawn_toggle(name_for_toggle.clone(), target_enabled, cx);
            }),
        );
    }
    row.child(button)
}

/// The chip colour for an availability state. `LIVE` reads as active in
/// the brand hue; the two "you can't use this" states take the semantic
/// danger/warning tokens rather than a bespoke hex.
fn availability_color(a: PanelAvailability) -> gpui::Rgba {
    match a {
        PanelAvailability::Live => BRAND_LIGHT,
        PanelAvailability::Unreachable => DANGER,
        PanelAvailability::NotRunning | PanelAvailability::Unknown => WARNING,
    }
}

/// One declared-panel card.
///
/// Every card carries a status chip — the Workspaces rule ("a card shows
/// status or affords action") applied to services. Before #239 this
/// rendered a title and a URL and nothing else, which is why a stub
/// pointing at a port whose service had been extracted looked
/// indistinguishable from a working panel.
fn panel_row(p: &ExtensionPanel) -> gpui::Div {
    let icon_glyph = p
        .icon
        .as_deref()
        .and_then(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| {
            p.title
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "·".into())
        });
    let status = p.availability;
    let status_color = availability_color(status);
    // An unavailable panel's identity is de-emphasised so the eye lands
    // on the chip, not on a title that looks as live as its neighbours.
    let title_color = if status.is_live() {
        TEXT_PRIMARY
    } else {
        TEXT_MUTED
    };

    let mut meta = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(title_color)))
                .child(SharedString::from(format!("{} / {}", p.extension, p.title))),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(p.url.clone())),
        );

    // The reason, when there is one. Without it "UNAVAILABLE" leaves the
    // user guessing between "wrong port", "not installed" and "crashed".
    if let Some(detail) = p.detail.as_deref() {
        meta = meta.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(status_color)))
                .child(SharedString::from(detail.to_owned())),
        );
    }

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
                .child(SharedString::from(icon_glyph)),
        )
        .child(meta)
        .child(availability_chip(status, status_color))
}

/// The status chip itself — a tinted pill carrying the state's label.
fn availability_chip(status: PanelAvailability, color: gpui::Rgba) -> gpui::Div {
    div()
        .px_2()
        .py(px(2.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(tint(color, 0.35))
        .bg(tint(color, 0.12))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(color)))
        .child(SharedString::from(status.label()))
}

/// The same hue at a lower opacity, for the chip's fill and border.
/// Keeps the chip readable on `SURFACE_800` without introducing three
/// more palette tokens.
fn tint(c: gpui::Rgba, alpha: f32) -> gpui::Rgba {
    gpui::Rgba { a: alpha, ..c }
}

fn empty_state() -> gpui::Div {
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
                .child(SharedString::from("No extensions installed")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Install an MCP extension to give Wylde new tools and UI panels.",
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
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts.  Same
/// shim every panel keeps locally.
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
    fn new_with_defaults_is_constructible() {
        let p = ToolsPanel::new();
        assert!(p.extensions.is_empty());
        assert!(p.panels.is_empty());
        assert!(p.pending_toggle.is_empty());
        assert!(p.error.is_none());
        assert!(p.loading);
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<ToolsPanel>();
    }

    #[test]
    fn each_pipe_call_uses_expected_verb() {
        // Build-time witness — same pattern Settings + Workspaces use.
        let _ = list_extensions;
        let _ = list_extension_panels;
        let _ = enable_extension;
        let _ = disable_extension;
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }
}
