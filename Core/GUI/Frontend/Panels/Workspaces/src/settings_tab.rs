//! The Workspaces panel's Settings tab (Slice C-settings, Plan v2 §9/§10).
//!
//! Graph-specific sections of the global settings menu: the **profile
//! library** (apply / save-as / delete, quick-switcher lives in the Graph
//! tab's breadcrumb bar), **Layout**, **Clustering** and **Navigation** knob
//! editors, and the **Theme** mode toggle. Sections that belong to other
//! surfaces (Vocabulary, Thought Bubbles, Token Budget, Domains) arrive with
//! their own slices (N / F / M).
//!
//! Knob edits follow an explicit **Apply** flow: type values, press "Apply
//! knobs" — parse errors surface inline and nothing half-applies. Applied
//! values change the live view immediately; they persist when saved into a
//! profile ("Save current as…"), which also bookmarks it for the active
//! workspace.

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, Render, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{BORDER_SUBTLE, BRAND, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::graph::cluster::ClusterConfig;
use crate::graph::layout::LayoutKind;
use crate::graph::navigation::NavConfig;
use crate::graph::GraphView;
use crate::workspaces_panel::pack;
use wylde_gui_controls::control;

/// The Settings tab view. Holds the graph entity it edits plus the input
/// widgets; all setting state lives on the [`GraphView`] (single owner).
pub struct GraphSettingsTab {
    graph: Entity<GraphView>,
    name_input: Entity<TextInput>,
    zoom_step: Entity<TextInput>,
    leave_hysteresis: Entity<TextInput>,
    fit_margin: Entity<TextInput>,
    auto_threshold: Entity<TextInput>,
    target_visible: Entity<TextInput>,
    min_fold: Entity<TextInput>,
    /// Inline status: `Ok` = last action confirmation, `Err` = parse/save
    /// error. Cleared on the next action.
    status: Option<Result<String, String>>,
}

impl GraphSettingsTab {
    pub fn new(graph: Entity<GraphView>, cx: &mut Context<Self>) -> Self {
        let field = |cx: &mut Context<Self>, key: &'static str, placeholder: &'static str| {
            cx.new(|c| {
                TextInput::single_line(c)
                    .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                    .with_element_key(key)
                    .with_placeholder(placeholder)
            })
        };
        let mut tab = Self {
            graph,
            name_input: field(cx, "graph-profile-name", "profile name"),
            zoom_step: field(cx, "knob-zoom-step", "1.15"),
            leave_hysteresis: field(cx, "knob-leave-hysteresis", "0.8"),
            fit_margin: field(cx, "knob-fit-margin", "0.85"),
            auto_threshold: field(cx, "knob-auto-threshold", "300"),
            target_visible: field(cx, "knob-target-visible", "150"),
            min_fold: field(cx, "knob-min-fold", "3"),
            status: None,
        };
        tab.reload_fields(cx);
        tab
    }

    /// Pull the live knob values from the graph into the input fields.
    fn reload_fields(&mut self, cx: &mut Context<Self>) {
        let nav = self.graph.read(cx).nav_config();
        let cluster = self.graph.read(cx).cluster_config();
        let set = |input: &Entity<TextInput>, v: String, cx: &mut Context<Self>| {
            input.update(cx, |i, c| i.set_text_silent(v, c));
        };
        set(&self.zoom_step, format!("{}", nav.zoom_step_factor), cx);
        set(
            &self.leave_hysteresis,
            format!("{}", nav.leave_hysteresis),
            cx,
        );
        set(&self.fit_margin, format!("{}", nav.cluster_fit_margin), cx);
        set(
            &self.auto_threshold,
            format!("{}", cluster.auto_threshold_nodes),
            cx,
        );
        set(
            &self.target_visible,
            format!("{}", cluster.target_visible_nodes),
            cx,
        );
        set(&self.min_fold, format!("{}", cluster.min_fold_size), cx);
    }

    /// Parse every knob field; on success push both configs into the graph.
    fn apply_knobs(&mut self, cx: &mut Context<Self>) {
        let f32_of = |input: &Entity<TextInput>, label: &str, cx: &Context<Self>| {
            input
                .read(cx)
                .text()
                .trim()
                .parse::<f32>()
                .map_err(|_| format!("{label} must be a number"))
        };
        let usize_of = |input: &Entity<TextInput>, label: &str, cx: &Context<Self>| {
            input
                .read(cx)
                .text()
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("{label} must be a whole number"))
        };
        let parsed = (|| -> Result<(NavConfig, ClusterConfig), String> {
            let nav = NavConfig {
                zoom_step_factor: f32_of(&self.zoom_step, "zoom step", cx)?,
                leave_hysteresis: f32_of(&self.leave_hysteresis, "leave hysteresis", cx)?,
                cluster_fit_margin: f32_of(&self.fit_margin, "fit margin", cx)?,
                ..self.graph.read(cx).nav_config()
            };
            let cluster = ClusterConfig {
                auto_threshold_nodes: usize_of(&self.auto_threshold, "auto threshold", cx)?,
                target_visible_nodes: usize_of(&self.target_visible, "target visible", cx)?,
                min_fold_size: usize_of(&self.min_fold, "min fold size", cx)?,
                ..self.graph.read(cx).cluster_config()
            };
            Ok((nav, cluster))
        })();
        match parsed {
            Ok((nav, cluster)) => {
                self.graph.update(cx, |g, gcx| {
                    g.set_nav_config(nav, gcx);
                    g.set_cluster_config(cluster, gcx);
                });
                self.status = Some(Ok(
                    "Knobs applied — save them into a profile to persist".to_owned()
                ));
            }
            Err(e) => self.status = Some(Err(e)),
        }
        cx.notify();
    }

    fn save_profile(&mut self, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).text().trim().to_owned();
        let outcome = self.graph.update(cx, |g, _| g.save_current_profile(&name));
        self.status = Some(match outcome {
            Ok(()) => {
                self.name_input.update(cx, |i, c| i.clear(c));
                Ok(format!("Saved profile \"{name}\""))
            }
            Err(e) => Err(e),
        });
        cx.notify();
    }

    // ── element builders ─────────────────────────────────────────────────

    fn heading(text: &str) -> gpui::Div {
        div()
            .text_size(px(size::SM))
            .font_weight(FontWeight(weight::SEMIBOLD as f32))
            .text_color(rgb(pack(TEXT_PRIMARY)))
            .child(SharedString::from(text.to_owned()))
    }

    fn hint(text: String) -> gpui::Div {
        div()
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(text))
    }

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
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| on_click(this, cx)),
            )
    }

    fn labelled_input(label: &str, input: &Entity<TextInput>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .w(px(170.0))
            .child(Self::hint(label.to_owned()))
            .child(div().child(input.clone()))
    }
}

impl Render for GraphSettingsTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let graph = self.graph.read(cx);
        let profile_names = graph.profile_names();
        let active = graph.active_profile_name().to_owned();
        let dark = graph.dark_mode();
        let layout = graph.current_layout_kind();
        let lib_error = graph.profiles_error().map(str::to_owned);

        let mut root = div()
            .id("workspaces-settings-tab")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .font_family(FAMILY_INTER);

        // ── Profiles ────────────────────────────────────────────────────
        root = root.child(Self::heading("Profiles"));
        root = root.child(Self::hint(
            "A profile snapshots every graph setting. The active one is bookmarked per workspace."
                .to_owned(),
        ));
        let mut rows = div().flex().flex_col().gap_1();
        for (i, name) in profile_names.iter().enumerate() {
            let is_active = *name == active;
            let marker = if is_active { "● " } else { "" };
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_0p5()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_800)))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(format!("{marker}{name}"))),
                );
            let apply_name = name.clone();
            row = row.child(Self::button(
                ("settings-profile-apply", i),
                "Apply",
                is_active,
                cx,
                move |this, cx| {
                    let n = apply_name.clone();
                    this.graph.update(cx, |g, gcx| {
                        g.apply_profile(&n, gcx);
                    });
                    this.reload_fields(cx);
                    this.status = Some(Ok(format!("Applied \"{n}\"")));
                    cx.notify();
                },
            ));
            let delete_name = name.clone();
            row = row.child(Self::button(
                ("settings-profile-delete", i),
                "Delete",
                false,
                cx,
                move |this, cx| {
                    let n = delete_name.clone();
                    let removed = this.graph.update(cx, |g, gcx| g.delete_profile(&n, gcx));
                    this.status = Some(if removed {
                        Ok(format!("Deleted \"{n}\""))
                    } else {
                        Err(format!("\"{n}\" can't be deleted"))
                    });
                    cx.notify();
                },
            ));
            rows = rows.child(row);
        }
        root = root.child(rows);

        // Save-current-as.
        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap_2()
                .child(Self::labelled_input("Save current as", &self.name_input))
                .child(Self::button(
                    ("settings-profile-save", 0),
                    "Save",
                    true,
                    cx,
                    |this, cx| this.save_profile(cx),
                )),
        );

        // ── Layout ──────────────────────────────────────────────────────
        root = root.child(Self::heading("Layout"));
        let mut layout_row = div().flex().flex_row().gap_2();
        for (i, kind) in [
            LayoutKind::ForceDirected,
            LayoutKind::Hierarchical,
            LayoutKind::StableGrid,
        ]
        .into_iter()
        .enumerate()
        {
            layout_row = layout_row.child(Self::button(
                ("settings-layout", i),
                kind.label(),
                kind == layout,
                cx,
                move |this, cx| {
                    this.graph.update(cx, |g, gcx| g.choose_layout(kind, gcx));
                    cx.notify();
                },
            ));
        }
        root = root.child(layout_row);

        // ── Theme ───────────────────────────────────────────────────────
        root = root.child(Self::heading("Theme"));
        root = root.child(div().flex().flex_row().gap_2().child(Self::button(
            ("settings-dark-toggle", 0),
            if dark {
                "Dark mode: on"
            } else {
                "Dark mode: off"
            },
            dark,
            cx,
            move |this, cx| {
                this.graph.update(cx, |g, gcx| g.set_dark_mode(!dark, gcx));
                cx.notify();
            },
        )));

        // ── Navigation + Clustering knobs ───────────────────────────────
        root = root.child(Self::heading("Navigation"));
        root = root.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .child(Self::labelled_input("Zoom step / notch", &self.zoom_step))
                .child(Self::labelled_input(
                    "Leave hysteresis (0–1)",
                    &self.leave_hysteresis,
                ))
                .child(Self::labelled_input("Cluster fit margin", &self.fit_margin)),
        );
        root = root.child(Self::heading("Clustering"));
        root = root.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .child(Self::labelled_input(
                    "Auto-cluster above N nodes",
                    &self.auto_threshold,
                ))
                .child(Self::labelled_input(
                    "Fold to ~N visible",
                    &self.target_visible,
                ))
                .child(Self::labelled_input("Min cluster size", &self.min_fold)),
        );
        root = root.child(div().flex().flex_row().gap_2().child(Self::button(
            ("settings-apply-knobs", 0),
            "Apply knobs",
            true,
            cx,
            |this, cx| this.apply_knobs(cx),
        )));

        // ── Status strip ────────────────────────────────────────────────
        if let Some(e) = lib_error {
            root = root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(format!("Profile library: {e}"))),
            );
        }
        if let Some(status) = &self.status {
            root = root.child(match status {
                Ok(msg) => div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(msg.clone())),
                Err(msg) => div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(msg.clone())),
            });
        }

        root.border_t_1().border_color(rgb(pack(BORDER_SUBTLE)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<GraphSettingsTab>();
    }
}
