//! Workspaces panel View.
//!
//! State (held inline on the View):
//!   * `workspaces` — last-read `workspaces.list_mru` reply.
//!   * `active_id`  — currently-active workspace as the user sees it
//!     (set on "Switch" click + initialised from the MRU head; the
//!     "Switch" handler also persists it via `workspaces.set_active`).
//!   * `error`      — last pipe error.  Surfaced as a red strip at
//!     the top of the body so the user knows the panel is stale
//!     rather than silently empty.
//!   * `loading`    — `true` until the first `workspaces.list_mru`
//!     reply arrives; the View paints a "Loading…" row in the
//!     interim instead of a blank pane.
//!
//! IPC reads use `cx.spawn` — same pattern slice 2's Settings panel
//! adopts.  The "Add workspace" button uses `rfd::FileDialog` from a
//! blocking dispatcher task; the picker doesn't have a non-blocking
//! API on Windows.

use std::path::PathBuf;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, AnyView, App, AppContext, AsyncApp, Context, ElementId,
    Entity, FontWeight, IntoElement, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::editor::EditorTab;
use crate::files::FilesTab;
use crate::graph::GraphView;
use crate::ipc::{
    activate_workspace, delete_workspace, list_workspaces, reindex_workspace, set_active_workspace,
    WorkspaceSummary,
};
use crate::settings_tab::GraphSettingsTab;
use crate::tabs::WorkspacesTab;
use crate::vocabulary::VocabularyTab;

/// Root Workspaces panel. Hosts a minimal tab system (Registry + Graph);
/// the active tab's body is rendered below a tab bar.
pub struct WorkspacesPanel {
    pub workspaces: Vec<WorkspaceSummary>,
    pub active_id: Option<String>,
    pub error: Option<String>,
    pub loading: bool,
    /// The selected tab.
    pub tab: WorkspacesTab,
    /// The Graph tab's view, created once at mount (loads the active
    /// workspace's code graph). `None` only in unit-test construction, where
    /// no gpui context exists to create a child entity.
    pub graph: Option<Entity<GraphView>>,
    /// The Settings tab's view (Slice C-settings: profile library + graph
    /// knob editors). Same `None`-in-unit-tests caveat as `graph`.
    pub settings: Option<Entity<GraphSettingsTab>>,
    /// The Vocabulary tab's view (Slice N: the anchor system UI). Same
    /// `None`-in-unit-tests caveat.
    pub vocabulary: Option<Entity<VocabularyTab>>,
    /// The Files tab's view (IDE S5: lazy workspace file-tree). Same
    /// `None`-in-unit-tests caveat.
    pub files: Option<Entity<FilesTab>>,
    /// The Editor tab's view (IDE S3/S4: the code editor). Same
    /// `None`-in-unit-tests caveat.
    pub editor: Option<Entity<EditorTab>>,
}

impl WorkspacesPanel {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active_id: None,
            error: None,
            loading: true,
            tab: WorkspacesTab::Registry,
            graph: None,
            settings: None,
            vocabulary: None,
            files: None,
            editor: None,
        }
    }

    /// Factory entry — matches the manifest `factory:` string
    /// (`wylde_panel_workspaces::WorkspacesPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            // Create the Graph tab's view eagerly so it starts loading the
            // active workspace's graph in the background — switching tabs is
            // then instant.
            let graph = cx.new(|gcx| {
                let view = GraphView::new();
                GraphView::spawn_load(gcx);
                view
            });
            // The Settings tab edits the graph view's live knobs + profiles.
            let settings = cx.new(|scx| GraphSettingsTab::new(graph.clone(), scx));
            // The Vocabulary tab loads the anchor stores eagerly too.
            let vocabulary = cx.new(VocabularyTab::new);
            // IDE tabs (S2): the Files tree and the code Editor, eager-mounted
            // like the others so switching is instant and a cross-tab
            // `open_in_editor` lands on an already-live entity.
            let files = cx.new(FilesTab::new);
            let editor = cx.new(EditorTab::new);
            // The file-tree drives the editor across the tab boundary (S5): a
            // file-row click emits FileOpenEvent; we open it + flip to Editor.
            cx.subscribe(
                &files,
                |panel: &mut Self, _files, event: &crate::files::FileOpenEvent, cx| {
                    let crate::files::FileOpenEvent::Open(path) = event;
                    panel.open_in_editor(path.clone(), None, cx);
                },
            )
            .detach();
            let mut panel = Self::new();
            panel.graph = Some(graph);
            panel.settings = Some(settings);
            panel.vocabulary = Some(vocabulary);
            panel.files = Some(files);
            panel.editor = Some(editor);
            Self::spawn_refresh(cx);
            // Drain cross-panel focus deep-links (S7): a vocab word in the
            // InferenceBar (Chat panel) pushes a WorkspaceFocus; this panel
            // selects the target tab + focuses the graph node. Buffered, so a
            // focus pushed before this first mount is still delivered.
            if let Some(rx) = wylde_gui_pipe::take_workspace_focus_receiver() {
                Self::spawn_focus_drain(rx, cx);
            }
            panel
        })
        .into()
    }

    /// Open a workspace-relative `path` in the Editor tab (optionally at a
    /// 1-based `line`) and switch to it. The shared cross-tab "open this file"
    /// affordance (IDE S2/S4): the Files tab calls it on a row click, and later
    /// the graph / composer can drive it too. Scopes the read to the active
    /// workspace. No-op on test-only construction where the editor is absent.
    pub fn open_in_editor(
        &mut self,
        path: impl Into<String>,
        line: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let path = path.into();
        let workspace_id = self.active_id.clone().unwrap_or_default();
        if let Some(editor) = &self.editor {
            editor.update(cx, |e, ecx| e.open(workspace_id, path, line, ecx));
        }
        self.tab = WorkspacesTab::Editor;
        cx.notify();
    }

    /// Drain the cross-panel focus bus (S7). Each [`wylde_gui_pipe::WorkspaceFocus`]
    /// selects a tab and (optionally) focuses a graph node.
    fn spawn_focus_drain(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<wylde_gui_pipe::WorkspaceFocus>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            while let Some(focus) = rx.recv().await {
                let alive = this
                    .update(app_cx, |panel, cx| panel.apply_nav_focus(focus, cx))
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }

    /// Apply a cross-panel focus: select the target tab, then focus the graph
    /// node (retried across the graph's async load).
    pub fn apply_nav_focus(
        &mut self,
        focus: wylde_gui_pipe::WorkspaceFocus,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = focus.tab.as_deref() {
            self.tab = match tab {
                "files" => WorkspacesTab::Files,
                "editor" => WorkspacesTab::Editor,
                "graph" => WorkspacesTab::Graph,
                "registry" => WorkspacesTab::Registry,
                "vocabulary" => WorkspacesTab::Vocabulary,
                "settings" => WorkspacesTab::Settings,
                _ => self.tab,
            };
        }
        if let Some(node) = focus.node_id {
            self.spawn_focus_node(node, cx);
        }
        cx.notify();
    }

    /// Focus a graph node, retrying as the graph's async load fills in nodes
    /// (a deep-link can arrive before the graph has loaded). Gives up quietly
    /// after a few seconds.
    fn spawn_focus_node(&mut self, node: String, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.clone() else {
            return;
        };
        cx.spawn(async move |_this, app_cx: &mut AsyncApp| {
            for _ in 0..20 {
                let done = graph.update(app_cx, |gv, gcx| gv.focus_node(&node, gcx));
                if done {
                    return;
                }
                app_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
            }
        })
        .detach();
    }

    /// Reload the workspace list from the harness.  One async task; if
    /// the call fails we stash the error on the View so the user sees
    /// the pipe is broken.
    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_workspaces().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(ws) => {
                        panel.error = None;
                        // Default `active_id` to the first MRU row.
                        if panel.active_id.is_none() {
                            panel.active_id = ws.first().map(|w| w.id.clone());
                        }
                        panel.workspaces = ws;
                    }
                    Err(err) => {
                        panel.error = Some(err);
                    }
                }
                panel.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Add-workspace flow: open the OS folder picker (blocking — done
    /// on a tokio blocking task), forward the picked path to
    /// `workspaces.create`, then re-read the list.
    pub fn spawn_add(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // The native picker is blocking.  This task runs on gpui's
            // executor (no tokio reactor), so a bare `tokio::task::
            // spawn_blocking` panics ("no reactor running").  Hop onto the
            // bridge runtime's blocking pool so the gpui dispatcher doesn't
            // stall and the await just parks on the join.
            let picked: Option<PathBuf> = wylde_gui_pipe::bridged_spawn_blocking(pick_folder).await;
            let Some(path) = picked else {
                return;
            };
            let path_str = path.to_string_lossy().to_string();
            let activate_outcome = activate_workspace(&path_str, false).await;
            let _ = this.update(app_cx, |panel, _cx| {
                if let Err(e) = &activate_outcome {
                    panel.error = Some(e.clone());
                }
            });
            // Refresh whether or not activate succeeded — a failure
            // mode worth seeing in the row list.
            let ws = list_workspaces().await.unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                if !ws.is_empty() {
                    panel.workspaces = ws;
                    panel.error = None;
                }
                panel.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Per-row "Switch" handler — persist the active workspace on the
    /// harness via `workspaces.set_active` (sets the active pointer + bumps
    /// the MRU, same verb the InferenceBar dropdown uses), update the
    /// panel's "active" tag optimistically, then refresh the list so the
    /// MRU re-order is reflected.
    pub fn spawn_set_active(id: String, cx: &mut Context<Self>) {
        // Optimistic local update so the highlight moves immediately.
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let _ = this.update(app_cx, |panel, cx| {
                panel.active_id = Some(id.clone());
                // Re-root the file tree on the newly-active workspace (S5).
                if let Some(files) = &panel.files {
                    files.update(cx, |f, fcx| f.reload(fcx));
                }
                cx.notify();
            });
            let outcome = set_active_workspace(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Err(e) = outcome {
                    panel.error = Some(e);
                }
                cx.notify();
            });
            let ws = list_workspaces().await.unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                if !ws.is_empty() {
                    panel.workspaces = ws;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Per-row "Re-index" handler — drives a full rebuild and folds the
    /// verb's reply back into the row.
    ///
    /// The reply (`file_count` / `last_error`) is the *only* source for the
    /// row's index state: `workspaces.list_mru` returns the slim
    /// `WorkspaceDefinition` projection, which carries no `file_count` /
    /// `last_indexed_at` / `indexing` fields, so a post-reindex `list_mru`
    /// refresh can't surface the result — and would clobber the optimistic
    /// "Indexing…" flag the click set. Re-index never changes the MRU set,
    /// so we skip the refresh and update the clicked row in place.
    pub fn spawn_reindex(id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = reindex_workspace(&id).await;
            let _ = this.update(app_cx, |panel, cx| {
                panel.apply_reindex_outcome(&id, &outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fold a `workspaces.reindex` reply into the clicked row: clear the
    /// in-progress flag, then either surface the fresh file count + a
    /// "just now" timestamp on success, or raise the embed error.
    ///
    /// The verb replies OK at the transport layer even when the embed
    /// itself failed (e.g. the embedder is unreachable) — the real failure
    /// rides in the reply's `last_error` — so we collapse both the
    /// transport error and the in-reply `last_error` into one error here.
    fn apply_reindex_outcome(&mut self, id: &str, outcome: &Result<serde_json::Value, String>) {
        let result: Result<Option<u64>, String> = match outcome {
            Err(transport_err) => Err(transport_err.clone()),
            Ok(reply) => match reply.get("last_error").and_then(|v| v.as_str()) {
                Some(embed_err) => Err(embed_err.to_owned()),
                None => Ok(reply.get("file_count").and_then(|v| v.as_u64())),
            },
        };
        if let Some(ws) = self.workspaces.iter_mut().find(|w| w.id == id) {
            ws.indexing = false;
            if let Ok(file_count) = &result {
                ws.file_count = *file_count;
                ws.last_indexed_at = Some("just now".to_owned());
            }
        }
        match result {
            Ok(_) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    /// Per-row "Remove" handler.  The Svelte page requires a
    /// click-to-confirm pattern for this; the gpui port lands the
    /// confirmation step in a follow-on slice (the pattern needs a
    /// per-row inline-confirm element-id; cheap to add later).
    pub fn spawn_remove(id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = delete_workspace(&id).await;
            let _ = this.update(app_cx, |panel, _cx| {
                if let Err(e) = outcome {
                    panel.error = Some(e);
                }
                if panel.active_id.as_deref() == Some(&id) {
                    panel.active_id = None;
                }
            });
            let ws = list_workspaces().await.unwrap_or_default();
            let _ = this.update(app_cx, |panel, cx| {
                panel.workspaces = ws;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Default for WorkspacesPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for WorkspacesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.tab {
            WorkspacesTab::Files => match self.files.clone() {
                Some(view) => view.into_any_element(),
                // No child entity (test-only construction) — render nothing.
                None => div().into_any_element(),
            },
            WorkspacesTab::Editor => match self.editor.clone() {
                Some(view) => view.into_any_element(),
                None => div().into_any_element(),
            },
            WorkspacesTab::Graph => match self.graph.clone() {
                Some(view) => view.into_any_element(),
                // No child entity (test-only construction) — render nothing.
                None => div().into_any_element(),
            },
            WorkspacesTab::Settings => match self.settings.clone() {
                Some(view) => view.into_any_element(),
                None => div().into_any_element(),
            },
            WorkspacesTab::Vocabulary => match self.vocabulary.clone() {
                Some(view) => view.into_any_element(),
                None => div().into_any_element(),
            },
            // Registry is the default; any not-yet-wired tab also falls back
            // here (the tab bar only offers WIRED tabs, so this is the
            // Registry body).
            _ => self.registry_body(cx).into_any_element(),
        };

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .flex()
            .flex_col()
            .child(self.tab_bar(cx))
            .child(
                // The body fills the remaining height; `min_h_0` lets the
                // graph canvas size to the slot instead of overflowing.
                div().flex_1().min_h(px(0.0)).overflow_hidden().child(body),
            )
    }
}

impl WorkspacesPanel {
    /// The tab bar — one button per [`WorkspacesTab::WIRED`] tab; the active
    /// one is accented.
    fn tab_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(pack(BORDER_SUBTLE)));
        for tab in WorkspacesTab::WIRED.iter().copied() {
            bar = bar.child(tab_button(tab, self.tab == tab, cx));
        }
        bar
    }

    /// The Registry tab body (the original panel content).
    fn registry_body(&self, cx: &mut Context<Self>) -> gpui::Div {
        let header = header_row(cx);

        let mut column = div()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err, cx));
        }

        if self.loading {
            column = column.child(loading_row());
        } else if self.workspaces.is_empty() {
            column = column.child(empty_state());
        } else {
            if let Some(active_id) = &self.active_id {
                column = column.child(active_card(active_id, &self.workspaces));
            }
            for ws in &self.workspaces {
                let is_active = self.active_id.as_deref() == Some(ws.id.as_str());
                column = column.child(workspace_card(ws, is_active, cx));
            }
        }

        div().p_6().child(column)
    }
}

/// One tab-bar button. Clicking switches the active tab.
fn tab_button(
    tab: WorkspacesTab,
    is_active: bool,
    cx: &mut Context<WorkspacesPanel>,
) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name(format!("ws-tab::{}", tab.label()).into());
    let (text_color, bg) = if is_active {
        (TEXT_PRIMARY, Some(SURFACE_800))
    } else {
        (TEXT_SECONDARY, None)
    };
    let mut btn = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded(px(4.0))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(if is_active {
            weight::SEMIBOLD as f32
        } else {
            weight::REGULAR as f32
        }))
        .text_color(rgb(pack(text_color)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut WorkspacesPanel, _ev, _window, cx| {
                if this.tab != tab {
                    this.tab = tab;
                    cx.notify();
                }
            }),
        )
        .child(SharedString::from(tab.label()));
    if let Some(c) = bg {
        btn = btn.bg(rgb(pack(c)));
    }
    btn
}

fn header_row(cx: &mut Context<WorkspacesPanel>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::LG))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from("Workspaces")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Each workspace has its own RAG index. Add a project folder, \
                             switch between them, or remove ones you no longer need.",
                        )),
                ),
        )
        .child(add_button(cx))
}

fn add_button(cx: &mut Context<WorkspacesPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("workspaces-add".into());
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .bg(rgb(pack(BRAND)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut WorkspacesPanel, _event, _window, cx| {
                WorkspacesPanel::spawn_add(cx);
            }),
        )
        .child(SharedString::from("+ Add workspace"))
}

fn active_card(active_id: &str, workspaces: &[WorkspaceSummary]) -> gpui::Div {
    let path = workspaces
        .iter()
        .find(|w| w.id == active_id)
        .map(|w| w.path.clone())
        .unwrap_or_default();
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(36.0))
                .h(px(36.0))
                .rounded(px(6.0))
                .bg(rgb(pack(BRAND_DIM)))
                .flex()
                .items_center()
                .justify_center()
                .font_family(FAMILY_INTER)
                .text_size(px(size::LG))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from("F")),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from("ACTIVE WORKSPACE")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from(active_id.to_owned())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(path)),
                ),
        )
}

fn workspace_card(
    ws: &WorkspaceSummary,
    is_active: bool,
    cx: &mut Context<WorkspacesPanel>,
) -> gpui::Div {
    let border = if is_active {
        BORDER_DEFAULT
    } else {
        BORDER_SUBTLE
    };
    let title_color = if is_active {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };

    let id_for_switch = ws.id.clone();
    let id_for_reindex = ws.id.clone();
    let id_for_remove = ws.id.clone();
    let label_active = SharedString::from(ws.id.clone());
    let label_path = SharedString::from(ws.path.clone());

    let mut row = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(border)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(title_color)))
                        .font_weight(FontWeight(if is_active {
                            weight::SEMIBOLD as f32
                        } else {
                            weight::REGULAR as f32
                        }))
                        .child(label_active),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(label_path),
                )
                .child(meta_strip(ws)),
        );

    // Action buttons.
    if !is_active {
        row = row.child(action_button(
            ElementId::Name(format!("ws-switch::{}", ws.id).into()),
            "Switch",
            cx.listener(move |_this: &mut WorkspacesPanel, _ev, _window, cx| {
                WorkspacesPanel::spawn_set_active(id_for_switch.clone(), cx);
            }),
        ));
    }
    row = row.child(action_button(
        ElementId::Name(format!("ws-reindex::{}", ws.id).into()),
        if ws.indexing {
            "Indexing…"
        } else {
            "Re-index"
        },
        cx.listener(move |this: &mut WorkspacesPanel, _ev, _window, cx| {
            // Optimistic in-progress flip so the button reads "Indexing…"
            // the instant it's clicked; `spawn_reindex` clears it on reply.
            if let Some(ws) = this.workspaces.iter_mut().find(|w| w.id == id_for_reindex) {
                ws.indexing = true;
            }
            cx.notify();
            WorkspacesPanel::spawn_reindex(id_for_reindex.clone(), cx);
        }),
    ));
    row = row.child(action_button(
        ElementId::Name(format!("ws-remove::{}", ws.id).into()),
        "Remove",
        cx.listener(move |_this: &mut WorkspacesPanel, _ev, _window, cx| {
            WorkspacesPanel::spawn_remove(id_for_remove.clone(), cx);
        }),
    ));
    row
}

fn action_button<F>(id: ElementId, label: &str, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    let label_owned = SharedString::from(label.to_owned());
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(label_owned)
}

fn meta_strip(ws: &WorkspaceSummary) -> gpui::Div {
    let chunks = ws
        .file_count
        .map(|n| format!("{n} files"))
        .unwrap_or_else(|| "—".into());
    let last = ws.last_indexed_at.clone().unwrap_or_else(|| "never".into());
    div()
        .flex()
        .flex_row()
        .gap_3()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(chunks))
        .child(SharedString::from(format!("Last index: {last}")))
}

fn empty_state() -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("No workspaces yet")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Point Wylde at a project folder to give it project-aware retrieval.",
                )),
        )
}

fn loading_row() -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("Loading…"))
}

/// True when a pipe error means the `wylde-workspaces` service is
/// unreachable (down / not launched / slow), as opposed to a logical
/// application error. Drives the friendly "service unavailable" fallback
/// (scope v2 §7.5) — the panel keeps its last-known list and offers Retry.
fn is_service_unavailable(err: &str) -> bool {
    err.contains("pipe_unavailable")
        || err.contains("pipe_connect")
        || err.contains("pipe_timeout")
        || err.contains("not running")
        || err.contains("no_action")
}

/// Error banner. For a workspaces-service-unavailable error it shows the
/// graceful-degradation message + a Retry button (re-reads the list); the
/// panel preserves its last-known workspace rows underneath. Other errors
/// render verbatim.
fn error_strip(msg: &str, cx: &mut Context<WorkspacesPanel>) -> gpui::Div {
    let unavailable = is_service_unavailable(msg);
    let text = if unavailable {
        "Workspaces service unavailable — showing last-known data. \
         Start the workspaces service, then Retry."
            .to_owned()
    } else {
        msg.to_owned()
    };

    let mut strip = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(text)),
        );

    if unavailable {
        strip = strip.child(action_button(
            ElementId::Name("workspaces-retry".into()),
            "Retry",
            cx.listener(|_this: &mut WorkspacesPanel, _event, _window, cx| {
                WorkspacesPanel::spawn_refresh(cx);
            }),
        ));
    }

    strip
}

/// Pack an `Rgba` into the `u32` shape gpui's `rgb()` accepts.  Same
/// shim every panel keeps locally.
pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

/// Synchronously open the native folder picker.  Lives in a free
/// function so `spawn_blocking` can call it without capturing a
/// non-`Send` closure.
fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Select project folder")
        .pick_folder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_with_defaults_is_constructible() {
        let p = WorkspacesPanel::new();
        assert!(p.workspaces.is_empty());
        assert!(p.active_id.is_none());
        assert!(p.error.is_none());
        assert!(p.loading);
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<WorkspacesPanel>();
    }

    #[test]
    fn each_section_uses_expected_pipe_verbs() {
        // Build-time witness — same pattern Settings tests use.
        let _ = list_workspaces;
        let _ = activate_workspace;
        let _ = reindex_workspace;
        let _ = delete_workspace;
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn service_unavailable_detects_pipe_down_errors() {
        // The shapes `wylde_gui_pipe::call` returns when wylde-workspaces
        // isn't reachable → the "service unavailable + Retry" fallback.
        assert!(is_service_unavailable(
            "pipe_unavailable: service 'wylde-workspaces' is not running (pipe not found)"
        ));
        assert!(is_service_unavailable(
            "pipe_connect: wylde-workspaces: oops"
        ));
        assert!(is_service_unavailable(
            "pipe_timeout: no response from 'wylde-workspaces' within 10s"
        ));
        // A logical application error is NOT a service-unavailable fallback.
        assert!(!is_service_unavailable(
            "bad_request: workspace_id is required"
        ));
        assert!(!is_service_unavailable(
            "not_found: workspace \"x\" not found"
        ));
    }

    fn panel_with_one_indexing_row() -> WorkspacesPanel {
        let mut p = WorkspacesPanel::new();
        p.workspaces = vec![WorkspaceSummary {
            id: "ws-a".to_owned(),
            indexing: true,
            ..Default::default()
        }];
        p
    }

    #[test]
    fn reindex_success_updates_row_and_clears_state() {
        let mut p = panel_with_one_indexing_row();
        p.error = Some("stale".to_owned());
        let reply = serde_json::json!({
            "ok": true, "workspace_id": "ws-a",
            "file_count": 42, "chunk_count": 100, "last_error": null,
        });
        p.apply_reindex_outcome("ws-a", &Ok(reply));
        let row = &p.workspaces[0];
        assert!(!row.indexing, "in-progress flag must clear");
        assert_eq!(row.file_count, Some(42), "fresh count comes from the reply");
        assert_eq!(row.last_indexed_at.as_deref(), Some("just now"));
        assert!(p.error.is_none(), "a clean reindex clears the error strip");
    }

    #[test]
    fn reindex_surfaces_embed_error_from_last_error() {
        // The verb replies transport-OK even when the embed failed; the real
        // failure rides in `last_error` and must reach the error strip.
        let mut p = panel_with_one_indexing_row();
        let reply = serde_json::json!({
            "ok": false, "workspace_id": "ws-a",
            "file_count": 0, "chunk_count": 0,
            "last_error": "embedder unreachable",
        });
        p.apply_reindex_outcome("ws-a", &Ok(reply));
        assert!(!p.workspaces[0].indexing, "flag clears even on failure");
        assert_eq!(p.error.as_deref(), Some("embedder unreachable"));
        // A failed pass must not claim a fresh index.
        assert!(p.workspaces[0].last_indexed_at.is_none());
    }

    #[test]
    fn reindex_surfaces_transport_error() {
        let mut p = panel_with_one_indexing_row();
        p.apply_reindex_outcome("ws-a", &Err("pipe_unavailable: down".to_owned()));
        assert!(!p.workspaces[0].indexing);
        assert_eq!(p.error.as_deref(), Some("pipe_unavailable: down"));
    }
}
