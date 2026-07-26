//! Breadcrumb bar (Slice C-navigation) — the scope trail across the top of
//! the graph canvas, styled entirely from the Theme
//! (`graph_panel.breadcrumb_bar`: 36 px bar, "›" separator, 12 px text).
//!
//! The **model** half ([`breadcrumbs`]) is pure and unit-tested: it builds
//! the crumb list for the current scope. The **render** half
//! (`GraphView::breadcrumb_bar`) turns it into a gpui strip; clicking the
//! root crumb zooms back out (the v1 leave action).
//!
//! v1 wire clusters (one per file parent directory) carry a path-derived
//! `parent_breadcrumb`, but those intermediate path segments don't map to
//! *enterable* clusters until C-cluster lands real hierarchy — so
//! intermediate crumbs render inert (context, not buttons), and only crumbs
//! with a real target are clickable. C-cluster upgrades them by giving every
//! level a cluster id.

use gpui::{div, prelude::*, px, Context, MouseButton, MouseDownEvent, SharedString, Window};

use super::super::GraphView;
use super::{display_name, NavAction, ScopeEntry};
use crate::graph::model::WorkspaceGraph;
use crate::graph::paint::to_rgba;
use crate::graph::render::Theme;
use wylde_gui_controls::control;

/// Where a crumb click takes you.
#[derive(Clone, Debug, PartialEq)]
pub enum CrumbTarget {
    /// Zoom out of the scope entirely (the workspace-level view).
    Root,
    /// Context-only (an intermediate path segment, or the current level).
    Inert,
}

/// One crumb in the trail.
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    pub label: String,
    pub target: CrumbTarget,
}

/// Build the crumb trail: the root (workspace) crumb, then — when scoped —
/// the entered cluster's path segments with the cluster itself last. The
/// final crumb is the current level (inert); ancestors of the entered
/// cluster are inert until C-cluster makes them enterable.
pub fn breadcrumbs(
    root_label: &str,
    scope: Option<&ScopeEntry>,
    graph: &WorkspaceGraph,
) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb {
        label: root_label.to_owned(),
        target: if scope.is_some() {
            CrumbTarget::Root
        } else {
            CrumbTarget::Inert
        },
    }];
    let Some(entry) = scope else {
        return crumbs;
    };
    let Some(cluster) = graph.cluster_by_id(&entry.cluster_id) else {
        // Scope references a cluster the (reloaded) graph no longer has —
        // still show where the user is by id.
        crumbs.push(Crumb {
            label: display_name(&entry.cluster_id),
            target: CrumbTarget::Inert,
        });
        return crumbs;
    };
    let leaf = display_name(&cluster.id);
    for seg in &cluster.parent_breadcrumb {
        // The wire's path trail usually ends with the cluster's own segment;
        // skip it so the leaf isn't shown twice.
        if seg == &leaf {
            continue;
        }
        crumbs.push(Crumb {
            label: seg.clone(),
            target: CrumbTarget::Inert,
        });
    }
    crumbs.push(Crumb {
        label: leaf,
        target: CrumbTarget::Inert,
    });
    crumbs
}

impl GraphView {
    /// The breadcrumb strip (rendered above the canvas whenever a graph is
    /// loaded). All values from Theme `graph_panel.breadcrumb_bar`.
    pub(in crate::graph) fn breadcrumb_bar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let bb = &theme.graph_panel.breadcrumb_bar;
        let root_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "workspace".to_owned());
        let crumbs = breadcrumbs(&root_label, self.navigator.scope(), &self.graph);

        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_3()
            .w_full()
            .h(px(bb.height_px))
            .bg(to_rgba(bb.background(self.dark)))
            .text_size(px(bb.font_size_px))
            .text_color(to_rgba(bb.text(self.dark)));

        // Inert ancestor crumbs dim (alpha on the Theme text colour) so the
        // current level pops; the root crumb stays full-strength when it's a
        // live "zoom out" control.
        let dim = to_rgba(bb.text(self.dark).with_alpha(0.6));
        let last = crumbs.len().saturating_sub(1);
        for (i, crumb) in crumbs.into_iter().enumerate() {
            if i > 0 {
                bar = bar.child(
                    div()
                        .text_color(dim)
                        .child(SharedString::from(bb.separator_glyph.clone())),
                );
            }
            let clickable = crumb.target == CrumbTarget::Root;
            let mut el =
                control(div(), ("graph-breadcrumb", i)).child(SharedString::from(crumb.label));
            if clickable {
                el = el.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, _w: &mut Window, cx| {
                        this.apply_nav_action(NavAction::LeaveScope, cx);
                    }),
                );
            } else if i != last {
                el = el.text_color(dim);
            }
            bar = bar.child(el);
        }

        // Profile quick-switcher (C-settings): right-aligned button showing
        // the active profile; click toggles the dropdown rendered over the
        // graph area (`GraphView::profile_menu_element`).
        bar = bar.child(div().flex_1());
        bar = bar.child(
            control(div(), "graph-profile-switcher")
                .cursor_pointer()
                .child(SharedString::from(format!(
                    "{} ▾",
                    self.active_profile_name()
                )))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &MouseDownEvent, _w: &mut Window, cx| {
                        cx.stop_propagation();
                        this.profile_menu_open = !this.profile_menu_open;
                        cx.notify();
                    }),
                ),
        );
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::Cluster;
    use crate::graph::render::viewport::Camera;

    fn graph_with(cluster: Cluster) -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![],
            edges: vec![],
            clusters: vec![cluster],
        }
    }

    fn entry(id: &str) -> ScopeEntry {
        ScopeEntry {
            cluster_id: id.to_owned(),
            saved_camera: Camera::default(),
            leave_below: 0.8,
        }
    }

    #[test]
    fn unscoped_shows_inert_root_only() {
        let g = WorkspaceGraph::default();
        let c = breadcrumbs("wylde-harness", None, &g);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].label, "wylde-harness");
        assert_eq!(c[0].target, CrumbTarget::Inert, "nothing to leave");
    }

    #[test]
    fn scoped_trail_is_root_then_path_then_cluster() {
        let g = graph_with(Cluster {
            id: "C:/ws/src/graph".to_owned(),
            member_ids: vec![],
            parent_breadcrumb: vec!["ws".to_owned(), "src".to_owned()],
            zoom_threshold: 1.0,
        });
        let e = entry("C:/ws/src/graph");
        let c = breadcrumbs("wylde-harness", Some(&e), &g);
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["wylde-harness", "ws", "src", "graph"]);
        assert_eq!(c[0].target, CrumbTarget::Root, "root climbs out");
        assert!(
            c[1..].iter().all(|c| c.target == CrumbTarget::Inert),
            "path + leaf inert until C-cluster"
        );
    }

    #[test]
    fn trail_skips_duplicate_leaf_segment() {
        // Wire breadcrumbs that already end in the cluster's own segment.
        let g = graph_with(Cluster {
            id: "C:/ws/src".to_owned(),
            member_ids: vec![],
            parent_breadcrumb: vec!["ws".to_owned(), "src".to_owned()],
            zoom_threshold: 1.0,
        });
        let e = entry("C:/ws/src");
        let c = breadcrumbs("root", Some(&e), &g);
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["root", "ws", "src"], "no doubled leaf");
    }

    #[test]
    fn missing_cluster_still_shows_scope_by_id() {
        let g = WorkspaceGraph::default();
        let e = entry("C:/gone/cluster");
        let c = breadcrumbs("root", Some(&e), &g);
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["root", "cluster"]);
    }
}
