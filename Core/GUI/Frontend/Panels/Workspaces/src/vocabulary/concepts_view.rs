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

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, IntoElement, MouseButton, MouseDownEvent, Render,
    SharedString, Window,
};
use wylde_gpui_input::TextInput;
use wylde_theme::colors::{BORDER_SUBTLE, BRAND, SURFACE_700, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

use super::concepts_ipc::{self, ScoredConceptView};

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
    /// Which concept card is expanded (its id).
    expanded: Option<String>,
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
            expanded: None,
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
                Ok(Some(id)) => match concepts_ipc::search_concepts(&id, &query, SEARCH_LIMIT).await {
                    Ok(list) => (Some(id), list, None),
                    Err(e) => (Some(id), Vec::new(), Some(e)),
                },
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
        self.expanded = if self.expanded.as_deref() == Some(id) {
            None
        } else {
            Some(id.to_owned())
        };
        cx.notify();
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
}

impl Render for ConceptsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws_label = self
            .workspace_id
            .clone()
            .unwrap_or_else(|| "no active workspace".to_owned());

        let mut root = div()
            .id("workspaces-concepts-subtab")
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
                .child(Self::hint(format!("{ws_label} · {} shown", self.results.len())))
                .child(div().flex_1())
                .child(Self::button(
                    ("concepts-build", 0),
                    if self.building { "Building…" } else { "Build" },
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

        // concept cards
        let mut list = div().flex().flex_col().gap_1();
        for (i, scored) in self.results.iter().enumerate() {
            let c = &scored.concept;
            let is_expanded = self.expanded.as_deref() == Some(c.id.as_str());
            let id_for_click = c.id.clone();
            let mut line = format!("{} · {}", c.label, c.source.replace('_', " "));
            line.push_str(&format!(" · {} member(s)", c.members.len()));
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
            let mut card = div()
                .id(("concept-card", i))
                .flex()
                .flex_col()
                .gap_0p5()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(if is_expanded { SURFACE_700 } else { SURFACE_800 })))
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
            list = list.child(card);
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
}
