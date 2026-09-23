//! The **Hierarchy** sub-tab (definitional-hierarchy plan H2) — a read-only,
//! drill-down view of the definitional concept DAG projected + overlaid by
//! `wylde-workspaces` (`workspaces.hierarchy.*`, H1).
//!
//! This is the **isolated, removable hierarchy folder** the plan specifies
//! (plan SS4: "a fourth `Hierarchy` sub-tab … lives or dies with the master
//! toggle and can be removed cleanly"). Everything hierarchy-GUI lives here, so
//! the feature deletes with the folder + the [`super::vocabulary::VocabSubTab`]
//! `Hierarchy` arm (the removal test).
//!
//! ## What it does
//!
//! Loads the whole applied DAG on mount via [`ipc::get_tree`] and renders it as
//! an indented, cycle-safe drill-down: roots at the top, each expandable to its
//! containment children. **Definitions are shown at every level** (truncated,
//! with the priority-ladder source badge); a `Missing` node carries a "needs
//! definition" badge (the invariant surfacing, plan SS3). Multi-parent nodes
//! show "also under: …". Selecting a node shows its definitional **ancestor
//! chain** breadcrumb (the future injection payload) and a Graph deep-link via
//! the focus bus — identical to the Concepts sub-tab's deep-link.
//!
//! ## Master toggle (fail-closed OFF)
//!
//! [`ipc::get_tree`] returns `enabled`. When the toggle is OFF the view renders
//! an **inert** disabled state with a single "Enable" button (it never shows the
//! tree, never mutates) — today's exact behaviour. H2 is read-only; the H3/H4
//! authoring affordances mount into the same selected-node detail panel.

pub mod ipc;

use std::collections::{HashMap, HashSet};

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, IntoElement, MouseButton, MouseDownEvent, Render,
    Rgba, SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    ACCENT_CYAN, BORDER_SUBTLE, BRAND, DANGER, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

use ipc::HierNodeView;
use wylde_gui_controls::control;

/// Hard caps so a pathological DAG (deep or wide) can't stall the render. A
/// shared node legitimately appears under each parent; the cycle guard stops
/// loops, these stop runaway breadth/depth.
const MAX_ROWS: usize = 4000;
const MAX_DEPTH: usize = 16;

/// One flattened, about-to-render tree row: a node id, its indent-from-root
/// depth, and whether it has containment children (so the chevron shows).
struct Row {
    id: String,
    depth: usize,
    expandable: bool,
}

/// The result of one combined tree + overlay fetch.
struct Loaded {
    tree: Option<ipc::TreeReply>,
    edges: Vec<ipc::OverlayEdgeView>,
    merges: Vec<ipc::OverlayMergeView>,
    error: Option<String>,
}

/// Fetch the applied tree, and — when enabled — the raw overlay (best-effort;
/// an overlay error degrades to an empty authoring section, never blocks the
/// tree).
async fn fetch(ws: &str) -> Loaded {
    match ipc::get_tree(ws).await {
        Ok(tree) => {
            let (edges, merges) = if tree.enabled {
                match ipc::get_overlay(ws).await {
                    Ok(ov) => (ov.edges, ov.merges),
                    Err(_) => (Vec::new(), Vec::new()),
                }
            } else {
                (Vec::new(), Vec::new())
            };
            Loaded {
                tree: Some(tree),
                edges,
                merges,
                error: None,
            }
        }
        Err(e) => Loaded {
            tree: None,
            edges: Vec::new(),
            merges: Vec::new(),
            error: Some(e),
        },
    }
}

/// The Hierarchy sub-tab view.
pub struct HierarchyView {
    workspace_id: Option<String>,
    /// Master-toggle state (from `get_tree`). OFF ⇒ inert disabled render.
    enabled: bool,
    /// The whole applied DAG.
    nodes: Vec<HierNodeView>,
    roots: Vec<String>,
    dangling_count: usize,
    loading: bool,
    error: Option<String>,
    status: Option<Result<String, String>>,
    /// Which nodes are expanded to show their children (by id).
    expanded: HashSet<String>,
    /// The selected node (shows its ancestor-chain breadcrumb + detail).
    selected: Option<String>,
    /// A toggle write is in flight.
    toggling: bool,
    /// The definition editor for the selected node (H3). Populated on select.
    def_input: Entity<TextInput>,
    /// A definition write is in flight (disables Save to avoid double-submit).
    saving: bool,
    // ── H4: edge / merge authoring ───────────────────────────────────────
    /// The raw authored containment edges (with dangling flags) — for the
    /// "authored edges" + dangling re-point section.
    overlay_edges: Vec<ipc::OverlayEdgeView>,
    /// The raw authored merges (with dangling flags).
    overlay_merges: Vec<ipc::OverlayMergeView>,
    /// Which authoring picker is open (add a child / parent / merge target).
    picker: Option<PickerMode>,
    /// Search box for the open picker (filters the loaded node universe).
    picker_search: Entity<TextInput>,
    _picker_sub: gpui::Subscription,
    /// Whether the "new node" create form is showing.
    show_create: bool,
    create_label: Entity<TextInput>,
    create_def: Entity<TextInput>,
    /// An edge / merge / create write is in flight.
    authoring: bool,
}

/// Which authoring action the open picker is choosing a target for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerMode {
    /// Add the picked node as a CHILD of the selected node.
    AddChild,
    /// Add the picked node as a PARENT of the selected node.
    AddParent,
    /// Merge the picked node (alias) INTO the selected node (primary).
    Merge,
}

impl PickerMode {
    fn title(self) -> &'static str {
        match self {
            PickerMode::AddChild => "Add child:",
            PickerMode::AddParent => "Add parent:",
            PickerMode::Merge => "Merge a node into this one:",
        }
    }
}

impl HierarchyView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let def_input = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("hierarchy-def-input")
                .with_placeholder("Write this node's definition…")
        });
        let picker_search = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("hierarchy-picker-search")
                .with_placeholder("search nodes…")
        });
        let picker_sub = cx.subscribe(
            &picker_search,
            |_this: &mut Self, _e, event: &wylde_gpui_input::InputEvent, cx| {
                if matches!(event, wylde_gpui_input::InputEvent::Changed(_)) {
                    cx.notify();
                }
            },
        );
        let create_label = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("hierarchy-create-label")
                .with_placeholder("label")
        });
        let create_def = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("hierarchy-create-def")
                .with_placeholder("definition (required)")
        });
        let view = Self {
            workspace_id: None,
            enabled: false,
            nodes: Vec::new(),
            roots: Vec::new(),
            dangling_count: 0,
            loading: true,
            error: None,
            status: None,
            expanded: HashSet::new(),
            selected: None,
            toggling: false,
            def_input,
            saving: false,
            overlay_edges: Vec::new(),
            overlay_merges: Vec::new(),
            picker: None,
            picker_search,
            _picker_sub: picker_sub,
            show_create: false,
            create_label,
            create_def,
            authoring: false,
        };
        Self::spawn_load(cx);
        view
    }

    /// Resolve the active workspace, then load the whole tree + raw overlay.
    fn spawn_load(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws_id = crate::vocabulary::ipc::active_workspace()
                .await
                .ok()
                .flatten();
            let loaded = match &ws_id {
                Some(id) => Some(fetch(id).await),
                None => None,
            };
            let _ = this.update(app_cx, |v, cx| {
                v.workspace_id = ws_id;
                v.apply_loaded(loaded, cx);
            });
        })
        .detach();
    }

    /// Reload the tree + overlay (after a toggle / authoring edit / Refresh).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let known = self.workspace_id.clone();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws_id = match known {
                Some(id) => Some(id),
                None => crate::vocabulary::ipc::active_workspace()
                    .await
                    .ok()
                    .flatten(),
            };
            let loaded = match &ws_id {
                Some(id) => Some(fetch(id).await),
                None => None,
            };
            let _ = this.update(app_cx, |v, cx| {
                v.workspace_id = ws_id;
                v.apply_loaded(loaded, cx);
            });
        })
        .detach();
    }

    /// Fold a completed [`fetch`] into the view state.
    fn apply_loaded(&mut self, loaded: Option<Loaded>, cx: &mut Context<Self>) {
        self.loading = false;
        if let Some(l) = loaded {
            self.error = l.error;
            if let Some(t) = l.tree {
                self.enabled = t.enabled;
                self.nodes = t.nodes;
                self.roots = t.roots;
                self.dangling_count = t.dangling_count;
            }
            self.overlay_edges = l.edges;
            self.overlay_merges = l.merges;
        }
        cx.notify();
    }

    /// Flip the master toggle, then reload (so OFF→ON pulls the tree in).
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.toggling = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::set_enabled(enabled).await;
            let _ = this.update(app_cx, |v, cx| {
                v.toggling = false;
                match outcome {
                    Ok(now) => {
                        v.enabled = now;
                        v.status = Some(Ok(if now {
                            "Hierarchy enabled".to_owned()
                        } else {
                            "Hierarchy disabled".to_owned()
                        }));
                        v.reload(cx);
                    }
                    Err(e) => v.status = Some(Err(format!("Toggle failed: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ── test / observability accessors ───────────────────────────────────

    /// Whether the initial load is still in flight (test accessor).
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    /// The master-toggle state the view last saw (test accessor).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    /// Number of nodes in the loaded tree (test accessor).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    /// The resolved active workspace id, if any (test accessor).
    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }
    /// The currently-selected node id (test accessor).
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    /// Expand a node (test/driver helper, no click needed).
    pub fn expand(&mut self, id: &str, cx: &mut Context<Self>) {
        self.expanded.insert(id.to_owned());
        cx.notify();
    }
    /// Select a node (test/driver helper) and populate the definition editor
    /// with its current text — so editing starts from what's there.
    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.set_selected(id, cx);
    }

    /// Select `id` and load its current definition into [`Self::def_input`].
    fn set_selected(&mut self, id: &str, cx: &mut Context<Self>) {
        self.selected = Some(id.to_owned());
        let text = self
            .index()
            .get(id)
            .filter(|n| !n.needs_definition())
            .map(|n| n.definition.text.clone())
            .unwrap_or_default();
        self.def_input
            .update(cx, |i, cx| i.set_text_silent(text, cx));
        cx.notify();
    }

    /// Author / override the selected node's definition (H3) — writes through
    /// `set_definition`, then reloads so the priority-ladder source flips to
    /// `authored`. Empty text is a no-op here (use [`Self::clear_definition`] to
    /// revert to the inherited description explicitly).
    pub fn save_definition(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        let text = self.def_input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            self.status = Some(Err(
                "Definition is empty — use Clear to revert to inherited".to_owned(),
            ));
            cx.notify();
            return;
        }
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::set_definition(&ws, Some(&id), &text, None).await;
            let _ = this.update(app_cx, |v, cx| {
                v.saving = false;
                v.status = Some(match outcome {
                    Ok(_) => Ok("Definition saved (authored)".to_owned()),
                    Err(e) => Err(format!("Save failed: {e}")),
                });
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Clear the selected node's authored override — reverts to the inherited
    /// description (or `needs definition` if there is none). Writes an empty
    /// definition, which the bridge prunes back to the projection ground state.
    pub fn clear_definition(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        self.saving = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::set_definition(&ws, Some(&id), "", None).await;
            let _ = this.update(app_cx, |v, cx| {
                v.saving = false;
                v.status = Some(match outcome {
                    Ok(_) => Ok("Authored definition cleared".to_owned()),
                    Err(e) => Err(format!("Clear failed: {e}")),
                });
                v.def_input
                    .update(cx, |i, cx| i.set_text_silent(String::new(), cx));
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Set the definition-editor draft text (test/driver helper).
    pub fn set_draft(&mut self, text: &str, cx: &mut Context<Self>) {
        self.def_input
            .update(cx, |i, cx| i.set_text_silent(text.to_owned(), cx));
        cx.notify();
    }

    // ── H4: edge / merge / new-node authoring ────────────────────────────

    /// Toggle the "new node" create form.
    fn toggle_create(&mut self, cx: &mut Context<Self>) {
        self.show_create = !self.show_create;
        cx.notify();
    }

    /// Mint a brand-new authored node from the create form (label + definition).
    /// A non-empty definition is required (the verb enforces it too).
    pub fn create_node(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        let label = self.create_label.read(cx).text().trim().to_owned();
        let def = self.create_def.read(cx).text().trim().to_owned();
        if def.is_empty() {
            self.status = Some(Err("A new node needs a definition".to_owned()));
            cx.notify();
            return;
        }
        let label_opt = (!label.is_empty()).then_some(label);
        self.authoring = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::set_definition(&ws, None, &def, label_opt.as_deref()).await;
            let _ = this.update(app_cx, |v, cx| {
                v.authoring = false;
                match outcome {
                    Ok(id) => {
                        v.status = Some(Ok(format!("Created node {id}")));
                        v.show_create = false;
                        v.create_label
                            .update(cx, |i, cx| i.set_text_silent(String::new(), cx));
                        v.create_def
                            .update(cx, |i, cx| i.set_text_silent(String::new(), cx));
                        v.reload(cx);
                    }
                    Err(e) => v.status = Some(Err(format!("Create failed: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the target picker for an authoring action on the selected node.
    fn begin_picker(&mut self, mode: PickerMode, cx: &mut Context<Self>) {
        self.picker = Some(mode);
        self.picker_search
            .update(cx, |i, cx| i.set_text_silent(String::new(), cx));
        cx.notify();
    }

    /// Close the picker without acting.
    fn cancel_picker(&mut self, cx: &mut Context<Self>) {
        self.picker = None;
        cx.notify();
    }

    /// Candidate nodes for the open picker: the loaded universe filtered by the
    /// picker search (case-insensitive substring on label or id), excluding the
    /// selected node, capped for the dropdown.
    fn picker_candidates(&self, cx: &Context<Self>) -> Vec<(String, String)> {
        let q = self.picker_search.read(cx).text().trim().to_lowercase();
        let sel = self.selected.clone().unwrap_or_default();
        self.nodes
            .iter()
            .filter(|n| n.id != sel)
            .filter(|n| {
                q.is_empty()
                    || n.label.to_lowercase().contains(&q)
                    || n.id.to_lowercase().contains(&q)
            })
            .take(20)
            .map(|n| (n.id.clone(), n.label.clone()))
            .collect()
    }

    /// Act on a picked target according to the open picker mode, then reload.
    fn pick_target(&mut self, target: &str, cx: &mut Context<Self>) {
        let Some(mode) = self.picker else { return };
        self.dispatch_pick(mode, target, cx);
    }

    /// Add `target` as a child of the selected node (test/driver helper).
    pub fn add_child(&mut self, target: &str, cx: &mut Context<Self>) {
        self.dispatch_pick(PickerMode::AddChild, target, cx);
    }
    /// Add `target` as a parent of the selected node (test/driver helper).
    pub fn add_parent(&mut self, target: &str, cx: &mut Context<Self>) {
        self.dispatch_pick(PickerMode::AddParent, target, cx);
    }
    /// Merge `alias` into the selected node (test/driver helper).
    pub fn merge_into(&mut self, alias: &str, cx: &mut Context<Self>) {
        self.dispatch_pick(PickerMode::Merge, alias, cx);
    }

    /// Run an authoring action against `target` for the selected node, then
    /// reload. No-op without a selection / workspace.
    fn dispatch_pick(&mut self, mode: PickerMode, target: &str, cx: &mut Context<Self>) {
        let Some(sel) = self.selected.clone() else {
            return;
        };
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        let target = target.to_owned();
        self.picker = None;
        self.authoring = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = match mode {
                PickerMode::AddChild => ipc::add_edge(&ws, &sel, &target).await,
                PickerMode::AddParent => ipc::add_edge(&ws, &target, &sel).await,
                PickerMode::Merge => ipc::merge_nodes(&ws, &sel, &target).await,
            };
            let _ = this.update(app_cx, |v, cx| {
                v.authoring = false;
                v.status = Some(match outcome {
                    Ok(()) => Ok(match mode {
                        PickerMode::AddChild => "Added child edge".to_owned(),
                        PickerMode::AddParent => "Added parent edge".to_owned(),
                        PickerMode::Merge => "Merged node".to_owned(),
                    }),
                    Err(e) => Err(format!("Authoring failed: {e}")),
                });
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Remove one authored containment edge (the ✕ on an authored / dangling
    /// edge). Re-point = remove here, then Add child/parent afresh.
    pub fn remove_edge(&mut self, parent: &str, child: &str, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        let (parent, child) = (parent.to_owned(), child.to_owned());
        self.authoring = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::remove_edge(&ws, &parent, &child).await;
            let _ = this.update(app_cx, |v, cx| {
                v.authoring = false;
                v.status = Some(match outcome {
                    Ok(_) => Ok("Edge removed".to_owned()),
                    Err(e) => Err(format!("Remove failed: {e}")),
                });
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Undo a merge (the alias re-appears as its own node).
    pub fn remove_merge(&mut self, primary: &str, alias: &str, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        let (primary, alias) = (primary.to_owned(), alias.to_owned());
        self.authoring = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = ipc::remove_merge(&ws, &primary, &alias).await;
            let _ = this.update(app_cx, |v, cx| {
                v.authoring = false;
                v.status = Some(match outcome {
                    Ok(_) => Ok("Merge undone".to_owned()),
                    Err(e) => Err(format!("Unmerge failed: {e}")),
                });
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Populate the create form (test/driver helper).
    pub fn set_create_draft(&mut self, label: &str, def: &str, cx: &mut Context<Self>) {
        self.create_label
            .update(cx, |i, cx| i.set_text_silent(label.to_owned(), cx));
        self.create_def
            .update(cx, |i, cx| i.set_text_silent(def.to_owned(), cx));
        cx.notify();
    }

    /// The number of authored overlay edges currently loaded (test accessor).
    pub fn overlay_edge_count(&self) -> usize {
        self.overlay_edges.len()
    }
    /// Whether the picker is open (test accessor).
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    // ── internal helpers ─────────────────────────────────────────────────

    fn index(&self) -> HashMap<&str, &HierNodeView> {
        self.nodes.iter().map(|n| (n.id.as_str(), n)).collect()
    }

    fn toggle_expand(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }

    /// Deep-link the Graph tab to a node's underlying source id (focus bus →
    /// the Workspaces panel selects the Graph tab and focuses the node) — the
    /// same path the Concepts sub-tab uses.
    fn focus_in_graph(source_id: String) {
        wylde_gui_pipe::request_workspace_focus(wylde_gui_pipe::WorkspaceFocus {
            tab: Some("graph".to_owned()),
            node_id: Some(source_id),
        });
    }

    /// Flatten the visible tree (roots → expanded descendants) into rows,
    /// depth-first, cycle-safe (a per-path visited set), bounded by
    /// [`MAX_ROWS`] / [`MAX_DEPTH`]. A shared multi-parent node appears under
    /// each parent it is expanded beneath — the DAG rendered nested (plan SS2.1).
    fn flatten(&self) -> Vec<Row> {
        let idx = self.index();
        let mut rows = Vec::new();
        // Sort roots by label for a stable order.
        let mut roots = self.roots.clone();
        roots.sort_by_key(|id| self.label_of(&idx, id));
        for r in &roots {
            self.walk(&idx, r, 0, &mut Vec::new(), &mut rows);
            if rows.len() >= MAX_ROWS {
                break;
            }
        }
        rows
    }

    fn walk<'a>(
        &self,
        idx: &HashMap<&'a str, &'a HierNodeView>,
        id: &str,
        depth: usize,
        path: &mut Vec<String>,
        rows: &mut Vec<Row>,
    ) {
        if rows.len() >= MAX_ROWS || depth > MAX_DEPTH {
            return;
        }
        if path.iter().any(|p| p == id) {
            return; // cycle on this path: stop
        }
        let Some(node) = idx.get(id) else { return };
        let expandable = !node.children.is_empty();
        rows.push(Row {
            id: id.to_owned(),
            depth,
            expandable,
        });
        if expandable && self.expanded.contains(id) {
            path.push(id.to_owned());
            let mut kids = node.children.clone();
            kids.sort_by_key(|id| self.label_of(idx, id));
            for c in &kids {
                self.walk(idx, c, depth + 1, path, rows);
            }
            path.pop();
        }
    }

    fn label_of(&self, idx: &HashMap<&str, &HierNodeView>, id: &str) -> String {
        idx.get(id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// The definitional ancestor chain of a node (nearest-first, start first),
    /// computed client-side along the primary (first) parent — the same chain
    /// `get_node` returns, without a round-trip. Cycle-safe.
    fn ancestor_chain(&self, start: &str) -> Vec<String> {
        let idx = self.index();
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = start.to_owned();
        while let Some(node) = idx.get(cur.as_str()) {
            if !seen.insert(cur.clone()) {
                break;
            }
            chain.push(cur.clone());
            match node.parents.first() {
                Some(p) => cur = p.clone(),
                None => break,
            }
        }
        chain
    }

    // ── element helpers (Vocabulary-tab idioms) ──────────────────────────

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

    /// A small coloured pill (kind / needs-definition / source badge).
    fn badge(text: &str, color: Rgba) -> gpui::Div {
        div()
            .px_1()
            .rounded(px(3.0))
            .bg(rgb(pack(SURFACE_700)))
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(color)))
            .child(SharedString::from(text.to_owned()))
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
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    on_click(this, cx)
                }),
            )
    }

    /// Truncate a definition for the inline row (the full text shows in the
    /// selected-node detail).
    fn truncate(text: &str, max: usize) -> String {
        let t = text.trim();
        if t.chars().count() <= max {
            return t.to_owned();
        }
        let cut: String = t.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }

    /// The kind badge colour.
    fn kind_color(kind: &str) -> Rgba {
        match kind {
            "concept" => ACCENT_CYAN,
            "vocab" => BRAND,
            _ => TEXT_MUTED, // authored
        }
    }

    /// Render one drill-down row.
    fn render_row(
        &self,
        ri: usize,
        row: &Row,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let idx = self.index();
        let node = idx
            .get(row.id.as_str())
            .copied()
            .cloned()
            .unwrap_or_default();
        let is_selected = self.selected.as_deref() == Some(row.id.as_str());
        let is_expanded = self.expanded.contains(&row.id);
        let chevron = if !row.expandable {
            "  "
        } else if is_expanded {
            "▾"
        } else {
            "▸"
        };

        // header line: chevron · label · kind · (needs-def) · also-under
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(chevron)),
            )
            .child(
                div()
                    .text_size(px(size::XS))
                    .font_weight(gpui::FontWeight(weight::MEDIUM as f32))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(node.label.clone())),
            )
            .child(Self::badge(&node.kind, Self::kind_color(&node.kind)));
        if node.needs_definition() {
            header = header.child(Self::badge("needs definition", DANGER));
        }
        // multi-parent: list the OTHER parents.
        if node.parents.len() > 1 {
            let others: Vec<String> = node
                .parents
                .iter()
                .map(|p| self.label_of(&idx, p))
                .collect();
            header = header.child(Self::hint(format!("also under: {}", others.join(", "))));
        }

        let def_line = if node.needs_definition() {
            Self::hint("— no definition yet —".to_owned())
        } else {
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(Self::truncate(
                    &node.definition.text,
                    100,
                )))
        };

        let id_for_click = row.id.clone();
        control(div(), ("hier-row", ri))
            .flex()
            .flex_col()
            .gap_0p5()
            .pl(px((row.depth as f32) * 16.0 + 4.0))
            .px_2()
            .py_0p5()
            .rounded(px(4.0))
            .bg(rgb(pack(if is_selected {
                SURFACE_700
            } else {
                SURFACE_800
            })))
            .cursor_pointer()
            .child(header)
            .child(def_line)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                    // Clicking a row selects it (loading its definition into the
                    // editor); if it has children, also toggles expansion.
                    this.set_selected(&id_for_click, cx);
                    this.toggle_expand(&id_for_click, cx);
                }),
            )
    }

    /// The selected-node detail panel: full definition, source rung, ancestor
    /// chain breadcrumb, and a Graph deep-link. (H3 mounts the definition editor
    /// here.)
    fn render_detail(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let id = self.selected.as_deref()?;
        let idx = self.index();
        let node = idx.get(id).copied()?.clone();

        // Breadcrumb: ancestor chain, root-first (reverse of nearest-first).
        let mut chain = self.ancestor_chain(id);
        chain.reverse();
        let crumb = chain
            .iter()
            .map(|c| self.label_of(&idx, c))
            .collect::<Vec<_>>()
            .join("  ›  ");

        let mut detail = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded(px(4.0))
            .bg(rgb(pack(SURFACE_700)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(Self::heading(&node.label))
                    .child(Self::badge(&node.kind, Self::kind_color(&node.kind)))
                    .child(div().flex_1())
                    .child(Self::button(
                        ("hier-view-graph", 0),
                        "View in graph",
                        false,
                        cx,
                        {
                            let src = node.source_id().to_owned();
                            move |_this, _cx| Self::focus_in_graph(src.clone())
                        },
                    )),
            )
            .child(Self::hint(format!("path:  {crumb}")));

        if node.needs_definition() {
            detail = detail.child(Self::badge("needs definition", DANGER));
        } else {
            detail = detail.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(node.definition.text.clone())),
            );
        }
        // The priority-ladder source rung — always shown (plan H3 "the priority
        // ladder shows the source"): authored | inherited_* | llm_draft | missing.
        detail = detail.child(Self::hint(format!("source: {}", node.definition.source)));

        // ── H3: definition editor ────────────────────────────────────────
        detail = detail
            .child(Self::hint(
                "Edit definition (authored overrides the inherited one):".to_owned(),
            ))
            .child(div().child(self.def_input.clone()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(Self::button(
                        ("hier-save-def", 0),
                        if self.saving {
                            "Saving…"
                        } else {
                            "Save definition"
                        },
                        true,
                        cx,
                        |this, cx| this.save_definition(cx),
                    ))
                    .child(Self::button(
                        ("hier-clear-def", 0),
                        "Clear override",
                        false,
                        cx,
                        |this, cx| this.clear_definition(cx),
                    )),
            );

        // ── H4: containment + merge authoring on the selected node ───────
        detail = detail.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .pt_1()
                .child(Self::button(
                    ("hier-add-child", 0),
                    "Add child",
                    false,
                    cx,
                    |this, cx| this.begin_picker(PickerMode::AddChild, cx),
                ))
                .child(Self::button(
                    ("hier-add-parent", 0),
                    "Add parent",
                    false,
                    cx,
                    |this, cx| this.begin_picker(PickerMode::AddParent, cx),
                ))
                .child(Self::button(
                    ("hier-merge", 0),
                    "Merge into…",
                    false,
                    cx,
                    |this, cx| this.begin_picker(PickerMode::Merge, cx),
                )),
        );

        // The open target picker (search + candidate buttons + cancel).
        if let Some(mode) = self.picker {
            let mut picker = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_800)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(Self::hint(mode.title().to_owned()))
                        .child(div().flex_1())
                        .child(Self::button(
                            ("hier-picker-cancel", 0),
                            "Cancel",
                            false,
                            cx,
                            |this, cx| this.cancel_picker(cx),
                        )),
                )
                .child(div().child(self.picker_search.clone()));
            for (ci, (id, label)) in self.picker_candidates(cx).into_iter().enumerate() {
                let target = id.clone();
                picker = picker.child(
                    control(div(), ("hier-cand", ci))
                        .px_2()
                        .py_0p5()
                        .rounded(px(3.0))
                        .bg(rgb(pack(SURFACE_700)))
                        .cursor_pointer()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(format!("{label}  ·  {id}")))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.pick_target(&target, cx);
                            }),
                        ),
                );
            }
            detail = detail.child(picker);
        }
        Some(detail)
    }

    /// The "new node" create form (H4) — mint a brand-new authored node.
    fn render_create(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut col = div().flex().flex_col().gap_1().child(Self::button(
            ("hier-new-node", 0),
            if self.show_create {
                "× New node"
            } else {
                "+ New node"
            },
            false,
            cx,
            |this, cx| this.toggle_create(cx),
        ));
        if self.show_create {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded(px(4.0))
                    .bg(rgb(pack(SURFACE_800)))
                    .child(div().child(self.create_label.clone()))
                    .child(div().child(self.create_def.clone()))
                    .child(Self::button(
                        ("hier-create", 0),
                        if self.authoring {
                            "Creating…"
                        } else {
                            "Create node"
                        },
                        true,
                        cx,
                        |this, cx| this.create_node(cx),
                    )),
            );
        }
        col
    }

    /// The authored-overlay section (H4): authored containment edges with a ✕,
    /// dangling ones flagged for re-point, and authored merges with an undo.
    fn render_overlay_section(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.overlay_edges.is_empty() && self.overlay_merges.is_empty() {
            return None;
        }
        let idx = self.index();
        let label = |id: &str| {
            idx.get(id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.to_owned())
        };

        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pt_2()
            .child(Self::heading("Authored edges & merges"));

        for (ei, e) in self.overlay_edges.iter().enumerate() {
            let line = format!("{}  →  {}", label(&e.parent), label(&e.child));
            let mut row = div().flex().flex_row().items_center().gap_2().child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(line)),
            );
            if e.dangling {
                row = row.child(Self::badge("dangling — re-point", DANGER));
            }
            let (p, c) = (e.parent.clone(), e.child.clone());
            row = row.child(div().flex_1()).child(Self::button(
                ("hier-edge-rm", ei),
                "✕",
                false,
                cx,
                move |this, cx| this.remove_edge(&p, &c, cx),
            ));
            section = section.child(row);
        }

        for (mi, m) in self.overlay_merges.iter().enumerate() {
            let line = format!("merge: {}  ⇐  {}", label(&m.primary), label(&m.alias));
            let mut row = div().flex().flex_row().items_center().gap_2().child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(line)),
            );
            if m.dangling {
                row = row.child(Self::badge("dangling", DANGER));
            }
            let (p, a) = (m.primary.clone(), m.alias.clone());
            row = row.child(div().flex_1()).child(Self::button(
                ("hier-merge-rm", mi),
                "undo",
                false,
                cx,
                move |this, cx| this.remove_merge(&p, &a, cx),
            ));
            section = section.child(row);
        }
        Some(section)
    }
}

impl Render for HierarchyView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "no active workspace".to_owned());

        let mut root = div()
            .id("workspaces-hierarchy-subtab")
            .flex()
            .flex_col()
            .gap_3()
            .font_family(FAMILY_INTER);

        // header
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(Self::heading("Hierarchy"));
        if self.enabled {
            header = header
                .child(Self::hint(format!(
                    "{ws_label} · {} node{}{}",
                    self.nodes.len(),
                    if self.nodes.len() == 1 { "" } else { "s" },
                    if self.dangling_count > 0 {
                        format!(" · {} dangling", self.dangling_count)
                    } else {
                        String::new()
                    }
                )))
                .child(div().flex_1())
                .child(Self::button(
                    ("hier-refresh", 0),
                    "Refresh",
                    false,
                    cx,
                    |this, cx| this.reload(cx),
                ))
                .child(Self::button(
                    ("hier-toggle", 0),
                    if self.toggling { "…" } else { "Disable" },
                    false,
                    cx,
                    |this, cx| this.set_enabled(false, cx),
                ));
        }
        root = root.child(header);

        if self.loading {
            return root.child(Self::hint("Loading hierarchy…".to_owned()));
        }

        // Master toggle OFF ⇒ inert disabled state (plan SS4).
        if !self.enabled {
            return root
                .child(Self::hint(
                    "The concept hierarchy is off. It projects a navigable, \
                     definition-per-node DAG from your concepts, vocabulary, and \
                     relations. Enabling it has no effect on retrieval yet (browse-only)."
                        .to_owned(),
                ))
                .child(div().child(Self::button(
                    ("hier-enable", 0),
                    if self.toggling {
                        "Enabling…"
                    } else {
                        "Enable hierarchy"
                    },
                    true,
                    cx,
                    |this, cx| this.set_enabled(true, cx),
                )))
                .child(maybe_status(&self.status));
        }

        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(format!("Hierarchy unreachable — {err}"))),
            );
        }

        // H4: the "new node" create form.
        root = root.child(self.render_create(cx));

        // The selected-node detail (breadcrumb + definition + editor +
        // containment/merge authoring).
        if let Some(detail) = self.render_detail(cx) {
            root = root.child(detail);
        }

        if self.nodes.is_empty() {
            root = root.child(Self::hint(
                "No nodes yet. Build concepts and add vocabulary terms; the \
                 hierarchy projects them into a drill-down DAG."
                    .to_owned(),
            ));
        }

        // The drill-down tree.
        let rows = self.flatten();
        let mut list = div().flex().flex_col().gap_0p5();
        for (ri, row) in rows.iter().enumerate() {
            list = list.child(self.render_row(ri, row, cx));
        }
        root = root.child(list);

        // H4: authored edges + merges (remove / re-point dangling).
        if let Some(section) = self.render_overlay_section(cx) {
            root = root.child(section);
        }

        root = root.child(maybe_status(&self.status));
        root.border_t_1().border_color(rgb(pack(BORDER_SUBTLE)))
    }
}

/// Render the inline status line (Ok info / Err failure), or an empty div.
fn maybe_status(status: &Option<Result<String, String>>) -> gpui::Div {
    match status {
        Some(Ok(msg)) => HierarchyView::hint(msg.clone()),
        Some(Err(msg)) => div()
            .text_size(px(size::XS))
            .text_color(rgb(0xE57373))
            .child(SharedString::from(msg.clone())),
        None => div(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<HierarchyView>();
    }

    #[test]
    fn truncate_caps_long_definitions() {
        assert_eq!(HierarchyView::truncate("short", 100), "short");
        let long = "x".repeat(150);
        let out = HierarchyView::truncate(&long, 100);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 101); // 100 + ellipsis
    }
}
