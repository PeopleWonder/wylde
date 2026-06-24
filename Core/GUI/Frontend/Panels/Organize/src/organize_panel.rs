//! Organize panel View — the native gpui cockpit over `wylde-organize`.
//!
//! Flow, top → bottom:
//!   * Header — title + Undo-last.
//!   * Scope picker — tier segmented buttons (User data / Whole profile / Whole
//!     drive), an opt-in toggle for the broader tiers, an optional roots field,
//!     and (drive only) the typed-confirmation field. A Scan button fires
//!     `organize.propose`.
//!   * Plan review — stats strip, then the proposed ops and removal candidates
//!     as rows with a per-row Keep/Skip toggle (read-only until Apply).
//!   * Apply / status — Apply the curated set (`organize.apply`); the result +
//!     any error render inline.
//!
//! The panel greys out (ServiceUnavailable stub) when `wylde-organize` is down —
//! declared via `required_services` in the manifest, handled by the Shell.

use std::collections::HashSet;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, Entity,
    FontWeight, IntoElement, Render, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_700, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{self, Proposal};

/// The three scope tiers, mirrored for the UI (the wire strings live in the
/// service's `scope.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierUi {
    UserData,
    UserProfile,
    Drive,
}

impl TierUi {
    fn wire(self) -> &'static str {
        match self {
            TierUi::UserData => "user_data",
            TierUi::UserProfile => "user_profile",
            TierUi::Drive => "drive",
        }
    }
    fn label(self) -> &'static str {
        match self {
            TierUi::UserData => "User data",
            TierUi::UserProfile => "Whole profile",
            TierUi::Drive => "Whole drive",
        }
    }
    /// Whether this tier needs the opt-in toggle on.
    fn needs_opt_in(self) -> bool {
        !matches!(self, TierUi::UserData)
    }
    /// Whether this tier needs the typed confirmation field.
    fn needs_confirmation(self) -> bool {
        matches!(self, TierUi::Drive)
    }
}

pub struct OrganizePanel {
    pub tier: TierUi,
    /// Opt-in for the broader tiers (the safety gate the service enforces).
    pub opt_in: bool,
    /// Optional explicit roots (comma-separated). Required for the drive tier.
    pub roots_input: Entity<TextInput>,
    /// Typed confirmation phrase (drive tier only).
    pub confirm_input: Entity<TextInput>,

    /// The last proposal returned by `organize.propose` (read-only).
    pub proposal: Option<Proposal>,
    /// Op ids the user has rejected (won't be sent to apply).
    pub rejected_ops: HashSet<u32>,
    /// Removal paths the user has rejected.
    pub rejected_removals: HashSet<String>,

    pub loading: bool,
    pub error: Option<String>,
    /// Transient status line (last scan / apply / undo summary).
    pub status: Option<String>,
    /// Undo token from the last successful apply (drives the Undo button).
    pub last_undo_token: Option<String>,
}

impl OrganizePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let roots_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Optional: comma-separated folders (required for Whole drive)")
        });
        let confirm_input = cx.new(|input_cx| {
            TextInput::single_line(input_cx)
                .with_placeholder("Type: organize my whole drive")
        });
        Self {
            tier: TierUi::UserData,
            opt_in: false,
            roots_input,
            confirm_input,
            proposal: None,
            rejected_ops: HashSet::new(),
            rejected_removals: HashSet::new(),
            loading: false,
            error: None,
            status: None,
            last_undo_token: None,
        }
    }

    /// Factory entry — matches the manifest factory string
    /// (`wylde_panel_organize::OrganizePanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| Self::new(cx)).into()
    }

    pub fn set_tier(&mut self, tier: TierUi, cx: &mut Context<Self>) {
        self.tier = tier;
        // Re-narrowing to user-data drops the opt-in so you can't leave a stale
        // broad-scope grant armed.
        if tier == TierUi::UserData {
            self.opt_in = false;
        }
        cx.notify();
    }

    pub fn toggle_opt_in(&mut self, cx: &mut Context<Self>) {
        self.opt_in = !self.opt_in;
        cx.notify();
    }

    /// Build the scope payload from the current picker state.
    fn build_payload(&self, cx: &Context<Self>) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "tier": self.tier.wire(),
            "opt_in": self.opt_in,
        });
        let roots_text = self.roots_input.read(cx).text().trim().to_owned();
        if !roots_text.is_empty() {
            let roots: Vec<String> = roots_text
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            payload["roots"] = serde_json::json!(roots);
        }
        let conf = self.confirm_input.read(cx).text().trim().to_owned();
        if !conf.is_empty() {
            payload["typed_confirmation"] = serde_json::json!(conf);
        }
        payload
    }

    /// Fire `organize.propose` and render the returned read-only plan.
    pub fn scan(&mut self, cx: &mut Context<Self>) {
        let payload = self.build_payload(cx);
        self.loading = true;
        self.error = None;
        self.status = None;
        self.proposal = None;
        self.rejected_ops.clear();
        self.rejected_removals.clear();
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::propose(payload).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.loading = false;
                match result {
                    Ok(p) => {
                        let s = &p.view.stats;
                        panel.status = Some(format!(
                            "Scanned {} files · {} moves · {} removals · {} protected skipped",
                            s.files_scanned, s.ops_proposed, s.removals_proposed, s.skipped_protected
                        ));
                        panel.proposal = Some(p);
                    }
                    Err(e) => panel.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_op(&mut self, id: u32, cx: &mut Context<Self>) {
        if !self.rejected_ops.remove(&id) {
            self.rejected_ops.insert(id);
        }
        cx.notify();
    }

    pub fn toggle_removal(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.rejected_removals.remove(&path) {
            self.rejected_removals.insert(path);
        }
        cx.notify();
    }

    /// Apply the curated plan (accepted ops + accepted removals only).
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(proposal) = &self.proposal else { return };
        let curated = ipc::curate(&proposal.raw, &self.rejected_ops, &self.rejected_removals);
        self.loading = true;
        self.error = None;
        self.status = None;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::apply(curated).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.loading = false;
                match result {
                    Ok(out) => {
                        panel.status = Some(format!(
                            "Applied {} · skipped {} · failed {}. Undo available.",
                            out.applied, out.skipped, out.failed
                        ));
                        if !out.undo_token.is_empty() {
                            panel.last_undo_token = Some(out.undo_token);
                        }
                        // The plan has been consumed — clear the review surface.
                        panel.proposal = None;
                        panel.rejected_ops.clear();
                        panel.rejected_removals.clear();
                    }
                    Err(e) => panel.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Undo the most recent applied plan.
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        let token = self.last_undo_token.clone().unwrap_or_else(|| "latest".to_owned());
        self.loading = true;
        self.error = None;
        self.status = None;
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let result = ipc::undo(&token).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.loading = false;
                match result {
                    Ok(u) => {
                        panel.status = Some(format!(
                            "Undo {}: restored {} · skipped {} · failed {}",
                            u.plan_id, u.restored, u.skipped, u.failed
                        ));
                        panel.last_undo_token = None;
                    }
                    Err(e) => panel.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ── rendering ─────────────────────────────────────────────────────────

impl Render for OrganizePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .font_family(FAMILY_INTER)
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(self.header(cx))
            .child(self.scope_picker(cx));

        if let Some(status) = &self.status {
            root = root.child(strip(status, TEXT_SECONDARY));
        }
        if let Some(err) = &self.error {
            root = root.child(strip(&format!("Error: {err}"), BRAND));
        }
        if self.loading {
            root = root.child(strip("Working…", TEXT_MUTED));
        }
        if self.proposal.is_some() {
            root = root.child(self.plan_review(cx));
        }
        root
    }
}

impl OrganizePanel {
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_size(px(size::LG))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .child(SharedString::from("Organize")),
            )
            .child(button(
                "organize-undo",
                "Undo last",
                BORDER_SUBTLE,
                cx.listener(|this: &mut OrganizePanel, _ev, _w, cx| this.undo(cx)),
            ))
    }

    fn scope_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tiers = [TierUi::UserData, TierUi::UserProfile, TierUi::Drive];
        let mut row = div().flex().flex_row().gap_2();
        for t in tiers {
            let selected = self.tier == t;
            row = row.child(tier_button(t, selected, cx));
        }

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_800)))
            .child(label("Scope"))
            .child(row);

        if self.tier.needs_opt_in() {
            let on = self.opt_in;
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(button(
                        "organize-optin",
                        if on { "✓ Opted in (broad scope)" } else { "Enable broader scope" },
                        if on { BRAND } else { BORDER_DEFAULT },
                        cx.listener(|this: &mut OrganizePanel, _ev, _w, cx| this.toggle_opt_in(cx)),
                    ))
                    .child(hint("Broader than your user-data folders — explicit opt-in required.")),
            );
        }

        col = col.child(self.roots_input.clone());

        if self.tier.needs_confirmation() {
            col = col
                .child(hint("Whole-drive scans need the typed phrase below to confirm."))
                .child(self.confirm_input.clone());
        }

        col.child(button(
            "organize-scan",
            "Scan",
            BRAND,
            cx.listener(|this: &mut OrganizePanel, _ev, _w, cx| this.scan(cx)),
        ))
    }

    fn plan_review(&self, cx: &mut Context<Self>) -> gpui::Div {
        // Caller (render) only invokes this when a proposal exists; a let-else
        // keeps the render path panic-free regardless (wylde_check rule 44).
        let Some(proposal) = self.proposal.as_ref() else {
            return div();
        };
        let view = &proposal.view;

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_800)))
            .child(label(&format!(
                "Proposed plan · {} moves · {} removals",
                view.ops.len(),
                view.removals.len()
            )));

        // Ops.
        for op in &view.ops {
            let rejected = self.rejected_ops.contains(&op.id);
            let id = op.id;
            let line = match (&op.from, op.kind.as_str()) {
                (Some(from), _) => format!("{} → {}  ({})", short(from), short(&op.to), op.rationale),
                (None, _) => format!("mkdir {}  ({})", short(&op.to), op.rationale),
            };
            col = col.child(review_row(
                ElementId::Name(format!("op-{id}").into()),
                &line,
                rejected,
                cx.listener(move |this: &mut OrganizePanel, _ev, _w, cx| this.toggle_op(id, cx)),
            ));
        }

        // Removals.
        for rem in &view.removals {
            let rejected = self.rejected_removals.contains(&rem.path);
            let path = rem.path.clone();
            let line = format!("[{}] {}  ({})", rem.reason, short(&rem.path), rem.detail);
            col = col.child(review_row(
                ElementId::Name(format!("rem-{}", rem.path).into()),
                &line,
                rejected,
                cx.listener(move |this: &mut OrganizePanel, _ev, _w, cx| {
                    this.toggle_removal(path.clone(), cx)
                }),
            ));
        }

        // Skipped (protected) — read-only, informational.
        if !view.skipped.is_empty() {
            col = col.child(hint(&format!(
                "{} protected path(s) skipped (never touched).",
                view.skipped.len()
            )));
        }

        col.child(button(
            "organize-apply",
            "Apply selected",
            BRAND,
            cx.listener(|this: &mut OrganizePanel, _ev, _w, cx| this.apply(cx)),
        ))
    }
}

// ── small element helpers ────────────────────────────────────────────

fn button(
    id: &'static str,
    text: &str,
    border: gpui::Rgba,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(id.into()))
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(border)))
        .cursor_pointer()
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .hover(|s| s.bg(rgb(pack(SURFACE_700))))
        .on_mouse_down(gpui::MouseButton::Left, on_click)
        .child(SharedString::from(text.to_owned()))
}

fn tier_button(t: TierUi, selected: bool, cx: &mut Context<OrganizePanel>) -> impl IntoElement {
    div()
        .id(ElementId::Name(format!("tier-{}", t.wire()).into()))
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(if selected { BRAND } else { BORDER_DEFAULT })))
        .bg(rgb(pack(if selected { BRAND_DIM } else { SURFACE_700 })))
        .cursor_pointer()
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut OrganizePanel, _ev, _w, cx| this.set_tier(t, cx)),
        )
        .child(SharedString::from(t.label()))
}

fn review_row(
    id: ElementId,
    line: &str,
    rejected: bool,
    on_toggle: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (toggle_label, toggle_color) = if rejected {
        ("Skip", TEXT_MUTED)
    } else {
        ("Keep", BRAND)
    };
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .justify_between()
        .py_1()
        .child(
            div()
                .text_size(px(size::SM))
                .text_color(rgb(pack(if rejected { TEXT_MUTED } else { TEXT_SECONDARY })))
                .child(SharedString::from(line.to_owned())),
        )
        .child(
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(toggle_color)))
                .cursor_pointer()
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .on_mouse_down(gpui::MouseButton::Left, on_toggle)
                .child(SharedString::from(toggle_label)),
        )
}

fn label(text: &str) -> impl IntoElement {
    div()
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(text.to_owned()))
}

fn hint(text: &str) -> impl IntoElement {
    div()
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(text.to_owned()))
}

fn strip(text: &str, color: gpui::Rgba) -> impl IntoElement {
    div()
        .p_2()
        .rounded(px(4.0))
        .bg(rgb(pack(SURFACE_800)))
        .text_size(px(size::SM))
        .text_color(rgb(pack(color)))
        .child(SharedString::from(text.to_owned()))
}

/// Shorten a long absolute path for display (keep the last two components).
fn short(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let parts: Vec<&str> = norm.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("…/{}/{}", parts[1], parts[0])
    } else {
        norm
    }
}

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<OrganizePanel>();
    }

    #[test]
    fn tier_wire_strings_match_service() {
        assert_eq!(TierUi::UserData.wire(), "user_data");
        assert_eq!(TierUi::UserProfile.wire(), "user_profile");
        assert_eq!(TierUi::Drive.wire(), "drive");
        assert!(TierUi::UserProfile.needs_opt_in());
        assert!(!TierUi::UserData.needs_opt_in());
        assert!(TierUi::Drive.needs_confirmation());
    }

    #[test]
    fn short_keeps_tail() {
        assert_eq!(short("C:/Users/x/Downloads/a.pdf"), "…/Downloads/a.pdf");
    }
}
