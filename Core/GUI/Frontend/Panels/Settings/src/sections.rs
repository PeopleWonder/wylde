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
    div, prelude::*, px, rgb, ElementId, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    SharedString, Stateful,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_FOCUSED, BORDER_SUBTLE, BRAND, BRAND_LIGHT,
    SURFACE_800, SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    ConsentSnapshot, OllamaSettings, UpdateCheck, UpdatePrefs, VoiceSettings, VoiceTest,
};
use crate::SettingsPanel;
use wylde_gui_controls::control;

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

    // Manual check + result. The "Check now" button always shows; the
    // Install affordance now lives inside the changelog card (Accept), so
    // it is no longer spliced into this row.
    let checking = matches!(check, UpdateCheck::Checking | UpdateCheck::Installing);
    c = c.child(
        div().flex().flex_row().items_center().gap_3().child(
            action_button(
                "settings-updates-check",
                check_button_label(check),
                checking,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.check_now(cx)),
            ),
        ),
    );
    // An available update opens the changelog card (release notes + Accept /
    // Decline); every other state renders the one-line status.
    if let UpdateCheck::Available(info) = check {
        c = c.child(changelog_card(info, cx));
    } else if let Some(line) = update_status_line(check) {
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

/// Display label for the persisted channel string. The wire value stays
/// `"beta"` end-to-end; only the user-facing word is "Experimental" (the maintainer's
/// wording). Keep this the single source of the label so the pill, the
/// warning modal, and any status text never drift apart.
fn channel_label(channel: &str) -> &'static str {
    match channel {
        "beta" => "Experimental",
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
    id: impl Into<gpui::ElementId>,
    label: &str,
    value: &str,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> Stateful<gpui::Div> {
    control(div(), id)
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

/// The push-to-talk hotkey row: a `(label, pill)` like [`labeled_pill_row`],
/// but the pill is a *live-capture* affordance instead of a cycle.
///
/// Resting state shows the current chord on a brand pill (matching the
/// other voice rows). Clicking arms capture — the panel focuses the pill
/// and the pill swaps to a focus-ringed "Press any key combination…"
/// prompt; the next chord (via the panel's `on_hotkey_key`) commits.
/// `note` carries a transient reserved-key message shown beneath.
///
/// `track_focus` + `on_key_down` mirror the `TextInput` widget's
/// keyboard-capture pattern — the only focusable elements in the panel.
fn hotkey_capture_row(
    focus: &FocusHandle,
    capturing: bool,
    value: &str,
    note: Option<&str>,
    cx: &mut Cx,
) -> gpui::Div {
    // The pill itself: focusable, so its `on_key_down` receives the chord
    // while armed; click toggles capture on/off.
    let display = if capturing {
        crate::hotkey::CAPTURE_PROMPT
    } else {
        value
    };
    let mut pill = control(div(), "settings-voice-hotkey")
        .cursor_pointer()
        .rounded(px(4.0))
        .border_1()
        .px_2()
        .py(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .track_focus(focus)
        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
            this.on_hotkey_key(ev, window, cx);
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, window, cx| this.toggle_hotkey_capture(window, cx)),
        )
        .child(SharedString::from(display.to_owned()));

    if capturing {
        // Armed: muted prompt on the surface, with the focus ring.  The
        // ring is alpha-bearing — pass it straight (no `pack`) so the
        // hue composites instead of flattening to an opaque line.
        pill = pill
            .bg(rgb(pack(SURFACE_800)))
            .border_color(BORDER_FOCUSED)
            .text_color(rgb(pack(TEXT_SECONDARY)));
    } else {
        // Resting: brand pill, matching the other voice rows.
        pill = pill
            .bg(rgb(pack(BRAND)))
            .border_color(rgb(pack(BORDER_DEFAULT)))
            .text_color(rgb(pack(TEXT_PRIMARY)));
    }

    let top = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from("Push-to-talk hotkey")),
        )
        .child(pill);

    let mut row = div().flex().flex_col().gap_1().child(top);

    // While armed, a one-line hint; if a reserved key was pressed, swap in
    // the rejection note instead.
    if capturing {
        let hint = note.unwrap_or("Esc to cancel · Enter and Tab are reserved.");
        row = row.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(if note.is_some() {
                    BRAND_LIGHT
                } else {
                    TEXT_MUTED
                })))
                .child(SharedString::from(hint.to_owned())),
        );
    }

    row
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

/// The changelog card, shown when a check resolves an available update
/// (Phase 12.5, req 7). Presents the release notes for the pending version
/// and the two decisions the user must make before anything is applied:
///
///   * **Accept** — download + verify + install (the existing manual path).
///     Nothing is fetched from the network until this click, on either
///     channel: the privacy-first "no bytes until you accept" contract.
///   * **Decline** — "Skip this version"; persists the version so the
///     automatic path stops re-offering it until a newer one appears.
fn changelog_card(info: &wylde_updater::UpdateInfo, cx: &mut Cx) -> gpui::Div {
    let title = format!("Update available: v{}", info.version);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .bg(rgb(pack(SURFACE_900)))
        .p_3()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(title)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("What's new")),
        )
        .child(changelog_body(&info.notes))
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap_2()
                .child(
                    modal_button(
                        "settings-updates-decline",
                        "Decline (Skip this version)",
                        false,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| this.skip_version(cx)),
                    ),
                )
                .child(
                    modal_button("settings-updates-accept", "Accept", true).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| this.install_update(cx)),
                    ),
                ),
        )
}

/// Render release notes as a scrollable, line-wrapped block. Splitting on
/// newlines gives reliable hard line breaks (a single text node collapses
/// them), and the fixed max height keeps a long changelog from pushing the
/// buttons off-screen. The notes are shown as plain text — a stub one-liner
/// (a release cut without `--notes-file`) still renders sensibly.
fn changelog_body(notes: &str) -> Stateful<gpui::Div> {
    let trimmed = notes.trim();
    let mut body = div()
        .id("settings-updates-changelog")
        .max_h(px(180.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)));
    if trimmed.is_empty() {
        return body.child(SharedString::from("No release notes were provided."));
    }
    // Defensive cap: changelogs are small, but never build an unbounded tree.
    for line in trimmed.lines().take(400) {
        body = body.child(div().child(SharedString::from(line.to_owned())));
    }
    body
}

/// Render the one-line status under the buttons for the current check
/// state. `Idle` shows nothing.
fn update_status_line(check: &UpdateCheck) -> Option<gpui::Div> {
    let (text, is_error): (String, bool) = match check {
        UpdateCheck::Idle => return None,
        UpdateCheck::Checking => ("Checking for updates…".into(), false),
        UpdateCheck::UpToDate => ("You're on the latest version.".into(), false),
        UpdateCheck::Available(info) => (
            format!("Update available: v{} — review and install.", info.version),
            false,
        ),
        UpdateCheck::Installing => ("Downloading and verifying update…".into(), false),
        UpdateCheck::Installed => ("Update installed — restart Wylde to apply.".into(), false),
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
    let ts_secs = if ts >= 1_000_000_000_000 {
        ts / 1000
    } else {
        ts
    };
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

// ── Ollama inference (per-model defaults + overrides) ─────────────────

/// How a field's value is typed when persisted as an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Integer (`i64`): num_ctx, num_predict, top_k, seed.
    Int,
    /// Float (`f64`): temperature, top_p, min_p, repeat_penalty.
    Float,
    /// Free string: keep_alive (`"5m"`, `"-1"`, …).
    Text,
}

/// One of the nine Ollama inference fields the section renders.
pub struct OllamaField {
    /// The override-store key (also the `/api/show` parameter name).
    pub key: &'static str,
    /// Human label shown above the input.
    pub label: &'static str,
    /// How to coerce a typed value before persisting.
    pub kind: FieldKind,
    /// Ollama's documented global default (scope §3.3), shown as the
    /// placeholder when the model itself declares no value for this field.
    pub fallback: &'static str,
}

/// The nine fields in render order. The fallback column is Ollama's
/// documented Modelfile defaults per the scope doc.
pub const OLLAMA_FIELDS: [OllamaField; 9] = [
    OllamaField {
        key: "num_ctx",
        label: "Context window (num_ctx)",
        kind: FieldKind::Int,
        fallback: "4096",
    },
    OllamaField {
        key: "num_predict",
        label: "Max output (num_predict)",
        kind: FieldKind::Int,
        fallback: "-1",
    },
    OllamaField {
        key: "temperature",
        label: "Temperature",
        kind: FieldKind::Float,
        fallback: "0.8",
    },
    OllamaField {
        key: "top_p",
        label: "Top-p",
        kind: FieldKind::Float,
        fallback: "0.9",
    },
    OllamaField {
        key: "top_k",
        label: "Top-k",
        kind: FieldKind::Int,
        fallback: "40",
    },
    OllamaField {
        key: "min_p",
        label: "Min-p",
        kind: FieldKind::Float,
        fallback: "0.0",
    },
    OllamaField {
        key: "repeat_penalty",
        label: "Repeat penalty",
        kind: FieldKind::Float,
        fallback: "1.1",
    },
    OllamaField {
        key: "seed",
        label: "Seed",
        kind: FieldKind::Int,
        fallback: "0",
    },
    OllamaField {
        key: "keep_alive",
        label: "Keep alive",
        kind: FieldKind::Text,
        fallback: "5m",
    },
];

/// Format an `OllamaSettings` field as a display string by key, or `None`
/// when that field is unset. Shared by placeholder (model defaults) and
/// value (stored overrides) sourcing.
pub fn ollama_field_string(o: &OllamaSettings, key: &str) -> Option<String> {
    match key {
        "num_ctx" => o.num_ctx.map(|v| v.to_string()),
        "num_predict" => o.num_predict.map(|v| v.to_string()),
        "temperature" => o.temperature.map(|v| format!("{v}")),
        "top_p" => o.top_p.map(|v| format!("{v}")),
        "top_k" => o.top_k.map(|v| v.to_string()),
        "min_p" => o.min_p.map(|v| format!("{v}")),
        "repeat_penalty" => o.repeat_penalty.map(|v| format!("{v}")),
        "seed" => o.seed.map(|v| v.to_string()),
        "keep_alive" => o.keep_alive.clone(),
        _ => None,
    }
}

/// Whether a key currently has a stored override (drives ↺ visibility +
/// the normal-weight value styling).
pub fn ollama_has_override(overrides: &OllamaSettings, key: &str) -> bool {
    ollama_field_string(overrides, key).is_some()
}

/// Transient card shown while the effective model + its defaults are
/// being resolved, so the section doesn't flash the empty state first.
pub fn ollama_loading_card() -> gpui::Div {
    card().child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::SM))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child("Ollama inference — loading…"),
    )
}

/// State 1 — no model selected. A small muted header (the maintainer's exact
/// wording) plus a "Go to Models panel" link; deliberately *not* a full
/// card with subtext.
fn ollama_empty_state(cx: &mut Cx) -> gpui::Div {
    card()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child("No Model Currently Loaded"),
        )
        .child(
            control(div(), "settings-ollama-goto-models")
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .child("Go to Models panel →")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| this.goto_models(cx)),
                ),
        )
}

/// The ↺ per-field reset affordance — present only when an override is
/// stored for `key`. Clicking clears that override so the field falls
/// back to its placeholder.
fn ollama_reset_button(key: &'static str, cx: &mut Cx) -> Stateful<gpui::Div> {
    control(div(), format!("ollama-reset-{key}"))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND_LIGHT)))
        .child("↺ reset")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, _window, cx| this.reset_ollama_field(key, cx)),
        )
}

/// One editable field: label (+ ↺ when overridden) over the text input.
/// Placeholder vs value styling is the input's own (greyed placeholder =
/// model default; normal-weight text = stored override).
fn ollama_field_row(
    field: &OllamaField,
    input: &gpui::Entity<wylde_gpui_input::TextInput>,
    has_override: bool,
    cx: &mut Cx,
) -> gpui::Div {
    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(field.label)),
        );
    if has_override {
        head = head.child(ollama_reset_button(field.key, cx));
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(head)
        .child(input.clone())
}

/// Ollama inference section — the four-state machine (scope §5).
///
///   * State 1 (no model): [`ollama_empty_state`].
///   * State 2 (model + Ollama unreachable): the editable grid with a
///     greyed-dash placeholder set on each input + a note.
///   * State 3 (loaded): the editable grid, placeholder = model default
///     (∪ global fallback), value = stored override, per-field ↺.
///   * State 4 is the same render driven by a `model_bus`-triggered
///     refresh — no separate branch here.
///
/// `inputs` is the panel's array of nine `TextInput` entities (parallel
/// to [`OLLAMA_FIELDS`]); placeholders/values are kept in sync by the
/// panel, so this builder only lays them out. An empty `inputs` (the
/// section hasn't initialised its inputs yet) renders the header only.
#[allow(clippy::too_many_arguments)]
pub fn ollama_section(
    model: Option<&str>,
    unreachable_note: Option<&str>,
    inputs: &[gpui::Entity<wylde_gpui_input::TextInput>],
    overrides: &OllamaSettings,
    cx: &mut Cx,
) -> gpui::Div {
    // State 1 — no model resolved.
    let Some(model) = model else {
        return ollama_empty_state(cx);
    };

    let mut c = card().child(section_title(
        &format!("Ollama inference · {model}"),
        "Placeholder = this model's default. Type to override; ↺ resets a field.",
    ));

    // State 2 note — Ollama upstream couldn't be queried for defaults.
    if let Some(note) = unreachable_note {
        c = c.child(error_strip(note));
    }

    // The editable grid (States 2 & 3 share it; the difference is the
    // placeholder the panel set on each input + the note above).
    for (i, field) in OLLAMA_FIELDS.iter().enumerate() {
        let Some(input) = inputs.get(i) else {
            continue;
        };
        let has_override = ollama_has_override(overrides, field.key);
        c = c.child(ollama_field_row(field, input, has_override, cx));
    }
    c
}

/// Voice section (Slice 6) — capture mode, push-to-talk hotkey, STT
/// backend preference, mic device, mic sensitivity, wake word, and a
/// one-shot "Test mic" affordance. Most editable rows are pills the user
/// cycles (the panel owns the cycle order + the write); the wake-word
/// enable is a toggle; the push-to-talk hotkey is a *live-capture* pill
/// (click to arm, press a chord to bind). Reads/writes go to
/// `\\.\pipe\wylde-voice` via the `voice.get_config` / `voice.set_config`
/// verbs.
///
/// The whole section degrades gracefully: when the voice service is
/// offline (`offline = true`) it renders on its defaults plus a note,
/// and writes simply surface the pipe error in the page banner.
///
/// `hotkey_focus` / `capturing` / `hotkey_note` thread the hotkey
/// widget's capture state down from the panel: the pill takes keyboard
/// focus while armed and shows a prompt + any reserved-key note.
#[allow(clippy::too_many_arguments)]
pub fn voice_section(
    voice: &VoiceSettings,
    test: &VoiceTest,
    offline: bool,
    hotkey_focus: &FocusHandle,
    capturing: bool,
    hotkey_note: Option<&str>,
    cx: &mut Cx,
) -> gpui::Div {
    let mut c = card().child(section_title(
        "Voice",
        "Speech-to-text, push-to-talk, and wake word.",
    ));

    if offline {
        c = c.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Voice service offline — showing defaults; changes won't \
                     save until it's running.",
                )),
        );
    }

    // Capture mode — push-to-talk ⇄ always-on.
    c = c.child(labeled_pill_row(
        "settings-voice-mode",
        "Mode",
        voice_mode_label(&voice.mode),
        cx.listener(|this, _ev, _window, cx| this.cycle_voice_mode(cx)),
    ));

    // Push-to-talk hotkey — live capture. Click to arm, then press the
    // chord. Only meaningful in push-to-talk mode, but shown always so
    // the choice is discoverable.
    c = c.child(hotkey_capture_row(
        hotkey_focus,
        capturing,
        &voice.push_to_talk_hotkey,
        hotkey_note,
        cx,
    ));

    // STT backend preference — Auto / CPU / NPU.
    c = c.child(labeled_pill_row(
        "settings-voice-backend",
        "Speech recognition",
        backend_label(&voice.stt_backend_pref),
        cx.listener(|this, _ev, _window, cx| this.cycle_voice_backend(cx)),
    ));

    // Input device — system default + each enumerated device.
    c = c.child(labeled_pill_row(
        "settings-voice-device",
        "Input device",
        &device_label(voice.input_device.as_deref()),
        cx.listener(|this, _ev, _window, cx| this.cycle_input_device(cx)),
    ));

    // Mic sensitivity (VAD) — Low / Medium / High.
    c = c.child(labeled_pill_row(
        "settings-voice-vad",
        "Mic sensitivity",
        vad_label(&voice.vad_sensitivity),
        cx.listener(|this, _ev, _window, cx| this.cycle_voice_vad(cx)),
    ));

    // Wake word — enable toggle + phrase picker (shown when enabled).
    c = c.child(
        toggle_row(
            "settings-voice-wakeword",
            "Wake word",
            "Listen for a spoken phrase to start a session in always-on mode.",
            voice.wake_word_enabled,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _window, cx| this.toggle_wake_word(cx)),
        ),
    );
    if voice.wake_word_enabled {
        c = c.child(labeled_pill_row(
            "settings-voice-wakeword-phrase",
            "Wake phrase",
            &wake_word_label(&voice.wake_word_model),
            cx.listener(|this, _ev, _window, cx| this.cycle_wake_word_model(cx)),
        ));
    }

    // Test mic — one-shot capture + level/transcript readout.
    let running = matches!(test, VoiceTest::Running);
    c = c.child(
        div()
            .border_t_1()
            .border_color(rgb(pack(BORDER_SUBTLE)))
            .pt_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                action_button("settings-voice-test", test_mic_button_label(test), running)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _window, cx| this.run_test_mic(cx)),
                    ),
            )
            .children(test_mic_line(test)),
    );

    c
}

/// Friendly label for the persisted capture mode.
fn voice_mode_label(mode: &str) -> &'static str {
    match mode {
        "always_on" => "Always-on",
        _ => "Push-to-talk",
    }
}

/// Friendly label for the STT backend preference.
fn backend_label(backend: &str) -> &'static str {
    match backend {
        "cpu" => "CPU",
        "npu" => "NPU",
        _ => "Auto",
    }
}

/// Friendly label for the VAD sensitivity bucket.
fn vad_label(sensitivity: &str) -> &'static str {
    match sensitivity {
        "low" => "Low",
        "high" => "High",
        _ => "Medium",
    }
}

/// Display string for the selected input device (`None` = system default).
fn device_label(device: Option<&str>) -> String {
    match device {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => crate::ipc::DEVICE_SYSTEM_DEFAULT.to_owned(),
    }
}

/// Display the wake-word model's phrase suffix (`openWakeWord/hey-jarvis`
/// → `hey-jarvis`), falling back to the full id if it has no vendor
/// prefix.
fn wake_word_label(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_owned()
}

/// Label for the test button, reflecting in-flight state.
fn test_mic_button_label(test: &VoiceTest) -> &'static str {
    match test {
        VoiceTest::Running => "Listening…",
        _ => "Test mic",
    }
}

/// One-line status under the test button. `Idle` shows nothing.
fn test_mic_line(test: &VoiceTest) -> Option<gpui::Div> {
    match test {
        VoiceTest::Idle => None,
        VoiceTest::Running => Some(status_text("Listening — speak now…")),
        VoiceTest::Failed(msg) => Some(error_strip(&format!("Test failed: {msg}"))),
        VoiceTest::Done(result) => {
            let level = (result.peak.clamp(0.0, 1.0) * 100.0).round() as u32;
            let line = if !result.transcript.is_empty() {
                format!(
                    "Level {level}% — heard: \u{201c}{}\u{201d}",
                    result.transcript
                )
            } else if let Some(note) = &result.note {
                format!("Level {level}% — {note}")
            } else {
                format!("Level {level}% — mic is working.")
            };
            Some(status_text(&line))
        }
    }
}

/// A muted one-line status string (shared by the test-mic readout).
fn status_text(text: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(SharedString::from(text.to_owned()))
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
            control(div(), "settings-consent-reset")
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
    control(div(), format!("settings-tool::{tool_id}"))
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

// ── Privacy & Network ─────────────────────────────────────────────────

/// Privacy & Network section — the centralized opt-in for features that
/// may make an outside network connection. Today: the HuggingFace
/// online-search toggle, plus a "Reset privacy warnings" affordance that
/// re-arms the first-time modal. Placed next to Updates (the other
/// outside-connection feature) at the top of the page.
pub fn privacy_section(
    privacy: crate::ipc::PrivacyPrefs,
    encryption_at_rest: bool,
    cx: &mut Cx,
) -> gpui::Div {
    card()
        .child(section_title(
            "Privacy & Network",
            "Features that may reach the internet. Everything here is off by default.",
        ))
        .child(
            toggle_row(
                "settings-privacy-hf-search",
                "Online model search (HuggingFace)",
                "Search HuggingFace's public catalog for models beyond the built-in list. \
                 Each search sends only your query term, over HTTPS.",
                privacy.hf_search_enabled,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.toggle_hf_search(cx)),
            ),
        )
        .child(
            // OI-14: encryption at rest. On by default, local-only (no
            // network) — lives here as the user-facing data-protection control.
            toggle_row(
                "settings-encryption-at-rest",
                "Encrypt local data at rest",
                "Encrypt saved conversations, the user profile, the anchor vocabulary, \
                 and workspace data on disk with your Windows account key (DPAPI). On by \
                 default; turn off only if you already use full-disk encryption.",
                encryption_at_rest,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.toggle_encryption_at_rest(cx)),
            ),
        )
        .child(
            control(div(), "settings-privacy-reset")
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
                    cx.listener(|this, _ev, _window, cx| this.reset_privacy_warnings(cx)),
                )
                .child(SharedString::from("Reset privacy warnings")),
        )
}

/// The first-time "Allow online model search?" modal. A dimmed backdrop
/// (occluding clicks to the settings behind) with a centered dialog card:
/// title, the privacy explanation, a "Don't show again" checkbox, and
/// Cancel / Enable buttons. Rendered by the panel root as an absolute
/// overlay only while armed.
pub fn hf_privacy_modal(dont_show_again: bool, cx: &mut Cx) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        // Dim backdrop — translucent black, passed straight (the `pack`
        // idiom would drop the alpha and render it opaque).
        .bg(gpui::rgba(0x00_00_00_99))
        .child(
            div()
                .w(px(440.0))
                .bg(rgb(pack(SURFACE_800)))
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .rounded(px(8.0))
                .shadow_lg()
                .p_5()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::BASE))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from("Allow online model search?")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Wylde will query HuggingFace's public API \
                             (https://huggingface.co/api/models) when you search for models \
                             not in the curated catalog. Each query sends your search term to \
                             HuggingFace; no other data is shared. The connection is HTTPS. You \
                             can disable this anytime in Settings.",
                        )),
                )
                .child(modal_checkbox_row(dont_show_again, cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(
                            modal_button("settings-hf-modal-cancel", "Cancel", false)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _ev, _window, cx| this.cancel_hf_modal(cx)),
                                ),
                        )
                        .child(
                            modal_button("settings-hf-modal-enable", "Enable", true).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, cx| this.confirm_hf_modal(cx)),
                            ),
                        ),
                ),
        )
}

/// The modal's "Don't show this warning again" checkbox row — a clickable
/// box glyph (filled brand when checked) + label.
fn modal_checkbox_row(checked: bool, cx: &mut Cx) -> Stateful<gpui::Div> {
    let (glyph, bg, border) = if checked {
        ("✓", BRAND, BORDER_EMPHASIS)
    } else {
        ("", SURFACE_900, BORDER_DEFAULT)
    };
    control(div(), "settings-hf-modal-dontshow")
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _window, cx| this.toggle_hf_dont_show_again(cx)),
        )
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.0))
                .border_1()
                .border_color(rgb(pack(border)))
                .bg(rgb(pack(bg)))
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(glyph)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from("Don't show this warning again")),
        )
}

/// A modal action button. `primary` fills it with the brand colour (the
/// "Enable" affirmative); otherwise it's an outline (the "Cancel").
fn modal_button(id: impl Into<ElementId>, label: &str, primary: bool) -> Stateful<gpui::Div> {
    let mut b = div()
        .id(id.into())
        .cursor_pointer()
        .rounded(px(4.0))
        .border_1()
        .px_3()
        .py(px(4.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(label.to_owned()));
    if primary {
        b = b
            .bg(rgb(pack(BRAND)))
            .border_color(rgb(pack(BORDER_EMPHASIS)));
    } else {
        b = b
            .bg(rgb(pack(SURFACE_900)))
            .border_color(rgb(pack(BORDER_DEFAULT)));
    }
    b
}

/// Consent modal shown when the user turns ON automatic update checks
/// (req 4). Honest about both facts that matter to the "completely
/// isolated" default: the weekly outbound contact, *and* that nothing is
/// pulled or applied without an explicit Accept on the changelog card
/// (the download-on-Accept model). Cloned from [`hf_privacy_modal`].
pub fn auto_check_consent_modal(cx: &mut Cx) -> gpui::Div {
    modal_shell(
        "Turn on automatic update checks?",
        "Wylde will contact GitHub about once a week to check whether a new version is \
         available. Nothing is downloaded or installed automatically: if an update is found, \
         Wylde shows you the changelog and asks you to Accept before it downloads or installs \
         anything. You can turn this off anytime in Settings.",
        ("settings-updates-auto-cancel", "Cancel"),
        ("settings-updates-auto-enable", "Enable"),
        cx.listener(|this, _ev, _window, cx| this.cancel_auto_check_modal(cx)),
        cx.listener(|this, _ev, _window, cx| this.confirm_auto_check_modal(cx)),
    )
}

/// Warning modal shown when switching TO the Experimental branch (req 6).
/// The body is the maintainer's copy, verbatim — do not paraphrase or fix casing.
/// Fires only on stable → experimental; switching back to Stable is free.
pub fn channel_warning_modal(cx: &mut Cx) -> gpui::Div {
    modal_shell(
        "Switch to the Experimental branch?",
        "The experimental branch is for testing new features and may contain significant bugs. \
         posting any found bugs on the GitHub page while using the branch helps the development \
         of the software",
        ("settings-updates-channel-cancel", "Cancel"),
        ("settings-updates-channel-confirm", "Switch to Experimental"),
        cx.listener(|this, _ev, _window, cx| this.cancel_channel_warning(cx)),
        cx.listener(|this, _ev, _window, cx| this.confirm_channel_warning(cx)),
    )
}

/// Shared two-button confirm-modal chrome (title + body + Cancel/primary),
/// factored out of the HuggingFace modal so the updater consent + channel
/// warning share one layout. The primary (right) button is the affirmative.
#[allow(clippy::type_complexity)]
fn modal_shell(
    title: &str,
    body: &str,
    cancel: (&'static str, &str),
    confirm: (&'static str, &str),
    on_cancel: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    on_confirm: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00_00_00_99))
        .child(
            div()
                .w(px(440.0))
                .bg(rgb(pack(SURFACE_800)))
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .rounded(px(8.0))
                .shadow_lg()
                .p_5()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::BASE))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(title.to_owned())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(body.to_owned())),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(
                            modal_button(cancel.0, cancel.1, false)
                                .on_mouse_down(MouseButton::Left, on_cancel),
                        )
                        .child(
                            modal_button(confirm.0, confirm.1, true)
                                .on_mouse_down(MouseButton::Left, on_confirm),
                        ),
                ),
        )
}

// ── Profile / Rules section (Thought Bubble System Slice D) ──────────

/// A small action button used by the proposal rows. `primary` styles it
/// as the accept affordance; the others are neutral-bordered.
fn profile_button(id: impl Into<ElementId>, label: &str, primary: bool) -> Stateful<gpui::Div> {
    let mut b = div()
        .id(id.into())
        .cursor_pointer()
        .rounded(px(4.0))
        .border_1()
        .px_2()
        .py(px(2.0))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(label.to_owned()));
    if primary {
        b = b
            .bg(rgb(pack(BRAND)))
            .border_color(rgb(pack(BORDER_EMPHASIS)));
    } else {
        b = b
            .bg(rgb(pack(SURFACE_900)))
            .border_color(rgb(pack(BORDER_DEFAULT)));
    }
    b
}

/// One editable profile field: label over its text input.
fn profile_field_row(label: &str, input: &gpui::Entity<wylde_gpui_input::TextInput>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(label.to_owned())),
        )
        .child(input.clone())
}

/// Read-only chip row for preferences / recurring topics — the bits the
/// section surfaces but doesn't structurally edit in v1 (the free-text
/// rules are the primary editable lever).
fn profile_readonly_block(title: &str, lines: &[String]) -> gpui::Div {
    let mut c = div().flex().flex_col().gap_1().child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(title.to_owned())),
    );
    for line in lines {
        c = c.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(line.clone())),
        );
    }
    c
}

/// One pending proposal card: the field + a current→proposed diff +
/// rationale + confidence, with Accept / Edit / Reject buttons.
fn proposal_card(p: &crate::ipc::ProfileProposal, cx: &mut Cx) -> gpui::Div {
    let current = p.current.clone().unwrap_or_else(|| "(unset)".to_owned());
    let diff = format!("{}: {} → {}", p.field, current, p.proposed);
    let meta = format!(
        "{}  ·  confidence {:.0}%",
        if p.rationale.is_empty() {
            "Proposed update"
        } else {
            p.rationale.as_str()
        },
        (p.confidence * 100.0).round()
    );

    let id_accept = p.id.clone();
    let id_reject = p.id.clone();
    let proposal_for_edit = p.clone();

    let mut buttons = div().flex().flex_row().gap_2().items_center().child(
        profile_button(
            ElementId::Name(format!("profile-prop-accept::{}", p.id).into()),
            "Accept",
            true,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, _window, cx| {
                this.accept_profile_proposal(id_accept.clone(), cx)
            }),
        ),
    );
    // "Edit" pre-fills the matching field input with the proposed value
    // and accepts the proposal, so the user lands with it in the editor
    // ready to tweak. Only meaningful for the text fields the section
    // edits; for others it behaves like Accept.
    if proposal_for_edit.is_text_field() {
        buttons = buttons.child(
            profile_button(
                ElementId::Name(format!("profile-prop-edit::{}", p.id).into()),
                "Edit",
                false,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    this.edit_profile_proposal(proposal_for_edit.clone(), cx)
                }),
            ),
        );
    }
    buttons = buttons.child(
        profile_button(
            ElementId::Name(format!("profile-prop-reject::{}", p.id).into()),
            "Reject",
            false,
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, _window, cx| {
                this.reject_profile_proposal(id_reject.clone(), cx)
            }),
        ),
    );

    div()
        .flex()
        .flex_col()
        .gap_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .bg(rgb(pack(SURFACE_900)))
        .p_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(diff)),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(meta)),
        )
        .child(buttons)
}

/// The "Profile / Rules" section (Build Order §3 / Plan v2 §6 Slice D):
/// the editable user profile + the pending LLM proposal queue.
///
/// `name_input` / `style_input` / `rules_input` are the panel's three
/// profile `TextInput` entities (synced by the panel); an empty `None`
/// (inputs not yet minted in a headless test) renders the header only.
pub fn profile_rules_section(
    name_input: Option<&gpui::Entity<wylde_gpui_input::TextInput>>,
    style_input: Option<&gpui::Entity<wylde_gpui_input::TextInput>>,
    rules_input: Option<&gpui::Entity<wylde_gpui_input::TextInput>>,
    profile: &crate::ipc::UserProfile,
    proposals: &[crate::ipc::ProfileProposal],
    cx: &mut Cx,
) -> gpui::Div {
    let mut c = card().child(section_title(
        "Profile / Rules",
        "Who you are and how you want the assistant to behave. Rules are followed verbatim. \
         The assistant may propose updates below — you accept, edit, or reject each.",
    ));

    if let Some(input) = name_input {
        c = c.child(profile_field_row("Name", input));
    }
    if let Some(input) = style_input {
        c = c.child(profile_field_row("Style (one line)", input));
    }
    if let Some(input) = rules_input {
        c = c.child(profile_field_row(
            "Rules (free text — followed verbatim)",
            input,
        ));
    }

    if !profile.preferences.is_empty() {
        let lines: Vec<String> = profile
            .preferences
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        c = c.child(profile_readonly_block("Preferences", &lines));
    }
    if !profile.recurring_topics.is_empty() {
        c = c.child(profile_readonly_block(
            "Recurring topics",
            &[profile.recurring_topics.join(", ")],
        ));
    }

    // Pending proposals.
    if proposals.is_empty() {
        c = c.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "No pending proposals — the assistant will suggest updates as it learns.",
                )),
        );
    } else {
        c = c.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from(format!(
                    "Proposed updates ({})",
                    proposals.len()
                ))),
        );
        for p in proposals {
            c = c.child(proposal_card(p, cx));
        }
    }
    c
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
    fn modal_button_renders_primary_and_secondary() {
        let _ = modal_button("x-enable", "Enable", true);
        let _ = modal_button("x-cancel", "Cancel", false);
    }

    #[test]
    fn state_pill_renders_value() {
        let _ = state_pill("weekly");
    }

    #[test]
    fn channel_label_relabels_beta_as_experimental() {
        // The user-facing word is "Experimental"; the wire value stays "beta".
        assert_eq!(channel_label("beta"), "Experimental");
        assert_eq!(channel_label("stable"), "Stable");
        // Unknown/legacy is conservative — never surface "Experimental".
        assert_eq!(channel_label("nightly"), "Stable");
    }

    #[test]
    fn changelog_body_renders_empty_and_populated() {
        // A stub/empty changelog still renders (no panic, sensible fallback).
        let _ = changelog_body("");
        let _ = changelog_body("   \n  ");
        // A multi-line changelog renders each line.
        let _ = changelog_body("## v0.3.0\n- fixed a thing\n- added another");
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
    fn voice_labels_map_known_and_fallback() {
        assert_eq!(voice_mode_label("always_on"), "Always-on");
        assert_eq!(voice_mode_label("push_to_talk"), "Push-to-talk");
        assert_eq!(voice_mode_label("garbage"), "Push-to-talk");
        assert_eq!(backend_label("cpu"), "CPU");
        assert_eq!(backend_label("npu"), "NPU");
        assert_eq!(backend_label("auto"), "Auto");
        assert_eq!(backend_label("???"), "Auto");
        assert_eq!(vad_label("low"), "Low");
        assert_eq!(vad_label("high"), "High");
        assert_eq!(vad_label("medium"), "Medium");
        assert_eq!(vad_label("???"), "Medium");
    }

    #[test]
    fn device_label_falls_back_to_system_default() {
        assert_eq!(device_label(Some("USB Mic")), "USB Mic");
        assert_eq!(device_label(None), crate::ipc::DEVICE_SYSTEM_DEFAULT);
        assert_eq!(device_label(Some("")), crate::ipc::DEVICE_SYSTEM_DEFAULT);
    }

    #[test]
    fn wake_word_label_strips_vendor_prefix() {
        assert_eq!(wake_word_label("openWakeWord/hey-jarvis"), "hey-jarvis");
        assert_eq!(wake_word_label("alexa"), "alexa");
    }

    #[test]
    fn test_mic_line_renders_each_state() {
        use crate::ipc::VoiceTestResult;
        assert!(test_mic_line(&VoiceTest::Idle).is_none());
        assert!(test_mic_line(&VoiceTest::Running).is_some());
        assert!(test_mic_line(&VoiceTest::Failed("no device".into())).is_some());
        assert!(test_mic_line(&VoiceTest::Done(VoiceTestResult {
            rms: 0.1,
            peak: 0.5,
            transcript: "hello there".into(),
            note: None,
        }))
        .is_some());
        // Empty transcript with a note still renders.
        assert!(test_mic_line(&VoiceTest::Done(VoiceTestResult {
            rms: 0.0,
            peak: 0.0,
            transcript: String::new(),
            note: Some("no model".into()),
        }))
        .is_some());
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
