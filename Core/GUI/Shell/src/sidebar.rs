//! Sidebar widget — left nav listing every registered panel.
//!
//! The sidebar is *not* its own gpui View — it is a render helper that
//! the Shell calls into.  Rationale: the selection state lives on the
//! Shell anyway (the slot needs to read it the same frame) and the
//! Sidebar has no animation / async work that would benefit from
//! owning its own update loop.
//!
//! Visual goals:
//!   * Mirror the Svelte sidebar's silhouette — `surface-950` bg, brand
//!     header at the top, then a column of rows.  Active row gets a
//!     left accent bar and brand-tinted bg gradient (approximated as a
//!     flat brand-dim fill since gpui has no gradient builder yet).
//!   * Each row is clickable.  The handler calls `Shell::on_nav_click`
//!     so the click → state-mutation flow stays in one place.

use gpui::{
    div, prelude::*, px, rgb, Context, ElementId, FontWeight, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_950, TEXT_MUTED, TEXT_PRIMARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::nav::{NavOrigin, NavRow};
use crate::pack::pack;
use crate::shell_root::Shell;

/// Width of the expanded sidebar.  Matches the Svelte `w-52`
/// (208 px) — kept slim so the slot has breathing room on the
/// 1280-wide window.
pub const SIDEBAR_WIDTH: f32 = 208.0;

/// Build the sidebar `Div`.  Called once per Shell render — the
/// returned element is consumed by the Shell's outer flex container.
pub fn render_sidebar(
    rows: &[NavRow],
    selected_key: Option<&str>,
    _window: &mut Window,
    cx: &mut Context<Shell>,
) -> Stateful<gpui::Div> {
    let header = brand_header();
    let mut nav = div()
        .id("wylde-sidebar-nav")
        .flex()
        .flex_col()
        .gap_1()
        .pt_2()
        .pb_2()
        .px_2()
        .overflow_hidden();

    for row in rows {
        let is_active = selected_key == Some(row.key.as_str());
        nav = nav.child(row_button(row, is_active, cx));
    }

    div()
        .id("wylde-sidebar")
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(pack(SURFACE_950)))
        .border_r_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .child(header)
        .child(nav)
}

/// Brand header — the "Wylde" wordmark sits at the top of the sidebar,
/// matching the Svelte sidebar's brand block.
fn brand_header() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .child(
            div()
                .w(px(28.0))
                .h(px(28.0))
                .rounded(px(7.0))
                .bg(rgb(pack(BRAND_DIM)))
                .flex()
                .items_center()
                .justify_center()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::BOLD as f32))
                .child(SharedString::from("W")),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::LG))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::BOLD as f32))
                        .child(SharedString::from(crate::PRODUCT_TITLE)),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from("Run Free")),
                ),
        )
}

/// Single nav row.  Mouse-down forwards to `Shell::on_nav_click`.
fn row_button(row: &NavRow, is_active: bool, cx: &mut Context<Shell>) -> Stateful<gpui::Div> {
    let key_owned = row.key.clone();
    let label = SharedString::from(row.title.clone());
    let icon_letter = SharedString::from(icon_letter_for(row));

    let (bg, fg) = if is_active {
        (Some(BRAND_DIM), TEXT_PRIMARY)
    } else {
        (None, TEXT_MUTED)
    };

    let id: ElementId = ElementId::Name(format!("nav-row::{}", row.key).into());

    let mut button = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(6.0))
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut Shell, _event, _window, cx| {
                this.on_nav_click(&key_owned);
                cx.notify();
            }),
        )
        .child(icon_chip(&icon_letter, is_active))
        .child(
            div()
                .flex_1()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(fg)))
                .font_weight(FontWeight(if is_active {
                    weight::SEMIBOLD as f32
                } else {
                    weight::REGULAR as f32
                }))
                .child(label),
        );

    if let Some(b) = bg {
        button = button.bg(rgb(pack(b)));
    }
    if matches!(row.origin, NavOrigin::Extension) {
        button = button.child(extension_badge());
    }
    button
}

fn icon_chip(letter: &SharedString, is_active: bool) -> gpui::Div {
    let bg = if is_active { BRAND } else { BRAND_DIM };
    div()
        .w(px(20.0))
        .h(px(20.0))
        .rounded(px(4.0))
        .bg(rgb(pack(bg)))
        .flex()
        .items_center()
        .justify_center()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(letter.clone())
}

fn extension_badge() -> gpui::Div {
    div()
        .px_1()
        .rounded(px(3.0))
        .bg(rgb(pack(BRAND_DIM)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from("ext"))
}

/// Map a row to a single-character glyph for its icon chip.  Until the
/// lucide bundle lands the chip just shows the icon name's first
/// letter (`s` for `settings`, `f` for `folder`, …).
pub fn icon_letter_for(row: &NavRow) -> String {
    if let Some(icon) = &row.icon {
        if let Some(c) = icon.chars().next() {
            return c.to_ascii_uppercase().to_string();
        }
    }
    row.title
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "·".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, title: &str, icon: Option<&str>) -> NavRow {
        NavRow {
            key: key.into(),
            origin: NavOrigin::FirstParty,
            title: title.into(),
            icon: icon.map(|s| s.into()),
            order: 0,
            required_services: vec![],
        }
    }

    #[test]
    fn icon_letter_prefers_icon_name() {
        let r = row("core/settings", "Settings", Some("settings"));
        assert_eq!(icon_letter_for(&r), "S");
    }

    #[test]
    fn icon_letter_falls_back_to_title_initial() {
        let r = row("core/chat", "Chat", None);
        assert_eq!(icon_letter_for(&r), "C");
    }

    #[test]
    fn icon_letter_handles_missing_label() {
        let r = NavRow {
            key: "x".into(),
            origin: NavOrigin::FirstParty,
            title: "".into(),
            icon: None,
            order: 0,
            required_services: vec![],
        };
        assert_eq!(icon_letter_for(&r), "·");
    }

    /// Width is exposed so the slot can lay itself out next to the
    /// sidebar.  If the constant ever drifts this guard fires.
    #[test]
    fn sidebar_width_matches_svelte_alpha() {
        assert!(
            (200.0..=220.0).contains(&SIDEBAR_WIDTH),
            "sidebar width drifted out of the 200-220 px band",
        );
    }
}
