//! Composer UI (Slice F) — mounts the symbol-aware recognition surfaces
//! into the InferenceBar: the per-word **chip strip** + message-level
//! context chip, the **disambiguation** dropdown, the **curate-before-send**
//! popover, and the **Ctrl+P symbol palette**. All styling from the locked
//! Visual Style v1 `chat_composer` section (the app renders dark; the
//! `_dark` variants apply, matching the panel's `wylde_theme` surfaces).
//!
//! State lives on [`ChatPanel::composer`]; the async scan/lookup plumbing is
//! in `chat_panel.rs` (`schedule_composer_scan` / `schedule_palette_query`).

use gpui::{div, prelude::*, px, rgb, Context, MouseButton, MouseDownEvent, SharedString};
use wylde_theme::colors::{BORDER_DEFAULT, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, FAMILY_INTER};

use crate::chat_panel::{pack, ChatPanel};
use crate::composer::highlight::{composer_theme, hex, ComposerTheme};
use crate::composer::{curation, disambiguator};

/// Append the composer surfaces to the InferenceBar (between the pill row
/// and the prompt row). No-ops into the same `bar` when there's nothing to
/// show.
pub(crate) fn mount(
    mut bar: gpui::Div,
    panel: &ChatPanel,
    cx: &mut Context<ChatPanel>,
) -> gpui::Div {
    let theme = composer_theme();
    if let Some(strip) = chip_strip(panel, &theme, cx) {
        bar = bar.child(strip);
    }
    if let Some(dropdown) = disambiguation_dropdown(panel, cx) {
        bar = bar.child(dropdown);
    }
    if let Some(popover) = curation_popover(panel, cx) {
        bar = bar.child(popover);
    }
    if let Some(palette) = palette_overlay(panel, cx) {
        bar = bar.child(palette);
    }
    bar
}

/// The per-word chip strip + right-aligned context chip (Plan §5.1).
fn chip_strip(
    panel: &ChatPanel,
    theme: &ComposerTheme,
    cx: &mut Context<ChatPanel>,
) -> Option<gpui::Div> {
    let chip_state = panel.composer.chip();
    let has_chips = panel
        .composer
        .words
        .iter()
        .any(|w| w.chip_label().is_some());
    if !has_chips && !panel.composer.degraded {
        return None;
    }

    let style = &theme.per_word_chip;
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_1()
        .font_family(FAMILY_INTER);

    for (i, w) in panel.composer.words.iter().enumerate() {
        let Some(label) = w.chip_label() else {
            continue;
        };
        let ambiguous = w.is_ambiguous();
        let (bg, fg) = if ambiguous {
            (
                hex(&style.ambiguous_state.background_dark),
                hex(&style.ambiguous_state.text_dark),
            )
        } else {
            (hex(&style.background_dark), hex(&style.text_dark))
        };
        let word_label = if w.excluded {
            format!("{} ✕", w.token.text)
        } else {
            w.token.text.clone()
        };
        row = row.child(
            div()
                .id(("composer-word-chip", i))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(style.height_px))
                .px(px(style.horizontal_padding_px))
                .rounded(px(style.border_radius_px))
                .bg(rgb(bg))
                .text_size(px(style.font_size_px))
                .text_color(rgb(fg))
                .cursor_pointer()
                .child(SharedString::from(word_label))
                .child(SharedString::from(label))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        // Ambiguous → disambiguate; resolved → toggle the
                        // word in/out of the send (inline curation).
                        if this.composer.words.get(i).is_some_and(|w| w.is_ambiguous()) {
                            this.composer.disambiguating = Some(i);
                            this.composer.curating = false;
                        } else {
                            this.composer.toggle_excluded(i);
                        }
                        cx.notify();
                    }),
                ),
        );
    }

    row = row.child(div().flex_1());

    if panel.composer.degraded {
        row = row.child(
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("recognition offline")),
        );
    }

    if let Some(label) = chip_state.label() {
        let ambiguous = chip_state.is_ambiguous();
        let (bg, fg) = if ambiguous {
            (
                hex(&theme.per_word_chip.ambiguous_state.background_dark),
                hex(&theme.per_word_chip.ambiguous_state.text_dark),
            )
        } else {
            (
                hex(&theme.per_word_chip.background_dark),
                hex(&theme.per_word_chip.text_dark),
            )
        };
        row = row.child(
            div()
                .id("composer-context-chip")
                .h(px(theme.per_word_chip.height_px))
                .px(px(theme.per_word_chip.horizontal_padding_px))
                .rounded(px(theme.per_word_chip.border_radius_px))
                .bg(rgb(bg))
                .text_size(px(theme.per_word_chip.font_size_px))
                .text_color(rgb(fg))
                .cursor_pointer()
                .flex()
                .items_center()
                .child(SharedString::from(label))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        if ambiguous {
                            this.composer.disambiguating = this.composer.first_ambiguous();
                            this.composer.curating = false;
                        } else {
                            this.composer.curating = !this.composer.curating;
                            this.composer.disambiguating = None;
                        }
                        cx.notify();
                    }),
                ),
        );
    }

    Some(row)
}

/// The `?N` word's candidate dropdown (Build Order `disambiguator.rs`).
fn disambiguation_dropdown(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Option<gpui::Div> {
    let idx = panel.composer.disambiguating?;
    let word = panel.composer.words.get(idx)?;
    let view = disambiguator::view_for(word)?;

    let mut card = panel_card().child(
        div()
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(format!(
                "Which \"{}\" did you mean?",
                view.word
            ))),
    );
    for (ri, r) in view.rows.iter().enumerate() {
        let id = r.id.clone();
        card = card.child(
            div()
                .id(("composer-disambig-row", ri))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_700)))
                .cursor_pointer()
                .child(
                    div()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(r.title.clone())),
                )
                .child(
                    div()
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(r.detail.clone())),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        this.composer.resolve(idx, &id);
                        cx.notify();
                    }),
                ),
        );
    }
    if view.offer_anchor {
        card = card.child(
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Anchor this? — vocabulary anchors land with Slice N",
                )),
        );
    }
    Some(card)
}

/// Curate-before-send (Build Order `curation.rs`): the `N ▸` chip's list.
fn curation_popover(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Option<gpui::Div> {
    if !panel.composer.curating {
        return None;
    }
    let view = curation::view_for(&panel.composer.words);
    if view.items.is_empty() {
        return None;
    }
    let mut card = panel_card().child(
        div()
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(format!(
                "Context for this message — {} of {} included",
                view.included_count(),
                view.items.len()
            ))),
    );
    for (ri, item) in view.items.iter().enumerate() {
        let word_idx = item.word_idx;
        let mark = if item.included { "☑" } else { "☐" };
        card = card.child(
            div()
                .id(("composer-curation-row", ri))
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_700)))
                .cursor_pointer()
                .child(SharedString::from(mark.to_owned()))
                .child(
                    div()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(item.word.clone())),
                )
                .child(
                    div()
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(item.summary.clone())),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        this.composer.toggle_excluded(word_idx);
                        cx.notify();
                    }),
                ),
        );
    }
    card = card.child(
        div()
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(
                "Exclusions apply to this message only (durable ignores land with Slice M)",
            )),
    );
    Some(card)
}

/// The Ctrl+P symbol palette: query input + ranked hits; click (or Enter)
/// inserts an `@symbol` reference at the prompt cursor.
fn palette_overlay(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Option<gpui::Div> {
    let palette = panel.composer.palette.as_ref()?;
    let mut card = panel_card()
        .child(
            div()
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(
                    "Symbol palette — Enter inserts the top hit · Esc closes",
                )),
        )
        .child(div().child(panel.palette_input.clone()));
    for (ri, hit) in palette.hits.iter().enumerate() {
        let name = hit.name.clone();
        let selected = ri == palette.selected;
        let bg = if selected { SURFACE_700 } else { SURFACE_800 };
        card = card.child(
            div()
                .id(("composer-palette-row", ri))
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(bg)))
                .cursor_pointer()
                .child(
                    div()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(format!("{} · {}", hit.name, hit.kind))),
                )
                .child(
                    div()
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(format!("{}:{}", hit.file, hit.line))),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(
                        move |this: &mut ChatPanel, _ev: &MouseDownEvent, window, cx| {
                            this.accept_palette_symbol(&name, window, cx);
                        },
                    ),
                ),
        );
    }
    if palette.hits.is_empty() && !palette.query.is_empty() {
        card = card.child(
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("no symbols match")),
        );
    }
    Some(card)
}

/// Shared popover chrome.
fn panel_card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded(px(6.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .font_family(FAMILY_INTER)
}
