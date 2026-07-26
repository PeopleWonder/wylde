//! The curate-before-inject **menu** (concept-routing **R2**, plan §4) — the
//! gpui surface that, on the first turn of a conversation, shows the user
//! *exactly what would be injected* and lets them add/remove before injection.
//! Never silent.
//!
//! Lives in the isolated experimental `routing/` folder so it deletes with the
//! feature. The pure logic — building the rows, pre-checking, the budget
//! indicator, the per-conversation cadence — is in [`super::curate_reducer`]
//! ([`CurateMenuModel`] / [`CurateCadence`]); this file is the thin gpui shell:
//! load via `chat.preview_context`, render the rows + budget + actions, and hand
//! the confirmed curated list back to the caller (the Chat composer carries it
//! on `chat.run_turn`).
//!
//! ## Flow (two-phase turn)
//!
//! 1. The composer calls [`CurateMenuView::load_for_turn`] before sending.
//! 2. If routing is OFF, nothing routed, or the cadence says "auto" → the menu
//!    stays closed and [`CurateMenuView::resolved_selection`] holds the set to
//!    inject (remembered, or the default-checked set) — the composer sends
//!    `chat.run_turn` immediately.
//! 3. Else the menu opens (Phase 1 candidates rendered); the user curates and
//!    hits **Inject selected** / **Skip — plain RAG** / **⟳ auto next time**,
//!    which records the choice in the cadence and exposes the curated list.

use gpui::{
    div, prelude::*, px, rgb, Context, IntoElement, MouseButton, MouseDownEvent, Render,
    SharedString, Window,
};
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND, BRAND_LIGHT, DANGER, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY,
    WARNING,
};
use wylde_theme::typography::{size, weight};

use crate::workspaces_panel::pack;

use super::curate_ipc;
use super::curate_reducer::{CurateAnnotation, CurateCadence, CurateMenuModel, RowKind};
use wylde_gui_controls::control;

/// What the composer should do after [`CurateMenuView::load_for_turn`] resolves.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnDecision {
    /// Routing is off / nothing routed / errored — run a plain turn (no
    /// `curated_concepts`).
    PlainTurn,
    /// Auto-applied (cadence): send `chat.run_turn` with these concept ids
    /// immediately, no menu.
    AutoInject(Vec<String>),
    /// The menu is open and blocking — wait for the user to confirm. The
    /// composer holds the send until [`CurateMenuView::resolved_selection`]
    /// becomes `Some`.
    Prompt,
}

/// The curate menu view.
pub struct CurateMenuView {
    /// The routed menu model (rows + budget) — `Some` only while the menu is
    /// open.
    model: Option<CurateMenuModel>,
    /// Per-conversation cadence (interactive first, auto-reuse after).
    cadence: CurateCadence,
    /// The conversation this menu is bound to (for cadence + the run_turn send).
    conversation_id: String,
    /// `true` while the menu is open and awaiting confirmation.
    open: bool,
    /// The decision after the last [`load_for_turn`](Self::load_for_turn).
    decision: TurnDecision,
    /// The curated concept ids the user confirmed (or auto-applied) — what the
    /// composer carries on `chat.run_turn`. `None` until resolved.
    resolved: Option<Vec<String>>,
    /// `curate_before_inject` from the last preview reply (drives cadence).
    curate_before_inject: bool,
    /// Inline status / error.
    status: Option<String>,
    loading: bool,
}

impl Default for CurateMenuView {
    fn default() -> Self {
        Self {
            model: None,
            cadence: CurateCadence::new(),
            conversation_id: String::new(),
            open: false,
            decision: TurnDecision::PlainTurn,
            resolved: None,
            curate_before_inject: true,
            status: None,
            loading: false,
        }
    }
}

impl CurateMenuView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self::default()
    }

    // ── accessors (tests + the composer) ─────────────────────────────────

    /// Whether the blocking menu is currently open.
    pub fn is_open(&self) -> bool {
        self.open
    }
    /// The decision after the last load.
    pub fn decision(&self) -> &TurnDecision {
        &self.decision
    }
    /// The resolved curated concept ids (the run_turn payload), if decided.
    pub fn resolved_selection(&self) -> Option<&Vec<String>> {
        self.resolved.as_ref()
    }
    /// The live menu model (open state).
    pub fn model(&self) -> Option<&CurateMenuModel> {
        self.model.as_ref()
    }
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Phase 1: route the turn's query and decide whether to prompt. Spawns
    /// `chat.preview_context`; on reply, either opens the menu (first turn /
    /// interactive) or auto-resolves the selection (off / auto / nothing routed).
    pub fn load_for_turn(
        &mut self,
        workspace_id: String,
        conversation_id: String,
        user_message: String,
        active_file: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.conversation_id = conversation_id.clone();
        self.resolved = None;
        self.status = None;
        self.loading = true;
        self.open = false;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let reply = curate_ipc::preview_context(
                &workspace_id,
                &conversation_id,
                &user_message,
                active_file.as_deref(),
            )
            .await;
            let _ = this.update(app_cx, |v, cx| {
                v.loading = false;
                v.apply_preview(reply, cx);
            });
        })
        .detach();
    }

    /// Resolve a preview reply into a decision (pure-ish; takes `cx` only to
    /// notify). Split out so a windowed test can drive it without a live pipe.
    pub fn apply_preview(
        &mut self,
        reply: Result<curate_ipc::PreviewReply, String>,
        cx: &mut Context<Self>,
    ) {
        match reply {
            Err(e) => {
                // Preview failed (toggle on but service unreachable) → plain turn.
                self.status = Some(format!("Concept routing unavailable: {e}"));
                self.decide_plain();
            }
            Ok(reply) if !reply.routing_enabled => {
                // Master toggle OFF — no menu, plain turn (today's behaviour).
                self.decide_plain();
            }
            Ok(reply) => {
                self.curate_before_inject = reply.curate;
                let conv = self.conversation_id.clone();
                match reply.candidates {
                    // Nothing routed → raw-RAG fallback, plain turn.
                    None => self.decide_plain(),
                    Some(set) if set.concepts.is_empty() => self.decide_plain(),
                    Some(set) => {
                        let model =
                            CurateMenuModel::from_candidates(&set, reply.inject_token_budget);
                        if self.cadence.should_prompt(&conv, reply.curate) {
                            // Open the blocking menu — never silent.
                            self.model = Some(model);
                            self.open = true;
                            self.decision = TurnDecision::Prompt;
                        } else {
                            // Auto-apply: the remembered selection, or this
                            // turn's default-checked set if none remembered yet.
                            let sel = self
                                .cadence
                                .remembered(&conv)
                                .cloned()
                                .unwrap_or_else(|| model.checked_concepts());
                            self.resolved = Some(sel.clone());
                            self.decision = TurnDecision::AutoInject(sel);
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    fn decide_plain(&mut self) {
        self.model = None;
        self.open = false;
        self.decision = TurnDecision::PlainTurn;
        self.resolved = Some(Vec::new());
    }

    // ── user actions (also public so tests drive without clicks) ─────────

    /// Toggle a concept row's checked state.
    pub fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(m) = self.model.as_mut() {
            m.toggle(key);
            cx.notify();
        }
    }

    /// **Inject selected** — confirm the checked set, record it for the
    /// conversation, close the menu, and resolve the curated list.
    pub fn confirm_inject(&mut self, cx: &mut Context<Self>) {
        let sel = self
            .model
            .as_ref()
            .map(CurateMenuModel::checked_concepts)
            .unwrap_or_default();
        self.cadence.confirm(&self.conversation_id, sel.clone());
        self.resolved = Some(sel);
        self.close();
        cx.notify();
    }

    /// **Skip — plain RAG** — inject nothing this turn (curated-empty), but still
    /// record the choice so later turns auto-apply "nothing" (re-openable).
    pub fn skip(&mut self, cx: &mut Context<Self>) {
        self.cadence.confirm(&self.conversation_id, Vec::new());
        self.resolved = Some(Vec::new());
        self.close();
        cx.notify();
    }

    /// **⟳ auto next time** — confirm the current selection AND opt this
    /// conversation into auto-apply (no menu on later turns; still re-openable).
    pub fn auto_next(&mut self, cx: &mut Context<Self>) {
        let sel = self
            .model
            .as_ref()
            .map(CurateMenuModel::checked_concepts)
            .unwrap_or_default();
        self.cadence.confirm(&self.conversation_id, sel.clone());
        self.cadence.set_auto(&self.conversation_id);
        self.resolved = Some(sel);
        self.close();
        cx.notify();
    }

    /// The re-open control — force the menu on the next turn even though this
    /// conversation had auto-applied.
    pub fn reopen(&mut self, cx: &mut Context<Self>) {
        self.cadence.reopen(&self.conversation_id);
        cx.notify();
    }

    fn close(&mut self) {
        self.open = false;
        self.decision = TurnDecision::Prompt; // resolved now drives the composer
    }

    // ── render helpers ───────────────────────────────────────────────────

    fn button<F>(
        id: (&'static str, usize),
        label: &str,
        accent: bool,
        cx: &mut Context<Self>,
        on_click: F,
    ) -> gpui::Stateful<gpui::Div>
    where
        F: Fn(&mut Self, &mut Context<Self>) + 'static,
    {
        let bg = if accent { BRAND } else { SURFACE_800 };
        control(div(), id)
            .px_2()
            .py_0p5()
            .rounded(px(4.0))
            .bg(rgb(pack(bg)))
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .cursor_pointer()
            .child(SharedString::from(label.to_owned()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    on_click(this, cx)
                }),
            )
    }

    /// One concept/vocab row: checkbox + glyph + label + score + via.
    fn row(&self, idx: usize, cx: &mut Context<Self>) -> gpui::Div {
        let Some(model) = self.model.as_ref() else {
            return div();
        };
        let r = &model.rows[idx];
        let injectable = matches!(r.kind, RowKind::Concept);
        let greyed = r.annotation.is_greyed();
        let text_color = if greyed {
            rgb(pack(TEXT_MUTED))
        } else {
            rgb(pack(TEXT_PRIMARY))
        };
        let glyph_color = match r.annotation {
            CurateAnnotation::Excluded => rgb(pack(DANGER)),
            CurateAnnotation::Dependency | CurateAnnotation::SeedLift => rgb(pack(BRAND_LIGHT)),
            _ => rgb(pack(TEXT_MUTED)),
        };
        let key = r.key.clone();
        let checkbox_glyph = if r.checked { "[x]" } else { "[ ]" };

        let mut row = div().flex().flex_row().items_center().gap_2().py_0p5();

        if injectable {
            let k = key.clone();
            row = row.child(
                control(div(), ("curate-check", idx))
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(if r.checked { BRAND_LIGHT } else { TEXT_MUTED })))
                    .cursor_pointer()
                    .child(SharedString::from(checkbox_glyph))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            this.toggle(&k, cx);
                        }),
                    ),
            );
        } else {
            row = row.child(div().w(px(18.0)));
        }

        row = row
            .child(
                div()
                    .text_size(px(size::XS))
                    .text_color(glyph_color)
                    .child(SharedString::from(r.annotation.glyph())),
            )
            .child(
                div()
                    .text_size(px(size::XS))
                    .text_color(text_color)
                    .child(SharedString::from(r.label.clone())),
            );

        if injectable {
            row = row.child(
                div()
                    .text_size(px(size::MICRO))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(format!("{:.2}", r.score))),
            );
        }
        if let Some(via) = &r.via {
            row = row.child(
                div()
                    .text_size(px(size::MICRO))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(format!("via {via}"))),
            );
        }
        row
    }
}

impl Render for CurateMenuView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Closed menu renders nothing (the composer proceeds on the decision).
        if !self.open {
            return div();
        }
        let Some(model) = self.model.as_ref() else {
            return div();
        };

        let over = model.over_budget();
        let est = model.estimated_tokens();
        let budget = model.token_budget;

        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_700)))
            .border_1()
            .border_color(rgb(pack(BORDER_SUBTLE)))
            .child(
                div()
                    .text_size(px(size::SM))
                    .font_weight(gpui::FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from("Context to inject")),
            );

        for idx in 0..model.rows.len() {
            panel = panel.child(self.row(idx, cx));
        }

        // Budget indicator (warns when the checked set is over the cap).
        let budget_color = if over { WARNING } else { TEXT_MUTED };
        let budget_text = if over {
            format!("⚠ {est} / {budget} tokens — over budget; lowest-activation concepts will be dropped")
        } else {
            format!("{est} / {budget} tokens")
        };
        panel = panel.child(
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(budget_color)))
                .child(SharedString::from(budget_text)),
        );

        // Actions.
        let actions = div()
            .flex()
            .flex_row()
            .gap_2()
            .pt_1()
            .child(Self::button(
                ("curate-inject", 0),
                "Inject selected",
                true,
                cx,
                |this, cx| this.confirm_inject(cx),
            ))
            .child(Self::button(
                ("curate-skip", 0),
                "Skip — plain RAG",
                false,
                cx,
                |this, cx| this.skip(cx),
            ))
            .child(Self::button(
                ("curate-auto", 0),
                "⟳ auto next time",
                false,
                cx,
                |this, cx| this.auto_next(cx),
            ));
        panel = panel.child(actions);

        if let Some(status) = &self.status {
            panel = panel.child(
                div()
                    .text_size(px(size::MICRO))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(status.clone())),
            );
        }
        panel
    }
}
