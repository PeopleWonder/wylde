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

use crate::ipc::{ConsentSnapshot, OllamaSettings, UpdateCheck, UpdatePrefs};
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

/// Updates section — master toggle, sub-controls when enabled, the
/// channel picker + manual "Check now" / "Install" flow (Phase 12.5), and
/// a status footer with current-version + last-checked.
pub fn updates_section(
    prefs: &UpdatePrefs,
    check: &UpdateCheck,
    current_version: &str,
    cx: &mut Cx,
) -> gpui::Div {
    let mut c = card().child(section_title(
        "Updates",
        "Privacy-first. Wylde never checks for updates unless you turn it on.",
    ));
    c = c.child(
        toggle_row(
            "settings-updates-enabled",
            "Check for updates",
            "When off, no automatic network calls. You can still check manually below.",
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
            .child(labeled_pill_row(
                "settings-updates-frequency",
                "Frequency",
                &prefs.frequency,
                cx.listener(|this, _ev, _window, cx| this.cycle_frequency(cx)),
            ));
    }

    // Channel picker — shown regardless of the master toggle, since the
    // manual check below uses it. Pill cycles stable ⇄ beta.
    c = c.child(labeled_pill_row(
        "settings-updates-channel",
        "Channel",
        channel_label(&prefs.channel),
        cx.listener(|this, _ev, _window, cx| this.cycle_channel(cx)),
    ));

    // Manual check + result.
    let checking = matches!(check, UpdateCheck::Checking | UpdateCheck::Installing);
    c = c.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                action_button("settings-updates-check", check_button_label(check), checking)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| this.check_now(cx)),
                    ),
            )
            .children(install_button(check, cx)),
    );
    if let Some(line) = update_status_line(check) {
        c = c.child(line);
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
                    .map(humanize_last_checked)
                    .unwrap_or_else(|| "never".into()),
            )),
    )
}

/// Title-case the persisted channel string for display ("stable" → "Stable").
fn channel_label(channel: &str) -> &'static str {
    match channel {
        "beta" => "Beta",
        _ => "Stable",
    }
}

/// Label for the check button, reflecting in-flight state.
fn check_button_label(check: &UpdateCheck) -> &'static str {
    match check {
        UpdateCheck::Checking => "Checking…",
        _ => "Check now",
    }
}

/// A `(label, pill)` row that cycles a value on click. Shared by the
/// Frequency and Channel pickers.
fn labeled_pill_row(
    id: impl Into<ElementId>,
    label: &str,
    value: &str,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> Stateful<gpui::Div> {
    div()
        .id(id.into())
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .on_mouse_down(MouseButton::Left, on_click)
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(label.to_owned())),
        )
        .child(state_pill(value))
}

/// A small clickable button. `dim` greys it out while an action is in
/// flight (the click handler also no-ops re-entrant clicks).
fn action_button(id: impl Into<ElementId>, label: &str, dim: bool) -> Stateful<gpui::Div> {
    let fg = if dim { TEXT_MUTED } else { TEXT_PRIMARY };
    div()
        .id(id.into())
        .cursor_pointer()
        .self_start()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_900)))
        .px_3()
        .py(px(4.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(fg)))
        .child(SharedString::from(label.to_owned()))
}

/// The "Install update" button — only present when a check resolved an
/// available update. Returned as an `Option` so the caller can splice it
/// in with `.children(...)`.
fn install_button(check: &UpdateCheck, cx: &mut Cx) -> Option<Stateful<gpui::Div>> {
    let installing = matches!(check, UpdateCheck::Installing);
    match check {
        UpdateCheck::Available(_) | UpdateCheck::Installing => Some(
            action_button("settings-updates-install", "Install update", installing).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.install_update(cx)),
            ),
        ),
        _ => None,
    }
}

/// Render the one-line status under the buttons for the current check
/// state. `Idle` shows nothing.
fn update_status_line(check: &UpdateCheck) -> Option<gpui::Div> {
    let (text, is_error): (String, bool) = match check {
        UpdateCheck::Idle => return None,
        UpdateCheck::Checking => ("Checking for updates…".into(), false),
        UpdateCheck::UpToDate => ("You're on the latest version.".into(), false),
        UpdateCheck::Available(info) => {
            (format!("Update available: v{} — review and install.", info.version), false)
        }
        UpdateCheck::Installing => ("Downloading and verifying update…".into(), false),
        UpdateCheck::Installed => {
            ("Update installed — restart Wylde to apply.".into(), false)
        }
        UpdateCheck::Failed(msg) => (format!("Update failed: {msg}"), true),
    };
    if is_error {
        return Some(error_strip(&text));
    }
    Some(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_SECONDARY)))
            .child(SharedString::from(text)),
    )
}

/// Render a persisted `last_checked` epoch as a human-readable relative
/// time ("just now", "5 minutes ago", "3 days ago").  Before this the
/// footer rendered `ts.to_string()` — a raw unix timestamp like
/// `1717372800` leaking straight into the Settings UI.
///
/// The lifecycle daemon owns the stored value and may write it in
/// seconds or milliseconds, so we normalise by magnitude (see
/// [`humanize_since`]) before computing the delta.
pub(crate) fn humanize_last_checked(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    humanize_since(now, ts)
}

/// Pure relative-time bucket math, split out from [`humanize_last_checked`]
/// so it's unit-testable without reading the wall clock.  Both `now_secs`
/// and `ts` are unix epochs; a `ts` past the seconds-epoch ceiling
/// (~year 33658) is treated as milliseconds and divided down.
fn humanize_since(now_secs: u64, ts: u64) -> String {
    // A seconds-epoch won't reach 1e12 until the year 33658, so anything
    // at/over that threshold is a milliseconds value.
    let ts_secs = if ts >= 1_000_000_000_000 { ts / 1000 } else { ts };
    if ts_secs == 0 {
        return "never".into();
    }
    // Future timestamp (clock skew) — clamp to "just now" rather than
    // underflowing the subtraction below.
    if ts_secs >= now_secs {
        return "just now".into();
    }
    let delta = now_secs - ts_secs;
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if delta < MIN {
        "just now".into()
    } else if delta < HOUR {
        let n = delta / MIN;
        format!("{n} minute{} ago", plural(n))
    } else if delta < DAY {
        let n = delta / HOUR;
        format!("{n} hour{} ago", plural(n))
    } else {
        let n = delta / DAY;
        format!("{n} day{} ago", plural(n))
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
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

    #[test]
    fn humanize_since_buckets() {
        let now = 1_000_000_000u64;
        assert_eq!(humanize_since(now, 0), "never");
        assert_eq!(humanize_since(now, now), "just now");
        // Future timestamp (clock skew) clamps rather than underflowing.
        assert_eq!(humanize_since(now, now + 50), "just now");
        assert_eq!(humanize_since(now, now - 30), "just now");
        assert_eq!(humanize_since(now, now - 60), "1 minute ago");
        assert_eq!(humanize_since(now, now - 5 * 60), "5 minutes ago");
        assert_eq!(humanize_since(now, now - 3600), "1 hour ago");
        assert_eq!(humanize_since(now, now - 5 * 3600), "5 hours ago");
        assert_eq!(humanize_since(now, now - 86_400), "1 day ago");
        assert_eq!(humanize_since(now, now - 3 * 86_400), "3 days ago");
    }

    #[test]
    fn humanize_since_detects_millis() {
        // `ts` one hour earlier than `now`, but expressed in milliseconds:
        // the magnitude heuristic must divide it back to seconds.
        let now = 2_000_000_000u64;
        let ts_millis = (now - 3600) * 1000;
        assert_eq!(humanize_since(now, ts_millis), "1 hour ago");
    }
}
