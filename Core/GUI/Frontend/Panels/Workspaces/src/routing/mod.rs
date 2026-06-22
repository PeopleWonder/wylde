//! The **Relations** editor (concept-routing **R1.5c**, relation-model addendum
//! `outputs/concept-routing-relation-model.md` §2.2) — the data-authoring
//! surface for the typed relation graph the spreading-activation engine
//! (R1.5a/b) consumes.
//!
//! This is the **isolated experimental routing folder** the addendum specifies
//! (`Panels/Workspaces/src/routing/`): everything routing-GUI lives here, so the
//! feature deletes with the folder (the removal test). It surfaces as a third
//! sub-tab of the Vocabulary tab, beside Concepts.
//!
//! ## What it does
//!
//! Pick a **focus node** (a concept or a `{{vocab}}` anchor); the editor shows
//! every relation touching it, bucketed into the four authoring groups
//! ([`reducer::RelGroup`]): **DEPENDS ON** (`→`), **DEPENDED ON BY** (`←`,
//! read-only backward blast-radius), **RELATES TO** (`↔` positive), and **IS
//! NOT** (`⊘` negative exclusion). Each authorable group has a `[+ add]` that
//! opens a fuzzy node-picker; clicking a candidate writes the edge via
//! [`workspaces.concepts.relations.add`](ipc::add_relation). Each edge has a `✕`
//! that removes it. With no focus selected, an **overview** lists the whole
//! graph grouped by from-node — the same shape R3's typed-edge tree will render
//! ([`reducer::overview`]).
//!
//! ## Behaviour-safe
//!
//! Authoring relations only *shapes* routing when the master toggle is ON
//! (default OFF), and nothing is injected until R2. The three edge kinds are
//! distinguished visually — colour + glyph + a coloured left rule — so the `⊘`
//! exclusion reads as a cut, not a link (the addendum's emphasis).
//!
//! ## R2 / R3 seams
//!
//! * **R2 (injection):** the authored exclusions/dependencies feed the
//!   curate-before-inject menu and the boundary blurb — this editor is their
//!   durable store.
//! * **R3 (tree viz):** [`reducer::overview`] hands the typed-edge tree the same
//!   per-node grouped shape; the tree mounts beside this editor in `routing/`.

pub mod curate_ipc;
pub mod curate_menu;
pub mod curate_reducer;
pub mod ipc;
pub mod reducer;

pub use curate_menu::{CurateMenuView, TurnDecision};

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, IntoElement, MouseButton, MouseDownEvent, Render,
    Rgba, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    ACCENT_CYAN, BORDER_SUBTLE, BRAND, BRAND_DIM, BRAND_LIGHT, DANGER, SURFACE_700, SURFACE_800,
    TEXT_MUTED, TEXT_PRIMARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

use ipc::{NodeItem, NodeRefView, RelationView};
use reducer::{GroupEdge, RelGroup};

/// The accent colour for an authoring group — the second half of the edge-kind
/// distinction (glyph is the first). Exclusion is `DANGER` red so a `⊘` row
/// reads as a severed link; dependency is the bright brand cyan (pulled-in),
/// its backward view dimmed; positive is the soft accent cyan.
fn group_color(group: RelGroup) -> Rgba {
    match group {
        RelGroup::DependsOn => BRAND_LIGHT,
        RelGroup::DependedOnBy => BRAND_DIM,
        RelGroup::RelatesTo => ACCENT_CYAN,
        RelGroup::IsNot => DANGER,
    }
}

/// The Relations editor sub-tab view.
pub struct RelationsView {
    workspace_id: Option<String>,
    /// Every relatable node (concepts + workspace vocab), for labels + pickers.
    universe: Vec<NodeItem>,
    /// The node currently being edited; `None` shows the whole-graph overview.
    focus: Option<NodeRefView>,
    /// Edges touching the focus node (`relations.list`).
    touching: Vec<RelationView>,
    /// The whole graph (`relations.graph`) — drives the no-focus overview.
    overview_rels: Vec<RelationView>,
    loading: bool,
    error: Option<String>,
    /// Inline status from the last action (`Ok` info / `Err` failure).
    status: Option<Result<String, String>>,
    /// Search box for picking the focus node (no-focus state).
    node_search: Entity<TextInput>,
    _node_search_sub: gpui::Subscription,
    /// Which group's `[+ add]` picker is open, if any.
    picker_open: Option<RelGroup>,
    /// Search box for the open add-picker.
    picker_search: Entity<TextInput>,
    _picker_search_sub: gpui::Subscription,
    /// Optional note applied to the next authored edge.
    note_input: Entity<TextInput>,
}

impl RelationsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let node_search = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("relations-node-search")
                .with_placeholder("Pick a node to edit its relations…")
        });
        let node_search_sub = cx.subscribe(
            &node_search,
            |_this: &mut Self, _e, event: &wylde_gpui_input::InputEvent, cx| {
                if matches!(event, wylde_gpui_input::InputEvent::Changed(_)) {
                    cx.notify();
                }
            },
        );
        let picker_search = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("relations-picker-search")
                .with_placeholder("search concepts + vocab…")
        });
        let picker_search_sub = cx.subscribe(
            &picker_search,
            |_this: &mut Self, _e, event: &wylde_gpui_input::InputEvent, cx| {
                if matches!(event, wylde_gpui_input::InputEvent::Changed(_)) {
                    cx.notify();
                }
            },
        );
        let note_input = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("relations-note")
                .with_placeholder("optional note (e.g. \"keeps the home IP current\")")
        });
        let view = Self {
            workspace_id: None,
            universe: Vec::new(),
            focus: None,
            touching: Vec::new(),
            overview_rels: Vec::new(),
            loading: true,
            error: None,
            status: None,
            node_search,
            _node_search_sub: node_search_sub,
            picker_open: None,
            picker_search,
            _picker_search_sub: picker_search_sub,
            note_input,
        };
        Self::spawn_load(cx);
        view
    }

    /// Resolve the active workspace, then load the node universe + the whole
    /// relation graph (the overview). The focus list loads lazily on select.
    fn spawn_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws = crate::vocabulary::ipc::active_workspace().await;
            let (ws_id, universe, overview_rels, error) = match ws {
                Ok(Some(id)) => {
                    let universe = ipc::load_node_universe(&id).await;
                    match ipc::load_graph(&id).await {
                        Ok(rels) => (Some(id), universe, rels, None),
                        Err(e) => (Some(id), universe, Vec::new(), Some(e)),
                    }
                }
                Ok(None) => (None, Vec::new(), Vec::new(), None),
                Err(e) => (None, Vec::new(), Vec::new(), Some(e)),
            };
            let _ = this.update(app_cx, |v, cx| {
                v.loading = false;
                v.workspace_id = ws_id;
                v.universe = universe;
                v.overview_rels = overview_rels;
                v.error = error;
                cx.notify();
            });
        })
        .detach();
    }

    /// Reload the overview graph (after an edit when no focus is set, or on
    /// Refresh). Reuses the known workspace id.
    fn reload_overview(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let rels = ipc::load_graph(&ws).await;
            let _ = this.update(app_cx, |v, cx| {
                match rels {
                    Ok(r) => v.overview_rels = r,
                    Err(e) => v.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the edges touching the focus node + refresh the overview (so both
    /// stay current after an edit).
    fn reload_focus(&mut self, cx: &mut Context<Self>) {
        let (Some(ws), Some(focus)) = (self.workspace_id.clone(), self.focus.clone()) else {
            return;
        };
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let touching = ipc::list_for_node(&ws, &focus).await;
            let overview = ipc::load_graph(&ws).await;
            let _ = this.update(app_cx, |v, cx| {
                match touching {
                    Ok(t) => v.touching = t,
                    Err(e) => v.error = Some(e),
                }
                if let Ok(o) = overview {
                    v.overview_rels = o;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Focus a node and load its relations (the intra-tab deep-link: clicking
    /// any node anywhere re-centres the editor on it). Public so a windowed test
    /// can drive it without simulating a click.
    pub fn set_focus(&mut self, node: NodeRefView, cx: &mut Context<Self>) {
        self.focus = Some(node);
        self.picker_open = None;
        self.touching = Vec::new();
        self.reload_focus(cx);
        cx.notify();
    }

    /// Return to the whole-graph overview.
    fn clear_focus(&mut self, cx: &mut Context<Self>) {
        self.focus = None;
        self.picker_open = None;
        self.reload_overview(cx);
        cx.notify();
    }

    /// Author one edge from the focus node into `group` toward `target`. Public
    /// so a windowed test can drive the add path directly.
    pub fn add_edge(&mut self, group: RelGroup, target: NodeRefView, cx: &mut Context<Self>) {
        let (Some(ws), Some(focus)) = (self.workspace_id.clone(), self.focus.clone()) else {
            return;
        };
        let kind = group.kind();
        let note = {
            let t = self.note_input.read(cx).text().trim().to_owned();
            (!t.is_empty()).then_some(t)
        };
        let note_input = self.note_input.clone();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome =
                ipc::add_relation(&ws, &focus, &target, kind, note.as_deref()).await;
            let _ = this.update(app_cx, |v, cx| {
                match outcome {
                    Ok(_) => {
                        v.status = Some(Ok(format!(
                            "Added {} {} {}",
                            reducer::label_for(&focus, &v.universe),
                            group.glyph(),
                            reducer::label_for(&target, &v.universe),
                        )));
                        v.picker_open = None;
                        note_input.update(cx, |i, c| i.clear(c));
                    }
                    Err(e) => v.status = Some(Err(reducer::explain_error(&e))),
                }
                v.reload_focus(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Remove one edge by its stored `(from,to,kind)`.
    fn remove_edge(&mut self, edge: GroupEdge, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::remove_relation(&ws, &edge.from, &edge.to, edge.kind).await;
            let _ = this.update(app_cx, |v, cx| {
                v.status = Some(match outcome {
                    Ok(_) => Ok("Removed relation".to_owned()),
                    Err(e) => Err(reducer::explain_error(&e)),
                });
                v.reload_focus(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ── test/observability accessors ─────────────────────────────────────

    /// Whether the initial load is still in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    /// The resolved active workspace id, if any.
    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }
    /// The focused node, if any.
    pub fn focus(&self) -> Option<&NodeRefView> {
        self.focus.as_ref()
    }
    /// Number of edges touching the focus node.
    pub fn touching_len(&self) -> usize {
        self.touching.len()
    }
    /// Number of edges in the whole-graph overview.
    pub fn overview_len(&self) -> usize {
        self.overview_rels.len()
    }
    /// Number of relatable nodes loaded.
    pub fn universe_len(&self) -> usize {
        self.universe.len()
    }

    // ── element helpers (Concepts/Vocabulary sub-tab idioms) ─────────────

    fn heading(text: &str) -> gpui::Div {
        div()
            .text_size(px(size::SM))
            .font_weight(gpui::FontWeight(weight::SEMIBOLD as f32))
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
        div()
            .id(id)
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

    /// One authoring group section: header + `[+ add]` (when authorable) + the
    /// edges, each with its kind colour, glyph, deep-link, and `✕`.
    fn group_section(
        &self,
        group: RelGroup,
        edges: &[GroupEdge],
        gi: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let color = group_color(group);
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(size::XS))
                    .font_weight(gpui::FontWeight(weight::SEMIBOLD as f32))
                    .text_color(color)
                    .child(SharedString::from(format!(
                        "{} {}",
                        group.glyph(),
                        group.header()
                    ))),
            )
            .child(Self::hint(group.hint().to_owned()));
        if group.is_authorable() {
            let is_open = self.picker_open == Some(group);
            header = header.child(div().flex_1()).child(Self::button(
                ("relations-group-add", gi),
                if is_open { "cancel" } else { "+ add" },
                is_open,
                cx,
                move |this, cx| {
                    this.picker_open = if is_open { None } else { Some(group) };
                    this.picker_search.update(cx, |i, c| i.clear(c));
                    cx.notify();
                },
            ));
        }

        let mut section = div().flex().flex_col().gap_1().child(header);

        if edges.is_empty() {
            section = section.child(Self::hint("—".to_owned()));
        }
        for (ei, edge) in edges.iter().enumerate() {
            let other_label = reducer::label_for(&edge.other, &self.universe);
            let mut row = format!("{} {}", group.glyph(), other_label);
            if let Some(note) = &edge.note {
                row.push_str(&format!("   \u{201c}{note}\u{201d}"));
            }
            let other_for_link = edge.other.clone();
            let edge_for_rm = edge.clone();
            let read_only = !group.is_authorable();
            let mut row_el = div()
                .id(("relations-edge", gi * 1000 + ei))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_0p5()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_800)))
                .border_l_2()
                .border_color(color)
                .child(
                    div()
                        .flex_1()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .cursor_pointer()
                        .child(SharedString::from(row))
                        // Deep-link: click the other endpoint to edit ITS relations.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                                this.set_focus(other_for_link.clone(), cx);
                            }),
                        ),
                );
            if !read_only {
                row_el = row_el.child(Self::button(
                    ("relations-edge-remove", gi * 1000 + ei),
                    "✕",
                    false,
                    cx,
                    move |this, cx| this.remove_edge(edge_for_rm.clone(), cx),
                ));
            }
            section = section.child(row_el);
        }

        // The inline add-picker for this group.
        if self.picker_open == Some(group) {
            section = section.child(self.add_picker(group, edges, cx));
        }
        section
    }

    /// The `[+ add]` picker body: a note field + a search + the filtered
    /// candidate buttons (drops self + already-related-in-this-kind).
    fn add_picker(
        &self,
        group: RelGroup,
        edges: &[GroupEdge],
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(focus) = self.focus.clone() else {
            return div();
        };
        let already: Vec<NodeRefView> = edges.iter().map(|e| e.other.clone()).collect();
        let query = self.picker_search.read(cx).text().to_owned();
        let cands = reducer::picker_candidates(&self.universe, &focus, &already, &query);

        let picker = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(6.0))
            .bg(rgb(pack(SURFACE_700)))
            .child(Self::hint(format!(
                "{} — pick a target node",
                group.header()
            )))
            .child(div().child(self.note_input.clone()))
            .child(div().child(self.picker_search.clone()));

        let mut grid = div().flex().flex_row().flex_wrap().gap_1();
        if cands.is_empty() {
            grid = grid.child(Self::hint("no matching nodes".to_owned()));
        }
        for (ci, item) in cands.iter().take(60).enumerate() {
            let target = item.node.clone();
            grid = grid.child(Self::button(
                ("relations-cand", ci),
                &item.label,
                false,
                cx,
                move |this, cx| this.add_edge(group, target.clone(), cx),
            ));
        }
        picker.child(grid)
    }
}

impl Render for RelationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "no active workspace".to_owned());

        let mut root = div()
            .id("workspaces-relations-subtab")
            .flex()
            .flex_col()
            .gap_3()
            .font_family(FAMILY_INTER);

        // ── header ────────────────────────────────────────────────────────
        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(Self::heading("Relations"))
                .child(Self::hint(format!(
                    "{ws_label} · {} edge(s) · experimental",
                    self.overview_rels.len()
                )))
                .child(div().flex_1())
                .child(Self::button(
                    ("relations-refresh", 0),
                    "Refresh",
                    false,
                    cx,
                    |this, cx| {
                        if this.focus.is_some() {
                            this.reload_focus(cx);
                        } else {
                            this.reload_overview(cx);
                        }
                    },
                )),
        );
        root = root.child(Self::hint(
            "Author typed edges between concepts and {{vocab}} terms. Behaviour-safe: \
             relations only shape retrieval when routing is ON (off by default), and \
             nothing is injected yet (R2)."
                .to_owned(),
        ));

        if self.loading {
            return root.child(Self::hint("Loading relations…".to_owned()));
        }
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(DANGER)))
                    .child(SharedString::from(format!(
                        "Relation store unreachable — {err}"
                    ))),
            );
        }

        match self.focus.clone() {
            // ── focused node: the four authoring groups ────────────────────
            Some(focus) => {
                let focus_label = reducer::label_for(&focus, &self.universe);
                root = root.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(Self::button(
                            ("relations-back", 0),
                            "← all relations",
                            false,
                            cx,
                            |this, cx| this.clear_focus(cx),
                        ))
                        .child(Self::heading(&format!("Relations: {focus_label}"))),
                );
                let groups = reducer::group_edges(&focus, &self.touching);
                let mut col = div().flex().flex_col().gap_3();
                for (gi, (group, edges)) in groups.iter().enumerate() {
                    col = col.child(self.group_section(*group, edges, gi, cx));
                }
                root = root.child(col);
            }
            // ── no focus: the whole-graph overview + node picker ────────────
            None => {
                root = root.child(Self::heading("Edit a node's relations"));
                root = root.child(div().child(self.node_search.clone()));
                // The focus-node picker: filtered node universe.
                let q = self.node_search.read(cx).text().to_lowercase();
                let mut grid = div().flex().flex_row().flex_wrap().gap_1();
                let mut shown = 0usize;
                for (ci, item) in self.universe.iter().enumerate() {
                    if !q.is_empty() && !item.label.to_lowercase().contains(q.trim()) {
                        continue;
                    }
                    if shown >= 80 {
                        break;
                    }
                    shown += 1;
                    let node = item.node.clone();
                    grid = grid.child(Self::button(
                        ("relations-pick-focus", ci),
                        &item.label,
                        false,
                        cx,
                        move |this, cx| this.set_focus(node.clone(), cx),
                    ));
                }
                if self.universe.is_empty() {
                    grid = grid.child(Self::hint(
                        "No concepts or vocabulary yet — build concepts or add anchors first."
                            .to_owned(),
                    ));
                }
                root = root.child(grid);

                // Overview: the whole graph grouped by from-node (R3's seam).
                root = root.child(Self::heading("All relations"));
                let rows = reducer::overview(&self.overview_rels);
                if rows.is_empty() {
                    root = root.child(Self::hint(
                        "No relations yet. Pick a node above and author its dependencies, \
                         exclusions, and related concepts."
                            .to_owned(),
                    ));
                }
                let mut list = div().flex().flex_col().gap_1();
                for (ri, row) in rows.iter().enumerate() {
                    let node_label = reducer::label_for(&row.node, &self.universe);
                    let node_for_link = row.node.clone();
                    let mut card = div()
                        .id(("relations-overview", ri))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(rgb(pack(SURFACE_800)))
                        .cursor_pointer()
                        .child(
                            div()
                                .text_size(px(size::XS))
                                .font_weight(gpui::FontWeight(weight::SEMIBOLD as f32))
                                .text_color(rgb(pack(TEXT_PRIMARY)))
                                .child(SharedString::from(node_label)),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                                this.set_focus(node_for_link.clone(), cx);
                            }),
                        );
                    for edge in &row.edges {
                        let group = match edge.kind {
                            ipc::RelationKindView::Dependency => RelGroup::DependsOn,
                            ipc::RelationKindView::Positive => RelGroup::RelatesTo,
                            ipc::RelationKindView::Negative => RelGroup::IsNot,
                        };
                        let to_label = reducer::label_for(&edge.to, &self.universe);
                        card = card.child(
                            div()
                                .text_size(px(size::MICRO))
                                .text_color(group_color(group))
                                .child(SharedString::from(format!(
                                    "{} {to_label}",
                                    group.glyph()
                                ))),
                        );
                    }
                    list = list.child(card);
                }
                root = root.child(list);
            }
        }

        // ── status strip ────────────────────────────────────────────────
        if let Some(status) = &self.status {
            root = root.child(match status {
                Ok(msg) => Self::hint(msg.clone()),
                Err(msg) => div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(DANGER)))
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
        assert_render::<RelationsView>();
    }

    #[test]
    fn group_colors_are_distinct_for_dependency_vs_exclusion() {
        // The addendum's emphasis: `⊘` exclusion must not read like a `→`
        // dependency link.
        assert_ne!(
            group_color(RelGroup::IsNot),
            group_color(RelGroup::DependsOn)
        );
        assert_eq!(group_color(RelGroup::IsNot), DANGER);
    }
}
