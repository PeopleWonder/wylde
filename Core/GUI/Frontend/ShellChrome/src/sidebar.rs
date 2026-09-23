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
use wylde_theme::colors::{BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_950, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::host::NavChromeHost;
use crate::nav::{NavOrigin, NavRow};
use crate::pack::pack;
use crate::resource_meter::{render_resource_meter, ResourceSnapshot};
use wylde_gui_controls::control;

/// Width of the expanded sidebar.  Matches the Svelte `w-52`
/// (208 px) — kept slim so the slot has breathing room on the
/// 1280-wide window.
pub const SIDEBAR_WIDTH: f32 = 208.0;

/// Build the sidebar `Div`.  Called once per Shell render — the
/// returned element is consumed by the Shell's outer flex container.
pub fn render_sidebar<V: NavChromeHost>(
    rows: &[NavRow],
    selected_key: Option<&str>,
    resources: Option<&ResourceSnapshot>,
    update_available: bool,
    _window: &mut Window,
    cx: &mut Context<V>,
) -> Stateful<gpui::Div> {
    let header = brand_header();
    // `flex_1` lets the nav column eat the slack so the resource meter
    // below pins to the bottom of the sidebar rather than floating up
    // under the last nav row.
    let mut nav = div()
        .id("wylde-sidebar-nav")
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .pt_2()
        .pb_2()
        .px_2()
        .overflow_hidden();

    for row in rows {
        let is_active = selected_key == Some(row.key.as_str());
        // The startup update check (slice 3d) surfaces as a small hint dot
        // on the Settings row — the panel that owns the Updates section.
        let show_update_dot = update_available && is_settings_row(row);
        nav = nav.child(row_button(row, is_active, show_update_dot, cx));
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
        .child(render_resource_meter(resources))
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

/// True for the first-party Settings row (`core/settings`) — the panel
/// that owns the Updates section, where an "update available" hint
/// belongs.  Matches on the id segment so a registry-key prefix change
/// (e.g. a future service rename) still resolves.
pub fn is_settings_row(row: &NavRow) -> bool {
    matches!(row.origin, NavOrigin::FirstParty) && row.key.rsplit('/').next() == Some("settings")
}

/// Single nav row.  Mouse-down forwards to `Shell::on_nav_click`.
/// `show_update_dot` paints the slice-3d "update available" hint.
fn row_button<V: NavChromeHost>(
    row: &NavRow,
    is_active: bool,
    show_update_dot: bool,
    cx: &mut Context<V>,
) -> Stateful<gpui::Div> {
    let key_owned = row.key.clone();
    let label = SharedString::from(row.title.clone());
    let icon_letter = SharedString::from(icon_letter_for(row));

    let (bg, fg) = if is_active {
        (Some(BRAND_DIM), TEXT_PRIMARY)
    } else {
        (None, TEXT_MUTED)
    };

    let id: ElementId = ElementId::Name(format!("nav-row::{}", row.key).into());

    let mut button = control(div(), id)
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
            cx.listener(move |this: &mut V, _event, _window, cx| {
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
    if show_update_dot {
        button = button.child(update_dot());
    }
    button
}

/// Small brand-tinted dot flagging "an update is available" on the
/// Settings row.  The label div eats the slack (`flex_1`) so the dot pins
/// to the row's right edge.
fn update_dot() -> gpui::Div {
    div()
        .w(px(8.0))
        .h(px(8.0))
        .rounded(px(4.0))
        .bg(rgb(pack(BRAND)))
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

    #[test]
    fn is_settings_row_matches_only_first_party_settings() {
        // The canonical first-party Settings key.
        assert!(is_settings_row(&row(
            "core/settings",
            "Settings",
            Some("settings")
        )));
        // A prefix change still resolves (we match the id segment).
        assert!(is_settings_row(&row("app/settings", "Settings", None)));
        // Other panels don't get the update dot.
        assert!(!is_settings_row(&row("core/chat", "Chat", None)));
        // An extension panel that happens to id "settings" is excluded —
        // the dot belongs to the first-party Updates section.
        let ext = NavRow {
            key: "ext:n8n/settings".into(),
            origin: NavOrigin::Extension,
            title: "Settings".into(),
            icon: None,
            order: 0,
            required_services: vec![],
        };
        assert!(!is_settings_row(&ext));
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
