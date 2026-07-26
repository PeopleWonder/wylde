//! Panel slot — the right pane of the main window.
//!
//! Render branches:
//!
//!   1. **`SlotState::Empty`** — there is nothing to render.  Happens
//!      when the registry is empty (a misconfigured build) or no row
//!      is selected.  Shows a small "No panels registered" message
//!      rather than a blank pane so the failure mode is visible.
//!
//!   2. **`SlotState::Mount`** — render the cached panel view.  For
//!      first-party gpui panels the Shell minted the `AnyView` on
//!      first selection; we paint it as a child.  For iframe panels
//!      the Shell mounted a native `wry::WebView` as a child window of
//!      the gpui Window's HWND; the slot paints a transparent
//!      placeholder of the same size so the WebView shows through.
//!
//!   3. **`SlotState::ServiceUnavailable`** — one or more
//!      `required_services` reported unhealthy, *or* the iframe URL
//!      probe failed.  Render the spec's "Service not running" stub:
//!      the panel title, the missing service name(s) (or `URL probe:
//!      <error>` for an iframe), and a "Start service" button.

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, AnyView, Context, ElementId, FontWeight, IntoElement, SharedString,
    Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::host::NavChromeHost;
use crate::nav::{NavRow, SlotState};
use crate::pack::pack;
use wylde_gui_controls::control;

/// Health of a mounted iframe panel's WebView, as the slot sees it. Plain data
/// (no `wry`) — it rode next to the `wry`-owning `IframeState` in the Shell,
/// which is why importing it forced the whole Shell crate to compile; it lives
/// here now so the nav chrome is `wry`-free. The Shell keeps the same enum on
/// its `IframeState` and projects it into [`IframeFrame`] each render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IframeHealth {
    Probing,
    Healthy,
    Unhealthy(String),
}

/// Slot-side projection of an iframe panel's state.  The Shell builds
/// one of these per render for the currently-selected iframe panel;
/// `render_slot` reads it to decide between the placeholder and the
/// "Checking…" / unavailable surfaces.
///
/// Why a dedicated type rather than threading the `IframeState`
/// through?  The `IframeState` owns the wry handle (`!Send`); keeping
/// `render_slot` over a plain data struct makes it testable without a
/// live WebView.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IframeFrame {
    pub key: String,
    pub url: String,
    pub sandbox: Option<String>,
    pub health: IframeHealth,
}

/// Render the slot for the current frame.
///
/// `mounted` is the cached gpui panel View for `state == Mount` on a
/// `gpui_view` source; when the Shell hasn't built it yet (very first
/// frame after selection) we paint a brief "Mounting…" placeholder so
/// the user sees something happen.
///
/// `iframe_frame` is the slot-side projection of the currently-selected
/// iframe panel.  When present (the selected panel is an iframe), the
/// Mount branch defers to the iframe rendering path: a transparent
/// placeholder if the WebView is mounted-and-healthy, a "Checking…"
/// strip while the URL probe is in flight.  The Shell synthesises a
/// `ServiceUnavailable` payload upstream if the probe failed, so the
/// failure path stays in the existing stub branch.
pub fn render_slot<V: NavChromeHost>(
    state: &SlotState,
    rows: &[NavRow],
    mounted: Option<&AnyView>,
    iframe_frame: Option<&IframeFrame>,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Stateful<gpui::Div> {
    let body: gpui::AnyElement = match state {
        SlotState::Empty => render_empty().into_any_element(),
        SlotState::Mount { key } => {
            // Iframe panel + slot has an iframe_frame → take the iframe
            // path; otherwise fall back to the gpui_view path.
            if let Some(frame) = iframe_frame {
                render_iframe(key, frame, rows).into_any_element()
            } else {
                match mounted {
                    Some(v) => div().size_full().child(v.clone()).into_any_element(),
                    None => render_mounting(key, rows).into_any_element(),
                }
            }
        }
        SlotState::ServiceUnavailable {
            key,
            missing,
            reasons,
        } => render_unavailable(key, missing, reasons, rows, window, cx).into_any_element(),
    };

    div()
        .id("wylde-slot")
        .flex_1()
        .h_full()
        .bg(rgb(pack(SURFACE_900)))
        .child(body)
}

/// Render an iframe panel.
///
/// Three sub-cases:
///   * `Probing` — "Checking <url>…" strip.
///   * `Healthy` — transparent placeholder.  The actual WebView is a
///     native child window of the gpui Window, mounted by
///     `Shell::mount_active_iframe`; this branch just declares that the
///     slot's airspace belongs to it so the user doesn't see a flicker
///     of gpui background paint underneath.
///   * `Unhealthy` — never reached here: the Shell synthesises
///     `SlotState::ServiceUnavailable` upstream so the failure path
///     stays consistent with required-service failure.
fn render_iframe(key: &str, frame: &IframeFrame, rows: &[NavRow]) -> gpui::Div {
    match &frame.health {
        IframeHealth::Probing => render_iframe_probing(key, frame, rows),
        IframeHealth::Healthy => render_iframe_placeholder(),
        IframeHealth::Unhealthy(_) => {
            // Defensive: if the Shell forgot to synthesise the
            // unavailable state, paint a minimal fallback so the slot
            // doesn't go blank.
            render_iframe_probing(key, frame, rows)
        }
    }
}

fn render_iframe_probing(key: &str, frame: &IframeFrame, rows: &[NavRow]) -> gpui::Div {
    let title = rows
        .iter()
        .find(|r| r.key == key)
        .map(|r| r.title.clone())
        .unwrap_or_else(|| key.to_owned());
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from(format!("Checking {title}…"))),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(frame.url.clone())),
                ),
        )
}

/// Transparent placeholder for an active iframe.  The native WebView
/// paints over this; we just claim the airspace.
fn render_iframe_placeholder() -> gpui::Div {
    div().size_full()
}

fn render_empty() -> gpui::Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "No panels registered — check the build aggregator.",
                )),
        )
}

fn render_mounting(key: &str, rows: &[NavRow]) -> gpui::Div {
    let title = rows
        .iter()
        .find(|r| r.key == key)
        .map(|r| r.title.clone())
        .unwrap_or_else(|| key.to_owned());
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(format!("Mounting {title}…"))),
        )
}

fn render_unavailable<V: NavChromeHost>(
    key: &str,
    missing: &[String],
    reasons: &[Option<String>],
    rows: &[NavRow],
    _window: &mut Window,
    cx: &mut Context<V>,
) -> gpui::Div {
    let title = rows
        .iter()
        .find(|r| r.key == key)
        .map(|r| r.title.clone())
        .unwrap_or_else(|| key.to_owned());

    let mut card = div()
        .max_w(px(420.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::LG))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from(format!("{title} unavailable"))),
        );

    // One block per missing service. A service the daemon reported a specific
    // reason for (a min_core incompatibility) shows that reason and NO Start
    // button — starting won't help; the user needs to update Wylde. A service
    // that's merely not running keeps its "Start" affordance.
    for (idx, service) in missing.iter().enumerate() {
        let reason = reasons.get(idx).and_then(Option::as_deref);

        let line = match reason {
            Some(r) => format!("`{service}` {r}"),
            None => format!("Required service `{service}` is not running."),
        };
        card = card.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(line)),
        );

        // Only offer "Start" when there's no incompatibility reason — an
        // incompatible service can't be fixed by starting it.
        if reason.is_none() {
            let service_owned: Arc<str> = Arc::from(service.as_str());
            let id: ElementId = ElementId::Name(format!("svc-start::{key}::{idx}").into());
            let label = SharedString::from(format!("Start {service}"));
            card = card.child(
                control(div(), id)
                    .px_3()
                    .py_2()
                    .rounded(px(4.0))
                    .bg(rgb(pack(BRAND)))
                    .border_1()
                    .border_color(rgb(pack(BORDER_DEFAULT)))
                    .cursor_pointer()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this: &mut V, _event, _window, cx| {
                            this.on_start_service_click(service_owned.clone(), cx);
                        }),
                    )
                    .child(label),
            );
        }
    }

    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .child(card)
}

/// Decide what a "Start service" button click should ask the Shell to
/// do, given the service name.  Returns the action verb + payload the
/// Shell forwards to `wylde_gui_pipe::lifecycle_action`.  Pure function
/// so the wiring is unit-testable without a live Lifecycle daemon.
pub fn start_service_action(service: &str) -> (&'static str, serde_json::Value) {
    ("service.start", serde_json::json!({ "name": service }))
}

/// Stylistic suppression for unused imports when gpui dims them across
/// rev bumps.  Cheaper than tagging each constant.
#[allow(dead_code)]
fn _imports_used() -> (gpui::Rgba, gpui::Rgba) {
    (BRAND_DIM, BORDER_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn start_service_action_targets_lifecycle_verb() {
        let (verb, payload) = start_service_action("wylde-harness");
        assert_eq!(verb, "service.start");
        assert_eq!(payload, json!({ "name": "wylde-harness" }));
    }

    #[test]
    fn start_service_action_carries_service_name_verbatim() {
        let (_, payload) = start_service_action("wylde-lifecycle");
        assert_eq!(payload["name"], "wylde-lifecycle");
    }

    #[test]
    fn iframe_frame_round_trips_through_equality() {
        let a = IframeFrame {
            key: "ext:n8n/editor".into(),
            url: "http://127.0.0.1:5678".into(),
            sandbox: Some("allow-scripts".into()),
            health: IframeHealth::Healthy,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn iframe_frame_distinguishes_health_variants() {
        let base = IframeFrame {
            key: "ext:n8n/editor".into(),
            url: "http://127.0.0.1:5678".into(),
            sandbox: None,
            health: IframeHealth::Probing,
        };
        let healthy = IframeFrame {
            health: IframeHealth::Healthy,
            ..base.clone()
        };
        let unhealthy = IframeFrame {
            health: IframeHealth::Unhealthy("refused".into()),
            ..base.clone()
        };
        assert_ne!(base, healthy);
        assert_ne!(healthy, unhealthy);
    }
}
