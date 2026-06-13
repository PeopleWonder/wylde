//! Composer UI (Slice F) — mounts the symbol-aware recognition surfaces
//! into the InferenceBar: the per-word **chip strip** + message-level
//! context chip, the **disambiguation** dropdown, the **curate-before-send**
//! popover, and the **Ctrl+P symbol palette**. All styling from the locked
//! Visual Style v1 `chat_composer` section (the app renders dark; the
//! `_dark` variants apply, matching the panel's `wylde_theme` surfaces).
//!
//! State lives on [`ChatPanel::composer`]; the async scan/lookup plumbing is
//! in `chat_panel.rs` (`schedule_composer_scan` / `schedule_palette_query`).

use gpui::{
    canvas, div, prelude::*, px, rgb, Bounds, Context, MouseButton, MouseDownEvent, Pixels,
    SharedString,
};
use wylde_theme::colors::{BORDER_DEFAULT, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, FAMILY_INTER};

use crate::chat_panel::{pack, ChatPanel};
use crate::composer::bubbles::{self, BubbleKind};
use crate::composer::highlight::{composer_theme, hex, ComposerTheme};
use crate::composer::{curation, disambiguator, IgnoreTierTag};

/// Append the composer surfaces to the InferenceBar (between the pill row
/// and the prompt row). No-ops into the same `bar` when there's nothing to
/// show.
pub(crate) fn mount(
    mut bar: gpui::Div,
    panel: &ChatPanel,
    cx: &mut Context<ChatPanel>,
) -> gpui::Div {
    let theme = composer_theme();
    // The Thought-Bubble strip floats at the top of the bar (§5.2), above
    // the chip strip, tethered toward the input below.
    if let Some(strip) = bubble_strip(panel, &theme, cx) {
        bar = bar.child(strip);
    }
    if let Some(strip) = chip_strip(panel, &theme, cx) {
        bar = bar.child(strip);
    }
    if let Some(dropdown) = disambiguation_dropdown(panel, cx) {
        bar = bar.child(dropdown);
    }
    if let Some(menu) = ignore_menu(panel, cx) {
        bar = bar.child(menu);
    }
    if let Some(offer) = anchor_offer(panel, cx) {
        bar = bar.child(offer);
    }
    if let Some(popover) = curation_popover(panel, cx) {
        bar = bar.child(popover);
    }
    if let Some(palette) = palette_overlay(panel, cx) {
        bar = bar.child(palette);
    }
    bar
}

// ── The Thought-Bubble strip (Plan §5.2–5.5) ─────────────────────────────

/// Strip height: compact bubbles + tether headroom.
const BUBBLE_STRIP_H: f32 = 72.0;
/// Compact bubble diameter (§5.3 ~24×24).
const BUBBLE_D: f32 = 24.0;
/// Horizontal spacing between bubble slots.
const BUBBLE_GAP: f32 = 34.0;
/// Bubbles sit this far below the strip top (tethers run beneath them).
const BUBBLE_TOP: f32 = 6.0;

/// The floating bubble layer for the open word's set: a relative strip
/// holding a tether canvas (which also captures the strip's window origin
/// — the graph panel's CanvasRect pattern), the bubble circles, the
/// expanded card, and the shared right-click menu.
fn bubble_strip(
    panel: &ChatPanel,
    theme: &ComposerTheme,
    cx: &mut Context<ChatPanel>,
) -> Option<gpui::Stateful<gpui::Div>> {
    let word_idx = panel.bubbles.word_idx?;
    let word = panel.composer.words.get(word_idx)?;
    let n = panel.bubbles.bubbles.len();

    // The word's on-screen anchor x (strip-relative) via the input's glyph
    // metrics + the origin the tether canvas captured last frame. Before
    // the first capture (or pre-paint) everything lands mid-strip.
    let (ox, _oy, ow) = panel.bubble_strip_origin;
    let word_rect = panel
        .prompt_input
        .read(cx)
        .rects_for_range(word.token.start..word.token.end)
        .into_iter()
        .next();
    let strip_w = if ow > 0.0 { ow } else { 600.0 };
    let word_x_rel = word_rect
        .map(|r| f32::from(r.origin.x) + f32::from(r.size.width) / 2.0 - ox)
        .unwrap_or(strip_w / 2.0)
        .clamp(0.0, strip_w);
    let xs = bubbles::slot_xs(n, word_x_rel, BUBBLE_GAP, strip_w);

    let mut strip = div()
        .id("chat-bubble-strip")
        .relative()
        .w_full()
        .h(px(BUBBLE_STRIP_H));

    // Tether canvas — paints a dotted line from each bubble's underside
    // toward the word's x at the strip's bottom edge (a dotted reading of
    // the theme's 4-4 dash; angled dashes need no path machinery this
    // way), and captures the strip's window-absolute origin for the next
    // frame's slot math.
    let entity = cx.entity();
    let tether_rgb = hex(&theme.tether_line.color_dark);
    let tether_opacity = theme.tether_line.opacity;
    let tether_px = theme.tether_line.thickness_px.max(1.0);
    let xs_for_canvas = xs.clone();
    strip = strip.child(
        canvas(
            move |bounds: Bounds<Pixels>, _window, app: &mut gpui::App| {
                let (bx, by, bw, bh) = (
                    f32::from(bounds.origin.x),
                    f32::from(bounds.origin.y),
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                );
                entity.update(app, |panel, _| {
                    panel.bubble_strip_origin = (bx, by, bw);
                });
                // Dotted tether segments in window coords.
                let mut dots: Vec<(f32, f32)> = Vec::new();
                for x in &xs_for_canvas {
                    let (x0, y0) = (bx + x + BUBBLE_D / 2.0, by + BUBBLE_TOP + BUBBLE_D);
                    let (x1, y1) = (bx + word_x_rel, by + bh);
                    let (dx, dy) = (x1 - x0, y1 - y0);
                    let len = (dx * dx + dy * dy).sqrt().max(1.0);
                    let step = 8.0; // 4 on / 4 off, as dots
                    let count = (len / step) as usize;
                    for k in 0..=count {
                        let t = (k as f32 * step / len).min(1.0);
                        dots.push((x0 + dx * t, y0 + dy * t));
                    }
                }
                dots
            },
            move |_bounds, dots: Vec<(f32, f32)>, window, _app| {
                let mut color: gpui::Rgba = rgb(tether_rgb);
                color.a = tether_opacity;
                for (x, y) in dots {
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            gpui::point(px(x), px(y)),
                            gpui::size(px(tether_px), px(tether_px)),
                        ),
                        color,
                    ));
                }
            },
        )
        .absolute()
        .size_full(),
    );

    // The bubbles. Excluded words gray their whole set (§5.4 "settles
    // flat"); pinned bubbles carry the tether-accent ring.
    let chip = &theme.per_word_chip;
    let excluded = word.excluded;
    for (i, bubble) in panel.bubbles.bubbles.iter().enumerate() {
        let glyph = match bubble.kind {
            BubbleKind::Anchor { .. } => "{}",
            BubbleKind::Symbol { .. } => "ƒ",
        };
        let pinned = panel.bubbles.pinned.contains(&bubble.label);
        let mut b = div()
            .id(("chat-bubble", i))
            .absolute()
            .left(px(xs.get(i).copied().unwrap_or(0.0)))
            .top(px(BUBBLE_TOP))
            .w(px(BUBBLE_D))
            .h(px(BUBBLE_D))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .border_1()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    this.expand_bubble(i, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    this.bubbles.menu = Some(i);
                    cx.notify();
                }),
            )
            .child(SharedString::from(glyph));
        if excluded {
            b = b
                .bg(rgb(pack(SURFACE_700)))
                .text_color(rgb(pack(TEXT_MUTED)))
                .border_color(rgb(pack(TEXT_MUTED)));
        } else {
            b = b
                .bg(rgb(hex(&chip.background_dark)))
                .text_color(rgb(hex(&chip.text_dark)))
                .border_color(rgb(if pinned {
                    hex(&theme.tether_line.color_dark)
                } else {
                    hex(&chip.background_dark)
                }));
        }
        strip = strip.child(b);
    }

    // Collapse affordance (Esc also collapses; double-click-outside is a
    // follow-up — needs a global click hook).
    strip = strip.child(
        div()
            .id("chat-bubble-collapse")
            .absolute()
            .top(px(BUBBLE_TOP))
            .right_2()
            .px_1()
            .cursor_pointer()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    this.bubbles.collapse();
                    cx.notify();
                }),
            )
            .child(SharedString::from("collapse ✕")),
    );

    if let Some(card) = bubble_card(panel, theme, &xs, cx) {
        strip = strip.child(card);
    }
    if let Some(menu) = bubble_menu(panel, word, &xs, cx) {
        strip = strip.child(menu);
    }
    Some(strip)
}

/// The expanded ~300×200 drill-in card (§5.3/§5.4): identity line, ✕/↺ +
/// 📌, then the anchor description or the symbol's body peek + blame +
/// caller/callee/type lists (read-only v1 — peer-to-peer connect is the
/// documented follow-up).
fn bubble_card(
    panel: &ChatPanel,
    theme: &ComposerTheme,
    xs: &[f32],
    cx: &mut Context<ChatPanel>,
) -> Option<gpui::Stateful<gpui::Div>> {
    let ix = panel.bubbles.expanded?;
    let bubble = panel.bubbles.bubbles.get(ix)?;
    let word_idx = panel.bubbles.word_idx?;
    let word = panel.composer.words.get(word_idx)?;
    let left = (xs.get(ix).copied().unwrap_or(0.0) - 140.0).max(0.0);
    let pinned = panel.bubbles.pinned.contains(&bubble.label);

    let mut card = div()
        .id("chat-bubble-card")
        .absolute()
        .left(px(left))
        .top(px(BUBBLE_TOP + BUBBLE_D + 6.0))
        .w(px(300.0))
        .max_h(px(200.0))
        .overflow_y_scroll()
        .p_2()
        .rounded(px(6.0))
        .bg(rgb(pack(SURFACE_700)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .flex()
        .flex_col()
        .gap_1()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)));

    // Header: label + 📌 + ✕/↺.
    let label = bubble.label.clone();
    let pin_label = if pinned { "📌 pinned" } else { "📌 pin" };
    let excl_label = if word.excluded {
        "↺ restore"
    } else {
        "✕ exclude"
    };
    card = card.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(bubble.label.clone())),
            )
            .child(
                div()
                    .id("chat-bubble-pin")
                    .cursor_pointer()
                    .text_color(rgb(if pinned {
                        hex(&theme.tether_line.color_dark)
                    } else {
                        pack(TEXT_MUTED)
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            this.toggle_bubble_pin_undoable(&label, cx);
                            cx.notify();
                        }),
                    )
                    .child(SharedString::from(pin_label)),
            )
            .child(
                div()
                    .id("chat-bubble-exclude")
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            if let Some(wi) = this.bubbles.word_idx {
                                this.toggle_word_excluded_undoable(wi, cx);
                            }
                            cx.notify();
                        }),
                    )
                    .child(SharedString::from(excl_label)),
            ),
    );

    match &bubble.kind {
        BubbleKind::Anchor { description } => {
            card = card.child(SharedString::from(format!("anchor · {description}")));
        }
        BubbleKind::Symbol { file, line, .. } => {
            card = card.child(SharedString::from(format!("{file}:{line}")));
            if let Some(ctxd) = &panel.bubbles.context {
                if let Some(preview) = &ctxd.body_preview {
                    card = card.child(SharedString::from(preview.clone()));
                }
                if let Some(blame) = &ctxd.blame_line {
                    card = card.child(SharedString::from(blame.clone()));
                }
                for (title, list) in [
                    ("callers", &ctxd.callers),
                    ("callees", &ctxd.callees),
                    ("types", &ctxd.types_used),
                ] {
                    if !list.is_empty() {
                        card =
                            card.child(SharedString::from(format!("{title}: {}", list.join(", "))));
                    }
                }
            } else if panel.bubbles.context_failed {
                card = card.child(SharedString::from("context unavailable"));
            } else {
                card = card.child(SharedString::from("loading context…"));
            }
        }
    }
    Some(card)
}

/// The shared right-click menu (Plan §6 — `wylde_anchor_actions::anchor_menu`,
/// its first renderer). Rows the composer can't route yet (Add Connection's
/// drawing mode, Edit Definition's cross-panel hop) are filtered out rather
/// than greyed.
fn bubble_menu(
    panel: &ChatPanel,
    word: &crate::composer::WordRecognition,
    xs: &[f32],
    cx: &mut Context<ChatPanel>,
) -> Option<gpui::Stateful<gpui::Div>> {
    use wylde_anchor_actions::{anchor_menu, IgnoreTier, MenuAction, MenuContext};
    let ix = panel.bubbles.menu?;
    let bubble = panel.bubbles.bubbles.get(ix)?;

    let ignored_tiers = word
        .ignored_tiers
        .iter()
        .map(|t| match t {
            IgnoreTierTag::Conversation => IgnoreTier::Conversation,
            IgnoreTierTag::Workspace => IgnoreTier::Workspace,
            IgnoreTierTag::Global => IgnoreTier::Global,
        })
        .collect();
    let menu_ctx = MenuContext {
        identifier: bubble.label.clone(),
        excluded: word.excluded,
        ignored_tiers,
        // v1: promotion stays a Vocabulary-tab flow → no Promote row here.
        is_workspace_scoped: false,
        pinned: panel.bubbles.pinned.contains(&bubble.label),
    };

    let mut menu = div()
        .id("chat-bubble-menu")
        .absolute()
        .left(px((xs.get(ix).copied().unwrap_or(0.0)).max(0.0)))
        .top(px(BUBBLE_TOP + BUBBLE_D + 6.0))
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .flex()
        .flex_col()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_PRIMARY)));

    for (row_ix, action) in anchor_menu(&menu_ctx)
        .into_iter()
        .filter(|a| {
            !matches!(
                a,
                MenuAction::AddConnection
                    | MenuAction::EditDefinition
                    | MenuAction::PromoteToGlobal
            )
        })
        .enumerate()
    {
        let label = action.label();
        menu = menu.child(
            div()
                .id(("chat-bubble-menu-row", row_ix))
                .px_2()
                .py_1()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        this.apply_bubble_menu_action(action.clone(), cx);
                    }),
                )
                .child(SharedString::from(label)),
        );
    }
    Some(menu)
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
        // ✕ = per-message exclude; ↺ = ignored, click to reactivate (Plan
        // §5.8 — ignored still highlights + counts, rides deselected).
        let word_label = if w.excluded {
            format!("{} ✕", w.token.text)
        } else if w.is_ignored() && !w.reactivated {
            format!("{} ↺", w.token.text)
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
                        // word in/out of the send (✕, or ↺ when ignored).
                        // §5.2: a chip click SPAWNS the word's bubble set
                        // (ambiguous words disambiguate first). Exclusion
                        // moved to the bubble's ✕ / the curation popover.
                        if this.composer.words.get(i).is_some_and(|w| w.is_ambiguous()) {
                            this.composer.disambiguating = Some(i);
                            this.composer.curating = false;
                        } else {
                            this.open_word_bubbles(i, cx);
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        // Right-click → the 3-tier ignore menu (Slice M).
                        this.composer.ignore_menu = Some(i);
                        this.composer.disambiguating = None;
                        this.composer.curating = false;
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
                        if this.composer.resolve(idx, &id) {
                            // Slice N: a fresh, anchorable pick with no
                            // anchors yet → offer to mint one.
                            let offer = this.composer.words.get(idx).is_some_and(|w| {
                                w.anchor_count == 0
                                    && crate::composer::is_anchorable_identifier(&w.token.text)
                            });
                            this.composer.anchor_offer = offer.then_some(idx);
                            // Ambiguous → recognized: restyle the underline.
                            this.sync_prompt_highlights(cx);
                        }
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
                    "Pick the right one — you can anchor it as vocabulary right after",
                )),
        );
    }
    Some(card)
}

/// The post-pick "Anchor this?" offer (Slice N): one click mints a symbol
/// anchor for the disambiguated word.
fn anchor_offer(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Option<gpui::Div> {
    let idx = panel.composer.anchor_offer?;
    let word = panel.composer.words.get(idx)?;
    let sym = word.effective_symbol()?;
    let label = format!(
        "Anchor this? Create {{{{{}}}}} → {} ({}:{})",
        word.token.text, sym.name, sym.file, sym.line
    );
    Some(
        panel_card().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .id("composer-anchor-create")
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(rgb(pack(SURFACE_700)))
                        .cursor_pointer()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from("Create anchor"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(
                                move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                                    this.create_anchor_for_word(idx, cx);
                                },
                            ),
                        ),
                )
                .child(
                    div()
                        .id("composer-anchor-dismiss")
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(rgb(pack(SURFACE_800)))
                        .cursor_pointer()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from("Not now"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(
                                move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                                    this.composer.anchor_offer = None;
                                    cx.notify();
                                },
                            ),
                        ),
                ),
        ),
    )
}

/// The right-click ignore menu (Slice M, Plan §5.8): toggle the word's
/// presence in each of the three tiers.
fn ignore_menu(panel: &ChatPanel, cx: &mut Context<ChatPanel>) -> Option<gpui::Div> {
    let idx = panel.composer.ignore_menu?;
    let word = panel.composer.words.get(idx)?;
    let mut card = panel_card().child(
        div()
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(format!(
                "\"{}\" — ignore means default-inactive from now on",
                word.token.text
            ))),
    );
    for (ri, tier) in [
        IgnoreTierTag::Conversation,
        IgnoreTierTag::Workspace,
        IgnoreTierTag::Global,
    ]
    .into_iter()
    .enumerate()
    {
        let active = word.ignored_tiers.contains(&tier);
        let label = if active {
            format!("Stop ignoring in this {}", tier.label())
        } else {
            format!("Ignore in this {}", tier.label())
        };
        card = card.child(
            div()
                .id(("composer-ignore-row", ri))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_700)))
                .cursor_pointer()
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(label))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatPanel, _ev: &MouseDownEvent, _w, cx| {
                        this.toggle_ignore_tier(idx, tier, cx);
                    }),
                ),
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
                        this.toggle_word_excluded_undoable(word_idx, cx);
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
