//! The **Concepts** sub-tab (TBS concept-system Phase 1, thesis §4.1).
//!
//! Lists the workspace's discovered concepts as cards; clicking a card expands
//! it to show the files involved and deep-links the Graph tab (via the focus
//! bus, `wylde_gui_pipe::request_workspace_focus`) to the concept's first
//! member — the same path the composer uses to centre the graph on a symbol.
//! A search box drives the hybrid (fuzzy + semantic) `workspaces.concepts.search`
//! verb; a **Build** button runs the Phase-0 cheap-concept pass.
//!
//! Lives beside the Vocabulary sub-tab inside [`super::VocabularyTab`]; the two
//! are the two halves of the Vocabulary tab's "browse the map" surface.

use std::collections::{BTreeSet, HashSet};

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, IntoElement, MouseButton, MouseDownEvent, Render,
    SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

use super::concepts_ipc::{self, ConceptView, ScoredConceptView};
use wylde_gui_controls::control;

/// The separator the backend inserts between a concept's base label and its
/// disambiguating token (`Composer · models`). Same-base-name concepts group
/// under one parent row keyed on the part before it.
const LABEL_SEP: &str = " · ";

/// The base label a concept groups under — everything before [`LABEL_SEP`].
fn base_label(label: &str) -> &str {
    label.split(LABEL_SEP).next().unwrap_or(label).trim()
}

/// The directory + file spread pill shown on each card (A.2): replaces the
/// redundant `embedding` provenance tag with a per-concept signal. `M` is the
/// number of distinct directories the member files span; `N` is the file count.
fn dir_file_signal(c: &ConceptView) -> String {
    let dirs: BTreeSet<&str> = c
        .member_files
        .iter()
        .filter_map(|f| parent_dir(f))
        .collect();
    let m = dirs.len();
    let n = c.member_files.len();
    format!(
        "{m} dir{} · {n} file{}",
        if m == 1 { "" } else { "s" },
        if n == 1 { "" } else { "s" }
    )
}

/// The parent directory path of a file (everything before the last separator).
fn parent_dir(path: &str) -> Option<&str> {
    path.rfind(['/', '\\']).map(|i| &path[..i])
}

/// Max search results requested per query.
const SEARCH_LIMIT: usize = 200;

/// The Concepts sub-tab view.
pub struct ConceptsView {
    workspace_id: Option<String>,
    results: Vec<ScoredConceptView>,
    loading: bool,
    building: bool,
    error: Option<String>,
    status: Option<Result<String, String>>,
    search: Entity<TextInput>,
    _search_sub: gpui::Subscription,
    /// Which concept cards are expanded to their file-list detail (by id). A
    /// set (not a single id) so several cards — and a parent group plus a child
    /// — can be open at once.
    expanded: HashSet<String>,
    /// Which same-base-name parent groups are expanded to show their children
    /// (by base label). Collapsed by default.
    expanded_groups: HashSet<String>,
}

impl ConceptsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let search = cx.new(|c| {
            TextInput::single_line(c)
                .with_submit_mode(wylde_gpui_input::SubmitMode::Never)
                .with_element_key("concepts-search")
                .with_placeholder("Search concepts by name or meaning…")
        });
        let search_sub = cx.subscribe(
            &search,
            |this: &mut Self, _e, event: &wylde_gpui_input::InputEvent, cx| {
                if matches!(event, wylde_gpui_input::InputEvent::Changed(_)) {
                    this.reload(cx);
                }
            },
        );
        let view = Self {
            workspace_id: None,
            results: Vec::new(),
            loading: true,
            building: false,
            error: None,
            status: None,
            search,
            _search_sub: search_sub,
            expanded: HashSet::new(),
            expanded_groups: HashSet::new(),
        };
        Self::spawn_load(cx);
        view
    }

    /// Resolve the active workspace, then (re)run the search with the current
    /// query. Empty query → the full concept set ordered by label.
    fn spawn_load(cx: &mut Context<Self>) {
        let query = String::new();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws = super::ipc::active_workspace().await;
            let (ws_id, results, error) = match ws {
                Ok(Some(id)) => {
                    match concepts_ipc::search_concepts(&id, &query, SEARCH_LIMIT).await {
                        Ok(list) => (Some(id), list, None),
                        Err(e) => (Some(id), Vec::new(), Some(e)),
                    }
                }
                Ok(None) => (None, Vec::new(), None),
                Err(e) => (None, Vec::new(), Some(e)),
            };
            let _ = this.update(app_cx, |v, cx| {
                v.loading = false;
                v.workspace_id = ws_id;
                v.results = results;
                v.error = error;
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-run the search verb with the current query (used on every keystroke
    /// and after a build). Reuses the known workspace id; falls back to
    /// resolving it when not yet loaded.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().to_owned();
        let known = self.workspace_id.clone();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let ws_id = match known {
                Some(id) => Some(id),
                None => super::ipc::active_workspace().await.ok().flatten(),
            };
            let (results, error) = match &ws_id {
                Some(id) => match concepts_ipc::search_concepts(id, &query, SEARCH_LIMIT).await {
                    Ok(list) => (list, None),
                    Err(e) => (Vec::new(), Some(e)),
                },
                None => (Vec::new(), None),
            };
            let _ = this.update(app_cx, |v, cx| {
                v.loading = false;
                v.workspace_id = ws_id;
                v.results = results;
                v.error = error;
                cx.notify();
            });
        })
        .detach();
    }

    /// Number of concept cards currently shown (test/observability accessor).
    pub fn results_len(&self) -> usize {
        self.results.len()
    }

    /// Whether the initial load is still in flight (test accessor).
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// The resolved active workspace id, if any (test accessor).
    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }

    /// Run the Phase-0 cheap-concept build, then reload. Public so a windowed
    /// test can drive the build path without simulating a click.
    pub fn build(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            self.status = Some(Err("No active workspace to build concepts for".to_owned()));
            cx.notify();
            return;
        };
        self.building = true;
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut gpui::AsyncApp| {
            let outcome = concepts_ipc::build_concepts(&ws).await;
            let _ = this.update(app_cx, |v, cx| {
                v.building = false;
                v.status = Some(match outcome {
                    Ok(n) => Ok(format!("Built {n} concept(s) from directory clusters")),
                    Err(e) => Err(format!("Build failed: {e}")),
                });
                v.reload(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Deep-link the Graph tab to a concept member (focus bus → the Workspaces
    /// panel's drain selects the Graph tab and focuses the node).
    fn focus_in_graph(node_id: String) {
        wylde_gui_pipe::request_workspace_focus(wylde_gui_pipe::WorkspaceFocus {
            tab: Some("graph".to_owned()),
            node_id: Some(node_id),
        });
    }

    fn toggle_expand(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }

    fn toggle_group(&mut self, base: &str, cx: &mut Context<Self>) {
        if !self.expanded_groups.remove(base) {
            self.expanded_groups.insert(base.to_owned());
        }
        cx.notify();
    }

    /// Group the current results by base label, in first-seen order. Returns
    /// `(base, indices-into-results)`; a group of one renders as a plain card,
    /// a group of many as a collapsible parent row.
    fn grouped(&self) -> Vec<(String, Vec<usize>)> {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, scored) in self.results.iter().enumerate() {
            let base = base_label(&scored.concept.label).to_owned();
            match at.get(&base) {
                Some(&g) => groups[g].1.push(i),
                None => {
                    at.insert(base.clone(), groups.len());
                    groups.push((base, vec![i]));
                }
            }
        }
        groups
    }

    /// Render one concept card: the header line (label + A.2 `M dirs · N files`
    /// pill + optional match scores), the description, and — when expanded — the
    /// file list plus a graph deep-link. `label_override` lets a child card show
    /// just its disambiguating token (`models`) instead of the full
    /// `Composer · models` already implied by its parent row.
    fn concept_card(
        &self,
        i: usize,
        label_override: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let scored = &self.results[i];
        let c = &scored.concept;
        let is_expanded = self.expanded.contains(&c.id);
        let id_for_click = c.id.clone();
        let label = label_override.unwrap_or(c.label.as_str());
        let mut line = format!("{label} · {}", dir_file_signal(c));
        if !c.parent_concepts.is_empty() {
            line.push_str(&format!(" · under {}", c.parent_concepts.join(", ")));
        }
        // explainability: show which signal matched when searching.
        if scored.fuzzy > 0.0 || scored.semantic > 0.0 {
            line.push_str(&format!(
                " · match {:.0}% name / {:.0}% meaning",
                scored.fuzzy * 100.0,
                scored.semantic * 100.0
            ));
        }
        let mut card = control(div(), ("concept-card", i))
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .py_1()
            .rounded(px(4.0))
            .bg(rgb(pack(if is_expanded {
                SURFACE_700
            } else {
                SURFACE_800
            })))
            .cursor_pointer()
            .child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(line)),
            )
            .child(Self::hint(c.description.clone()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                    this.toggle_expand(&id_for_click, cx);
                }),
            );

        if is_expanded {
            // "Files involved" + a graph deep-link to the first member.
            let mut detail = div().flex().flex_col().gap_0p5().pt_1();
            if c.member_files.is_empty() {
                detail = detail.child(Self::hint("no files recorded".to_owned()));
            } else {
                detail = detail.child(Self::hint("files involved:".to_owned()));
                for f in c.member_files.iter().take(40) {
                    detail = detail.child(
                        div()
                            .text_size(px(size::MICRO))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(f.clone())),
                    );
                }
            }
            if let Some(first_member) = c.members.first().cloned() {
                detail = detail.child(div().pt_1().child(Self::button(
                    ("concept-view-graph", i),
                    "View in graph",
                    false,
                    cx,
                    move |_this, _cx| Self::focus_in_graph(first_member.clone()),
                )));
            }
            card = card.child(detail);
        }
        card
    }

    /// The disambiguating token a child card shows under its parent row — the
    /// part of the label after the base (`Composer · models` → `models`).
    fn child_token<'a>(label: &'a str, base: &str) -> &'a str {
        label
            .strip_prefix(base)
            .map(|rest| rest.trim_start_matches(LABEL_SEP))
            .filter(|t| !t.is_empty())
            .unwrap_or(label)
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
}

impl Render for ConceptsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "no active workspace".to_owned());

        let mut root = control(div(), "workspaces-concepts-subtab")
            .flex()
            .flex_col()
            .gap_3()
            .font_family(FAMILY_INTER);

        // header
        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(Self::heading("Concepts"))
                .child(Self::hint(format!(
                    "{ws_label} · {} shown",
                    self.results.len()
                )))
                .child(div().flex_1())
                .child(Self::button(
                    ("concepts-build", 0),
                    if self.building {
                        "Building…"
                    } else {
                        "Build"
                    },
                    true,
                    cx,
                    |this, cx| this.build(cx),
                ))
                .child(Self::button(
                    ("concepts-refresh", 0),
                    "Refresh",
                    false,
                    cx,
                    |this, cx| this.reload(cx),
                )),
        );
        root = root.child(div().child(self.search.clone()));

        if self.loading {
            return root.child(Self::hint("Loading concepts…".to_owned()));
        }
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(0xE57373))
                    .child(SharedString::from(format!(
                        "Concept store unreachable — {err}"
                    ))),
            );
        }

        if self.results.is_empty() {
            root = root.child(Self::hint(
                "No concepts yet. Click Build to label this workspace's directory clusters \
                 into stand-in concepts (semantic clustering refines them later)."
                    .to_owned(),
            ));
        }

        // Concept cards, grouped by base label (A.3). A base name shared by
        // several semantically-distinct clusters collapses under one parent row
        // ("Composer (3)"); its disambiguated children render indented beneath
        // when the row is expanded. A base owned by a single concept renders as
        // a plain card.
        let mut list = div().flex().flex_col().gap_1();
        for (gi, (base, idxs)) in self.grouped().into_iter().enumerate() {
            if idxs.len() == 1 {
                list = list.child(self.concept_card(idxs[0], None, cx));
                continue;
            }
            let expanded = self.expanded_groups.contains(&base);
            let chevron = if expanded { "▾" } else { "▸" };
            let base_for_click = base.clone();
            let parent = control(div(), ("concept-group", gi))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(SURFACE_700)))
                .cursor_pointer()
                .child(
                    div()
                        .text_size(px(size::XS))
                        .font_weight(gpui::FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(format!(
                            "{chevron} {base} ({})",
                            idxs.len()
                        ))),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                        this.toggle_group(&base_for_click, cx);
                    }),
                );
            list = list.child(parent);
            if expanded {
                for &i in &idxs {
                    let token = Self::child_token(&self.results[i].concept.label, &base).to_owned();
                    let card = self.concept_card(i, Some(&token), cx);
                    list = list.child(div().pl(px(16.0)).child(card));
                }
            }
        }
        root = root.child(list);

        if let Some(status) = &self.status {
            root = root.child(match status {
                Ok(msg) => Self::hint(msg.clone()),
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
        assert_render::<ConceptsView>();
    }

    #[test]
    fn base_label_splits_on_separator() {
        assert_eq!(base_label("Composer · models"), "Composer");
        assert_eq!(base_label("Graph"), "Graph");
        // Multi-segment label still groups under the first segment.
        assert_eq!(base_label("Conf · mod · 0007"), "Conf");
    }

    #[test]
    fn child_token_strips_the_base() {
        assert_eq!(
            ConceptsView::child_token("Composer · models", "Composer"),
            "models"
        );
        // A label that is exactly the base falls back to the whole label.
        assert_eq!(ConceptsView::child_token("Graph", "Graph"), "Graph");
        // Multi-segment token keeps everything after the base.
        assert_eq!(
            ConceptsView::child_token("Conf · mod · 0007", "Conf"),
            "mod · 0007"
        );
    }

    #[test]
    fn dir_file_signal_counts_dirs_and_files() {
        let c = ConceptView {
            member_files: vec![
                "src/a/x.rs".into(),
                "src/a/y.rs".into(),
                "src/b/z.rs".into(),
            ],
            ..Default::default()
        };
        assert_eq!(dir_file_signal(&c), "2 dirs · 3 files");

        let one = ConceptView {
            member_files: vec!["src/a/x.rs".into()],
            ..Default::default()
        };
        assert_eq!(dir_file_signal(&one), "1 dir · 1 file");
    }
}
