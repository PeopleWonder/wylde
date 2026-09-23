//! The ambient bottom-left update **pill** and the changelog pop-up chrome
//! (#196).
//!
//! Claude-desktop-style: a small, dismissable notification anchored to the
//! shell's bottom-left — *not* a modal that blocks the app. It carries the
//! resolved version tag, an "Update" button (the existing whole-stack install
//! path), an "Ignore" button (dismiss this version only), and a "What's new"
//! affordance that opens the lazy-loaded [`wylde_changelog::ChangelogView`].
//!
//! These are render helpers, not a View: the pill's visibility gate and all its
//! click handlers live on the [`Shell`] (which owns the state the same frame),
//! exactly like [`crate::sidebar`]. The visibility decision itself is
//! [`wylde_changelog::pill_visible`]; this module only paints when the Shell has
//! already decided to show it.

use gpui::{
    div, prelude::*, px, rgb, AnyView, Context, FontWeight, IntoElement, MouseButton, SharedString,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BRAND, BRAND_LIGHT, SURFACE_650, SURFACE_700, SURFACE_800,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::host::NavChromeHost;
use crate::pack::pack;
use wylde_gui_controls::control;

/// Paint the update pill, anchored bottom-left of the shell's (relative) root.
/// `version` is the resolved available version, shown as a tag and carried into
/// the "Ignore" handler so the dismissal is keyed to exactly this release.
pub fn render_update_pill<V: NavChromeHost>(
    version: &str,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let ignore_version = version.to_string();
    div()
        // wylde-check: control-ok: the pill is a layout container — "What's
        // new", Update and Ignore inside it are the controls, not this shell.
        .id("wylde-update-pill")
        .absolute()
        .bottom_4()
        .left_4()
        .w(px(288.0))
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded(px(10.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .shadow_lg()
        // Header: brand dot · "Update available" · version tag.
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded(px(4.0))
                        .bg(rgb(pack(BRAND))),
                )
                .child(
                    div()
                        .flex_1()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from("Update available")),
                )
                .child(version_tag(version)),
        )
        // "What's new" → open the changelog pop-up.
        .child(
            control(div(), "wylde-update-pill-changelog")
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .hover(|s| s.text_color(rgb(pack(TEXT_PRIMARY))))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _w, cx| this.open_changelog(cx)),
                )
                .child(SharedString::from("What's new →")),
        )
        // Actions: Update (primary, whole-stack install) · Ignore (this version).
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    pill_button("wylde-update-pill-update", "Update", true).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| this.on_update_click(cx)),
                    ),
                )
                .child(
                    pill_button("wylde-update-pill-ignore", "Ignore", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _w, cx| {
                            this.on_ignore_click(ignore_version.clone(), cx)
                        }),
                    ),
                ),
        )
}

/// Paint the changelog pop-up: a dim, click-catching scrim (which closes on a
/// backdrop click) centred over a card holding the changelog viewer plus a
/// close button. The card stops mouse-down propagation so interacting with the
/// changelog never closes it.
pub fn render_changelog_modal<V: NavChromeHost>(
    view: &AnyView,
    cx: &mut Context<V>,
) -> impl IntoElement {
    control(div(), "wylde-changelog-scrim")
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        // Dim backdrop — translucent black passed straight (the `pack` idiom
        // drops alpha and would render it opaque).
        .bg(gpui::rgba(0x00_00_00_99))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| this.close_changelog(cx)),
        )
        .child(
            control(div(), "wylde-changelog-card")
                .relative()
                // Swallow clicks on the card so they don't reach the backdrop's
                // close handler (same idiom the Chat composer popovers use).
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _ev, _w, cx| cx.stop_propagation()),
                )
                .child(view.clone())
                .child(
                    control(div(), "wylde-changelog-close")
                        .absolute()
                        .top_2()
                        .right_2()
                        .w(px(26.0))
                        .h(px(26.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(rgb(pack(SURFACE_700)))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .hover(|s| {
                            s.bg(rgb(pack(SURFACE_650)))
                                .text_color(rgb(pack(TEXT_PRIMARY)))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev, _w, cx| this.close_changelog(cx)),
                        )
                        .child(SharedString::from("✕")),
                ),
        )
}

/// The version chip on the pill header — `v0.3.0` in a subtle pill.
fn version_tag(version: &str) -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_700)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .font_weight(FontWeight(weight::MEDIUM as f32))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(SharedString::from(format!("v{version}")))
}

/// A pill action button. `primary` = brand fill (Update); otherwise a quiet
/// ghost (Ignore). The caller attaches the `on_mouse_down` handler.
///
/// Routes through `control()` so both buttons register in the walk's per-frame
/// control registry (#247): they are real affordances — Update kicks the
/// whole-stack install, Ignore dismisses this version — so the control-walk
/// discovers and clicks them like any other control. The id is a bound param,
/// not a literal at this call, so the static id-scan doesn't demand it; the
/// pill state paints both, and their host-method deltas (`updated` /
/// `dismissed_version`) are what the walk asserts moved.
fn pill_button(id: &'static str, label: &'static str, primary: bool) -> gpui::Stateful<gpui::Div> {
    let (bg, fg, border, hover_bg) = if primary {
        (BRAND, TEXT_PRIMARY, BRAND, BRAND_LIGHT)
    } else {
        (SURFACE_700, TEXT_SECONDARY, BORDER_DEFAULT, SURFACE_650)
    };
    control(div(), id)
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .py_1()
        .rounded(px(6.0))
        .bg(rgb(pack(bg)))
        .border_1()
        .border_color(rgb(pack(border)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::MEDIUM as f32))
        .text_color(rgb(pack(fg)))
        .hover(move |s| s.bg(rgb(pack(hover_bg))))
        .child(SharedString::from(label))
}
