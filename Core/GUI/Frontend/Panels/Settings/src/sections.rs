//! Settings sections — small builder functions that return styled
//! `Div`s ready for the View's render tree.
//!
//! Each function is a *helper*, not a `View` — Settings is one panel
//! whose layout is one render call; splitting it into views adds
//! lifecycle overhead this slice doesn't need.
//!
//! Interaction lives here too: the toggle rows are `Stateful` (they
//! carry an `ElementId`) and the section builders take a
//! `&mut Context<SettingsPanel>` so they can attach an `on_mouse_down`
//! listener that calls back into the panel's write methods.  The rows
//! stay presentational — the *behaviour* (which verb to fire) is wired
//! at the call site, keeping the row builders reusable.

use gpui::{
    div, prelude::*, px, rgb, ElementId, FontWeight, MouseButton, SharedString, Stateful,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_LIGHT, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{ConsentSnapshot, OllamaSettings, UpdatePrefs};
use crate::SettingsPanel;

/// Shorthand for the panel render context the section builders thread
/// through to attach `on_mouse_down` listeners.
type Cx<'a> = gpui::Context<'a, SettingsPanel>;

/// Convert a theme `Rgba` to the packed `u32` the gpui `rgb()` helper
/// accepts.  Local copy of the shim in `Shell/src/window.rs::rgba_to_u32`;
/// each panel keeps its own so a future theme change doesn't ripple
/// through the Shell.
pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

/// Identifier reused on every section's outer container to keep the
/// `card` shape consistent with the Svelte `.card` class.
pub fn card() -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
}

/// Section title — small heading + muted subtitle.
pub fn section_title(title: &str, subtitle: &str) -> gpui::Div {
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
                .child(SharedString::from(title.to_owned())),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(subtitle.to_owned())),
        )
}

/// Render a clickable toggle row: label + hint + state badge.  The row
/// carries an `ElementId` (so it's `Stateful` and can take a mouse
/// listener) and a pointer cursor; the caller attaches the actual
/// `on_mouse_down` handler.
pub fn toggle_row(
    id: impl Into<ElementId>,
    label: &str,
    hint: &str,
    on: bool,
) -> Stateful<gpui::Div> {
    div()
        .id(id.into())
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_4()
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
                        .child(SharedString::from(label.to_owned())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(hint.to_owned())),
                ),
        )
        .child(state_badge(on))
}

/// Visual state badge for a toggle.  Cyan fill when on, dim outline off.
pub fn state_badge(on: bool) -> gpui::Div {
    let label = if on { "ON" } else { "OFF" };
    let bg = if on { BRAND } else { SURFACE_900 };
    let fg = if on { TEXT_PRIMARY } else { TEXT_MUTED };
    div()
        .bg(rgb(pack(bg)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_2()
        .py(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(fg)))
        .child(SharedString::from(label))
}

/// Updates section — master toggle, sub-controls when enabled, status
/// footer with current-version + last-checked.
pub fn updates_section(prefs: &UpdatePrefs, current_version: &str, cx: &mut Cx) -> gpui::Div {
    let mut c = card().child(section_title(
        "Updates",
        "Privacy-first. Wylde never checks for updates unless you turn it on.",
    ));
    c = c.child(
        toggle_row(
            "settings-updates-enabled",
            "Check for updates",
            "When off, no network calls. You can still check manually.",
            prefs.enabled,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _window, cx| this.toggle_updates_enabled(cx)),
        ),
    );
    if prefs.enabled {
        c = c
            .child(
                toggle_row(
                    "settings-updates-auto",
                    "Check automatically",
                    "Background check on the schedule below.",
                    prefs.auto_check,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| this.toggle_auto_check(cx)),
                ),
            )
            .child(
                div()
                    .id("settings-updates-frequency")
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| this.cycle_frequency(cx)),
                    )
                    .child(
                        div()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::SM))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child("Frequency"),
                    )
                    .child(state_pill(&prefs.frequency)),
            );
    }
    c.child(
        div()
            .border_t_1()
            .border_color(rgb(pack(BORDER_SUBTLE)))
            .pt_2()
            .flex()
            .flex_row()
            .gap_4()
            .child(meta_pair("Current version", current_version))
            .child(meta_pair(
                "Last checked",
                &prefs
                    .last_checked
                    .map(|ts| ts.to_string())
                    .unwrap_or_else(|| "never".into()),
            )),
    )
}

/// Startup section — autostart toggle.  Single row.
pub fn startup_section(enabled: bool, err: Option<&str>, cx: &mut Cx) -> gpui::Div {
    let mut c = card()
        .child(section_title(
            "Startup",
            "Launch Wylde automatically when you sign in.",
        ))
        .child(
            toggle_row(
                "settings-autostart",
                "Launch at login",
                "Registers Wylde in Windows' per-user startup list.",
                enabled,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.toggle_autostart(cx)),
            ),
        );
    if let Some(msg) = err {
        c = c.child(error_strip(msg));
    }
    c
}

/// Ollama inference defaults — read-only display.  Editable controls
/// come with the gpui-component slice (a later Frontend slice); this
/// section deliberately has no write path yet.
pub fn ollama_section(o: &OllamaSettings) -> gpui::Div {
    let rows = [
        ("Context window (num_ctx)", o.num_ctx.map(|v| v.to_string())),
        ("Max output (num_predict)", o.num_predict.map(|v| v.to_string())),
        ("Temperature", o.temperature.map(|v| format!("{v:.2}"))),
        ("Top-p", o.top_p.map(|v| format!("{v:.2}"))),
        ("Top-k", o.top_k.map(|v| v.to_string())),
        ("Min-p", o.min_p.map(|v| format!("{v:.2}"))),
        ("Repeat penalty", o.repeat_penalty.map(|v| format!("{v:.2}"))),
        ("Seed", o.seed.map(|v| v.to_string())),
        ("Keep alive", o.keep_alive.clone()),
    ];
    let mut c = card().child(section_title(
        "Ollama inference",
        "Defaults applied to every chat. Leave a field blank to use Ollama's built-in.",
    ));
    for (label, value) in rows {
        c = c.child(meta_pair(label, &value.unwrap_or_else(|| "—".into())));
    }
    c
}

/// Consent section — global no-auth toggle, a per-tool list (each row
/// flips approved ⇄ denied), and a reset-all affordance.
pub fn consent_section(snap: &ConsentSnapshot, cx: &mut Cx) -> gpui::Div {
    let mut c = card()
        .child(section_title(
            "Tool permissions",
            "Approve or deny tools the harness asks to run. Defaults to per-tool prompts.",
        ))
        .child(
            toggle_row(
                "settings-consent-no-auth",
                "Skip every prompt (no-auth)",
                "Every tool runs without asking. Use with care.",
                snap.no_auth,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.toggle_no_auth(cx)),
            ),
        );
    if snap.tools.is_empty() {
        c = c.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "No per-tool decisions yet — the harness will prompt next time it asks.",
                )),
        );
    } else {
        for (tool_id, decision) in &snap.tools {
            let tid = tool_id.clone();
            c = c.child(per_tool_row(tool_id, decision).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    this.cycle_tool_decision(tid.clone(), cx)
                }),
            ));
        }
        c = c.child(
            div()
                .id("settings-consent-reset")
                .cursor_pointer()
                .self_start()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_DEFAULT)))
                .px_2()
                .py(px(2.0))
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| this.reset_consent_action(cx)),
                )
                .child("Reset all decisions"),
        );
    }
    c
}

fn per_tool_row(tool_id: &str, decision: &str) -> Stateful<gpui::Div> {
    let (label, on) = match decision {
        "approved" => ("APPROVED", true),
        "denied" => ("DENIED", false),
        // Anything else (e.g. a backend that adds a new state) shows up
        // so it's visible rather than swallowed.
        other => (other, false),
    };
    let bg = if on { BRAND_LIGHT } else { SURFACE_900 };
    let fg = if on { TEXT_PRIMARY } else { TEXT_MUTED };
    div()
        .id(ElementId::Name(format!("settings-tool::{tool_id}").into()))
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .py_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(tool_id.to_owned())),
        )
        .child(
            div()
                .bg(rgb(pack(bg)))
                .border_1()
                .border_color(rgb(pack(BORDER_DEFAULT)))
                .rounded(px(4.0))
                .px_2()
                .py(px(2.0))
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(fg)))
                .child(SharedString::from(label.to_owned())),
        )
}

/// Pill showing a current string value (e.g. the update cadence).
fn state_pill(value: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(BRAND)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_2()
        .py(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(value.to_owned()))
}

/// Two-line `(label, value)` pair.  Used by Updates + Ollama sections.
fn meta_pair(label: &str, value: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(value.to_owned())),
        )
}

fn error_strip(message: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_900)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(message.to_owned()))
}

/// Top-of-panel banner for a write-side failure.  Emphasis-tinted
/// border so a failed toggle is obvious even though the badge already
/// rolled back to its prior state.  Matches the Models panel's
/// `error_strip` look (the palette has no dedicated danger hue).
pub fn error_banner(message: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(6.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(message.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips_known_surface() {
        // SURFACE_900 == #0a0e17.
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn state_badge_renders_both_states() {
        let _ = state_badge(true);
        let _ = state_badge(false);
    }

    #[test]
    fn state_pill_renders_value() {
        let _ = state_pill("weekly");
    }

    #[test]
    fn per_tool_row_handles_known_and_unknown_decisions() {
        let _ = per_tool_row("read_file", "approved");
        let _ = per_tool_row("write_file", "denied");
        let _ = per_tool_row("exec", "pending-new-backend-state");
    }

    #[test]
    fn error_banner_renders() {
        let _ = error_banner("consent: pipe down");
    }
}
