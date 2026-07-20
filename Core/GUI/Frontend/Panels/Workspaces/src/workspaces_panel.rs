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
    div, prelude::*, px, relative, rgb, AnyElement, AnyView, App, AppContext, AsyncApp, Context,
    ElementId, Entity, FontWeight, IntoElement, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, DANGER, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY, WARNING,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

/// Lifecycle service name for the workspaces backend — the target of the
/// in-panel Start/Restart affordance (decision 7).
const WORKSPACES_SERVICE: &str = "wylde-workspaces";

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
    /// Panel navigation state (UX rework). `None` ⇒ the **Registry** landing
    /// view (the recency list of workspace cards — the home you land on).
    /// `Some(id)` ⇒ you've **entered** that workspace, so the in-workspace
    /// view is shown: a back arrow + the scoped tab bar (Files/Editor/Graph/
    /// Vocabulary/Settings) + body. Entering a card sets this (and activates
    /// the workspace); the back arrow clears it.
    pub entered: Option<String>,
    /// The selected tab *within* the entered workspace. Only meaningful while
    /// [`Self::entered`] is `Some`; ignored in the Registry view.
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
    /// The docked InferenceBar (UX rework decision 6): a thin view over the
    /// SHARED `ChatPanel` singleton's bar, mounted at the bottom of the
    /// in-workspace view so chat is grounded in the workspace and the
    /// conversation is shared with the main Chat panel. `None` only in
    /// test-only construction (no gpui context to build the child view).
    pub dock: Option<AnyView>,
    /// In-flight "download a missing embedding model" affordance, set when a
    /// re-index fails with a `model not installed` error. Drives the inline
    /// "Download <model>" button + progress so the user never drops to a
    /// terminal. `None` when the last index error (if any) isn't a
    /// missing-model error.
    pub pull: Option<ModelPull>,
}

/// An offer to download a model surfaced from a failed re-index, plus its
/// in-flight progress. Built from the index error via
/// [`wylde_gui_pipe::parse_pullable_model`] so the model name is never
/// hardcoded.
pub struct ModelPull {
    /// Model to pull, e.g. `"nomic-embed-text"`.
    pub model: String,
    /// Workspace to re-index automatically once the model is installed, so
    /// the user isn't left to re-trigger the action that failed.
    pub retry_id: String,
    pub phase: PullPhase,
}

/// Lifecycle of the inline model download.
pub enum PullPhase {
    /// Button shown; the pull hasn't started.
    Offered,
    /// Streaming `ollama.pull`; carries the running aggregate (overall
    /// percent across layers) that the progress bar renders.
    Downloading(wylde_gui_pipe::PullAggregate),
    /// The pull failed; carries the message. The button re-offers a retry.
    Failed(String),
}

impl WorkspacesPanel {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active_id: None,
            error: None,
            loading: true,
            entered: None,
            tab: WorkspacesTab::Registry,
            graph: None,
            settings: None,
            vocabulary: None,
            files: None,
            editor: None,
            dock: None,
            pull: None,
        }
    }

    /// Factory entry — matches the manifest `factory:` string
    /// (`wylde_panel_workspaces::WorkspacesPanel::view`).
    pub fn view(window: &mut Window, cx: &mut App) -> AnyView {
        // Build the docked InferenceBar over the SHARED ChatPanel singleton
        // (decision 6), using the App context before we descend into the
        // panel's own Context. This also creates + wires the Chat singleton on
        // first use if no Chat surface has mounted yet.
        let dock = wylde_panel_chat::InferenceBarDock::view(window, cx);
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
            panel.dock = Some(dock);
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

    /// Enter a workspace (UX rework): the Registry-card click handler. Marks
    /// the workspace active (same `set_active` path "Switch" used, so the rest
    /// of the system follows the MRU), flips the panel into the in-workspace
    /// view landing on the Files tab, and re-roots the file tree. The back
    /// arrow ([`Self::leave_workspace`]) returns to the Registry.
    pub fn enter_workspace(&mut self, id: String, cx: &mut Context<Self>) {
        self.entered = Some(id.clone());
        self.tab = WorkspacesTab::Files;
        Self::spawn_set_active(id.clone(), cx);
        // C3 (the A1 live-bug fix): re-scope the docked chat to this workspace.
        // Before C3, `enter_workspace` only marked the workspace active in the
        // service — nothing told the InferenceBar dock, so every dock turn rode
        // `None` and ran on base (unbound) context. Publishing here on the
        // cross-panel workspace-scope bus is the consumer half the C3-bus file
        // was built for: the docked `ChatPanel` drains it and adopts this
        // `workspace_id` (`apply_workspace_scope`), so the next dock turn
        // carries the workspace's context.
        wylde_gui_pipe::publish_active_workspace(Some(id));
        // 2.5: a fresh workspace has no file open until one is clicked; clear any
        // file left over from a previous workspace so it can't bias this one.
        wylde_gui_pipe::publish_active_file(None);
        cx.notify();
    }

    /// Leave the entered workspace and return to the Registry landing view
    /// (the back arrow). Keeps `active_id` — leaving the IDE view doesn't
    /// deactivate the workspace, it just stops scoping the panel to it.
    pub fn leave_workspace(&mut self, cx: &mut Context<Self>) {
        self.entered = None;
        // C3: leaving a workspace clears the docked chat's scope back to
        // unbound (the dock applies `None`), the mirror of `enter_workspace`'s
        // publish above.
        wylde_gui_pipe::publish_active_workspace(None);
        // 2.5: no workspace entered → no active file.
        wylde_gui_pipe::publish_active_file(None);
        cx.notify();
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
        // The active workspace's absolute folder — the LSP root + file-URI base.
        let folder = self
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .map(|w| w.path.clone())
            .unwrap_or_default();
        if let Some(editor) = &self.editor {
            editor.update(cx, |e, ecx| {
                e.open(workspace_id, folder, path.clone(), line, ecx)
            });
        }
        // 2.5 (active-file boost): publish the now-open file so a turn sent on
        // the docked chat biases RAG toward it. Workspace-relative path, exactly
        // what the index stores; cleared on enter/leave so it never crosses
        // workspaces.
        wylde_gui_pipe::publish_active_file(Some(path));
        // Editor is an in-workspace tab; ensure we're entered (a graph/composer
        // deep-link could open a file from the Registry landing).
        if self.entered.is_none() {
            self.entered = self.active_id.clone();
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
            // "registry" is the home, not an in-workspace tab — route it to the
            // back arrow (leave the workspace).
            if tab == "registry" {
                self.entered = None;
            } else if let Some(t) = WorkspacesTab::from_focus_key(tab) {
                // A deep-link into a scoped tab (e.g. composer "view in graph")
                // must first ENTER a workspace, since tabs only exist
                // in-workspace. Adopt the active workspace if we're still on
                // the Registry landing.
                if self.entered.is_none() {
                    self.entered = self.active_id.clone();
                }
                self.tab = t;
            }
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

    /// Drive a one-click service-control affordance (decision 7): start or
    /// restart the `wylde-workspaces` service via the reusable lifecycle
    /// helpers, then re-read the list so a now-reachable / now-current service
    /// repopulates the panel without a manual Retry. `restart` picks the
    /// `service.restart` verb (out-of-date recovery); otherwise `service.start`
    /// (down recovery). A control failure replaces the banner with its error.
    pub fn spawn_service_control(restart: bool, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = if restart {
                wylde_gui_pipe::restart_service(WORKSPACES_SERVICE).await
            } else {
                wylde_gui_pipe::start_service(WORKSPACES_SERVICE).await
            };
            if let Err(e) = outcome {
                let _ = this.update(app_cx, |panel, cx| {
                    panel.error = Some(e);
                    cx.notify();
                });
                return;
            }
            // Control succeeded — re-read the list (clears the banner on success).
            let ws = list_workspaces().await;
            let _ = this.update(app_cx, |panel, cx| {
                match ws {
                    Ok(ws) => {
                        panel.error = None;
                        panel.workspaces = ws;
                    }
                    Err(e) => panel.error = Some(e),
                }
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
        // Shared completion flag: the reindex task sets it when the (long) verb
        // returns so the progress poller stops. The poller keeps the card's
        // live progress fresh meanwhile — `workspaces.reindex` is one blocking
        // call that carries no intermediate frames, so without this the bar
        // would never move until the very end.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self::spawn_progress_poller(id.clone(), done.clone(), cx);
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = reindex_workspace(&id).await;
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = this.update(app_cx, |panel, cx| {
                panel.apply_reindex_outcome(&id, &outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// Poll `list_mru` every ~600ms while a reindex runs and fold the in-flight
    /// workspace's live progress snapshot into its row, so the card shows a
    /// moving bar + ETA. The backend service handles `list_mru` on its own
    /// connection task (concurrent with the embed loop, which yields on every
    /// paced batch), so the poll returns the freshly-written `RagState`
    /// progress promptly. Stops on the `done` flag (set when the reindex verb
    /// returns) or after a generous safety cap.
    fn spawn_progress_poller(
        id: String,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        use std::sync::atomic::Ordering;
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // ~10 min ceiling (1000 × 600ms) — a backstop in case the done flag
            // is never observed; the reindex deadline is itself 300s.
            for _ in 0..1000 {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                app_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(600))
                    .await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(rows) = list_workspaces().await else {
                    continue;
                };
                let fresh = rows.into_iter().find(|w| w.id == id).map(|w| w.progress);
                let _ = this.update(app_cx, |panel, cx| {
                    // The reindex task may have completed between the poll and
                    // this update — don't resurrect the cleared state.
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(progress) = fresh {
                        if let Some(w) = panel.workspaces.iter_mut().find(|w| w.id == id) {
                            // Keep the optimistic in-progress flag; adopt the
                            // live snapshot (and surface its file total).
                            w.indexing = true;
                            if let Some(p) = &progress {
                                w.file_count = Some(p.files_total.max(w.file_count.unwrap_or(0)));
                            }
                            w.progress = progress;
                            cx.notify();
                        }
                    }
                });
            }
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
            // The pass is over — drop the live progress snapshot so the card
            // reverts to its static file-count / last-indexed strip.
            ws.progress = None;
            if let Ok(file_count) = &result {
                ws.file_count = *file_count;
                ws.last_indexed_at = Some("just now".to_owned());
            }
        }
        match result {
            Ok(_) => {
                self.error = None;
                self.pull = None;
            }
            Err(e) => {
                // If the failure is a missing-embedding-model error, offer an
                // inline "Download <model>" affordance (and remember which
                // workspace to re-index once it lands) instead of leaving the
                // user to run `ollama pull` in a terminal. Don't clobber a
                // download already in flight for the same model.
                let already_pulling = matches!(
                    self.pull.as_ref(),
                    Some(p) if matches!(p.phase, PullPhase::Downloading(_))
                );
                if !already_pulling {
                    self.pull = wylde_gui_pipe::parse_pullable_model(&e).map(|model| ModelPull {
                        model,
                        retry_id: id.to_owned(),
                        phase: PullPhase::Offered,
                    });
                }
                self.error = Some(e);
            }
        }
    }

    /// Start (or retry) the inline model download for the current
    /// [`ModelPull`] offer. Streams `ollama.pull`, updating the phase with
    /// each progress frame; on success it clears the offer and
    /// automatically re-triggers the re-index that failed, so the user
    /// isn't left to re-run it by hand. Mirrors the Models panel's pull
    /// loop, with auto-retry instead of an installed-list refresh.
    pub fn start_model_pull(&mut self, cx: &mut Context<Self>) {
        let Some(pull) = self.pull.as_ref() else {
            return;
        };
        if matches!(pull.phase, PullPhase::Downloading(_)) {
            return; // already in flight
        }
        let model = pull.model.clone();
        let retry_id = pull.retry_id.clone();

        let mut stream = match wylde_gui_pipe::pull_model(&model) {
            Ok(s) => s,
            Err(e) => {
                if let Some(p) = self.pull.as_mut() {
                    p.phase = PullPhase::Failed(format!("download start: {e}"));
                }
                cx.notify();
                return;
            }
        };
        if let Some(p) = self.pull.as_mut() {
            p.phase = PullPhase::Downloading(wylde_gui_pipe::PullAggregate::default());
        }
        cx.notify();

        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                match stream.recv().await {
                    Some(Ok(v)) => {
                        let progress = wylde_gui_pipe::PullProgress::from_value(&v);
                        let done = progress.is_success();
                        let _ = this.update(app_cx, |panel, cx| {
                            if let Some(p) = panel.pull.as_mut() {
                                // Fold the frame into the running aggregate so
                                // the bar tracks overall percent, not per-layer.
                                if let PullPhase::Downloading(agg) = &mut p.phase {
                                    agg.update(&progress);
                                } else {
                                    let mut agg = wylde_gui_pipe::PullAggregate::default();
                                    agg.update(&progress);
                                    p.phase = PullPhase::Downloading(agg);
                                }
                            }
                            cx.notify();
                        });
                        if done {
                            let _ = this.update(app_cx, |panel, cx| {
                                panel.pull = None;
                                panel.error = None;
                                cx.notify();
                                // Auto-retry the index now that the model exists.
                                WorkspacesPanel::spawn_reindex(retry_id.clone(), cx);
                            });
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = this.update(app_cx, |panel, cx| {
                            if let Some(p) = panel.pull.as_mut() {
                                p.phase = PullPhase::Failed(format!("download '{model}': {e}"));
                            }
                            cx.notify();
                        });
                        return;
                    }
                    None => {
                        let _ = this.update(app_cx, |panel, cx| {
                            if let Some(p) = panel.pull.as_mut() {
                                p.phase = PullPhase::Failed(format!(
                                    "download '{model}': stream ended unexpectedly"
                                ));
                            }
                            cx.notify();
                        });
                        return;
                    }
                }
            }
        })
        .detach();
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
        // UX rework state machine: the Registry landing (a list of workspace
        // cards you land on) ⇄ the in-workspace view (back arrow + scoped tab
        // bar + body), keyed off `entered`.
        let root = div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .flex()
            .flex_col();
        match &self.entered {
            None => root.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.registry_body(cx)),
            ),
            Some(_) => {
                let body: AnyElement = match self.tab {
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
                    // Files is the in-workspace landing; any non-IDE tab also
                    // falls back here.
                    _ => match self.files.clone() {
                        Some(view) => view.into_any_element(),
                        None => div().into_any_element(),
                    },
                };
                let mut col = root.child(self.in_workspace_bar(cx)).child(
                    // The body fills the remaining height; `min_h_0` lets the
                    // graph canvas size to the slot instead of overflowing.
                    div().flex_1().min_h(px(0.0)).overflow_hidden().child(body),
                );
                // Dock the shared InferenceBar at the bottom of the in-workspace
                // view (decision 6) — chat grounded in the workspace, sharing
                // the conversation with the main Chat panel. Fixed height below
                // the flex_1 body; only shown in-workspace, never on the Registry.
                if let Some(dock) = &self.dock {
                    col = col.child(dock.clone());
                }
                col
            }
        }
    }
}

impl WorkspacesPanel {
    /// The in-workspace top bar: a back arrow returning to the Registry, the
    /// entered workspace's name, then the scoped tab bar (one button per
    /// [`WorkspacesTab::WIRED`] tab; the active one accented).
    fn in_workspace_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let name = self.entered.clone().unwrap_or_default();
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(pack(BORDER_SUBTLE)))
            // The back arrow — returns to the Registry landing.
            .child(back_button(cx))
            .child(
                div()
                    // Clip a long workspace id rather than pushing the tabs off.
                    .max_w(px(220.0))
                    .overflow_hidden()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(name)),
            )
            // A thin separator between the workspace identity and its tabs.
            .child(
                div()
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(rgb(pack(BORDER_SUBTLE)))
                    .mx_1(),
            );
        for tab in WorkspacesTab::WIRED.iter().copied() {
            bar = bar.child(tab_button(tab, self.tab == tab, cx));
        }
        // A flexible spacer pushes the readiness chip to the right edge of the
        // tab bar, so state is legible from whichever tab is selected.
        bar = bar.child(div().flex_1());
        bar.child(readiness_chip(self.readiness()))
    }

    /// Readiness of the entered workspace (decision 5), folding the last
    /// service error with that workspace's index state.
    fn readiness(&self) -> Readiness {
        let ws = self
            .entered
            .as_ref()
            .and_then(|id| self.workspaces.iter().find(|w| &w.id == id));
        Readiness::compute(self.error.as_deref(), ws)
    }

    /// The Registry landing body: a single uniform, recency-ordered list of
    /// clickable workspace cards (UX rework decision 3 — the separate "ACTIVE
    /// WORKSPACE" hero card is gone, no duplication). `list_mru` already
    /// returns MRU order, so the topmost row (index 0) is the most recent and
    /// is labelled as such.
    fn registry_body(&self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
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

        if let Some(pull) = &self.pull {
            column = column.child(download_strip(pull, cx));
        }

        if self.loading {
            column = column.child(loading_row());
        } else if self.workspaces.is_empty() {
            column = column.child(empty_state());
        } else {
            for (i, ws) in self.workspaces.iter().enumerate() {
                let is_active = self.active_id.as_deref() == Some(ws.id.as_str());
                // Topmost MRU row is the most recent.
                column = column.child(workspace_card(ws, is_active, i == 0, cx));
            }
        }

        // The list scrolls within the panel slot; a long MRU window shouldn't
        // push the docked surfaces off-screen.
        div()
            .id(ElementId::Name("ws-registry-scroll".into()))
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(column)
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

/// The back arrow at the top-left of the in-workspace view — returns to the
/// Registry landing (clears `entered`).
fn back_button(cx: &mut Context<WorkspacesPanel>) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name("ws-back".into()))
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut WorkspacesPanel, _ev, _window, cx| {
                this.leave_workspace(cx);
            }),
        )
        // "← Workspaces" reads as "back to the workspace list".
        .child(SharedString::from("← Workspaces"))
}

/// The readiness chip (decision 5): a small coloured dot + short label in the
/// in-workspace tab bar. Conveys service-up/indexed state at a glance from any
/// tab — itself meaningful status, per the maintainer's no-decoration principle.
fn readiness_chip(readiness: Readiness) -> gpui::Div {
    let (colour, label) = readiness.chip();
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .child(
            div()
                .w(px(7.0))
                .h(px(7.0))
                .rounded(px(999.0))
                .bg(rgb(pack(colour))),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(label)),
        )
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

fn workspace_card(
    ws: &WorkspaceSummary,
    is_active: bool,
    is_most_recent: bool,
    cx: &mut Context<WorkspacesPanel>,
) -> Stateful<gpui::Div> {
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

    let id_for_enter = ws.id.clone();
    let id_for_reindex = ws.id.clone();
    let id_for_remove = ws.id.clone();
    let label_active = SharedString::from(ws.id.clone());
    let label_path = SharedString::from(ws.path.clone());

    // The identity column: an optional "MOST RECENT" caption on the topmost
    // (MRU-head) card, the workspace id, its path, then the index-status strip.
    let mut identity = div()
        .flex_1()
        // Shrink below content so a long path doesn't push the action
        // buttons out of the card.
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1();
    if is_most_recent {
        identity = identity.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BRAND)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("MOST RECENT")),
        );
    }

    // The whole card is clickable → ENTER the workspace (UX rework decision 2).
    let mut row = div()
        .id(ElementId::Name(format!("ws-card::{}", ws.id).into()))
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(border)))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .cursor_pointer()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this: &mut WorkspacesPanel, _ev, _window, cx| {
                this.enter_workspace(id_for_enter.clone(), cx);
            }),
        )
        .child(
            identity
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
                        // Clip a long no-whitespace path at the card edge.
                        .overflow_hidden()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(label_path),
                )
                .child(meta_strip(ws)),
        );

    // Action buttons. (The old per-row "Switch" is gone — clicking the card
    // ENTERS the workspace, which activates it, so a separate Switch was
    // redundant.) Each button calls `stop_propagation` so it acts without
    // also entering the card underneath it.
    // Button label tracks live progress: a determinate pass shows the percent
    // ("Indexing 46%"), the indeterminate walk shows "Indexing…", idle shows
    // "Re-index".
    let reindex_label = if ws.indexing {
        ws.progress
            .as_ref()
            .and_then(|p| p.percent())
            .map(|pct| format!("Indexing {pct}%"))
            .unwrap_or_else(|| "Indexing…".to_owned())
    } else {
        "Re-index".to_owned()
    };
    row = row.child(action_button(
        ElementId::Name(format!("ws-reindex::{}", ws.id).into()),
        &reindex_label,
        cx.listener(move |this: &mut WorkspacesPanel, _ev, _window, cx| {
            cx.stop_propagation();
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
            cx.stop_propagation();
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
        // Never let the button shrink/wrap when it shares a row with a long
        // wrapping label — it keeps its intrinsic size; the text takes the rest.
        .flex_shrink_0()
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

/// The per-card index-status strip (decision 4): file count · last-indexed,
/// plus a live "Indexing…" pill while a re-index is in flight. The fields ride
/// `list_mru` (F4 joined RagState into it), so this survives a reload instead
/// of reverting to "never". Every token here is real status — no decoration.
fn meta_strip(ws: &WorkspaceSummary) -> gpui::Div {
    // While a re-index runs, the strip becomes a live progress affordance
    // (status line + bar + percent + "X / Y files" + ETA) instead of the static
    // file-count / last-indexed row.
    if ws.indexing {
        return index_progress_block(ws);
    }
    let chunks = ws
        .file_count
        .map(|n| format!("{n} files"))
        .unwrap_or_else(|| "—".into());
    let last = ws.last_indexed_at.clone().unwrap_or_else(|| "never".into());
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from(chunks))
        .child(SharedString::from(format!("Last index: {last}")))
}

/// Live re-index progress for a card: a status line ("Embedding · 46% · 120 /
/// 287 files · ~2m 30s remaining") above a thin progress bar. Before the total
/// is known (the walk phase) it shows an indeterminate state — a "Scanning
/// files…" label and a static partial-fill bar — then switches to the
/// determinate bar + ETA once counting is done. Matches the model-download
/// bar's track/fill styling (`SURFACE_900` track, `BRAND` fill, `relative`
/// width) so it reads as the same family.
fn index_progress_block(ws: &WorkspaceSummary) -> gpui::Div {
    let progress = ws.progress.as_ref();
    // Compose the status line + the bar fill ratio from the snapshot.
    let (status, ratio): (String, Option<f64>) = match progress {
        Some(p) => match p.percent() {
            Some(pct) => {
                // Determinate: label · percent · files · ETA.
                let mut parts = vec![format!("{} · {pct}%", p.phase_label())];
                if p.files_total > 0 {
                    parts.push(format!(
                        "{} / {} files",
                        p.files_done.min(p.files_total),
                        p.files_total
                    ));
                }
                if let Some(eta) = p.eta_label() {
                    parts.push(eta);
                }
                (parts.join("  ·  "), p.ratio())
            }
            // Indeterminate (walk/chunk): no total yet, so no percent/ETA.
            None => (format!("{}…", p.phase_label()), None),
        },
        // Indexing flag set but no snapshot yet (the optimistic click instant).
        None => ("Indexing…".to_owned(), None),
    };

    let bar_fill = match ratio {
        // Determinate fill: brand bar to the exact ratio.
        Some(r) => div()
            .h(px(4.0))
            .w(relative(r.clamp(0.0, 1.0) as f32))
            .bg(rgb(pack(BRAND)))
            .rounded(px(2.0)),
        // Indeterminate: a static dim partial fill (gpui's static render has no
        // marquee), signalling "working, total unknown" without a fake percent.
        None => div()
            .h(px(4.0))
            .w(relative(0.4))
            .bg(rgb(pack(BRAND_DIM)))
            .rounded(px(2.0)),
    };

    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(status)),
        )
        .child(
            div()
                .w_full()
                .h(px(4.0))
                .bg(rgb(pack(SURFACE_900)))
                .rounded(px(2.0))
                .overflow_hidden()
                .child(bar_fill),
        )
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

/// How a `wylde-workspaces` error should be presented (F2). A `pipe_*`
/// transport failure means the service is genuinely *down* (→ "Start it"); a
/// `no_action` means the service is *up* but its binary predates the verb (→
/// "Update/Restart it") — conflating the two told users to start a running
/// service. Anything else is a plain logical error, shown verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceErrorKind {
    Down,
    OutOfDate,
    Logical,
}

fn classify_service_error(err: &str) -> ServiceErrorKind {
    if err.contains("pipe_unavailable")
        || err.contains("pipe_connect")
        || err.contains("pipe_timeout")
        || err.contains("pipe_io")
        || err.contains("not running")
    {
        ServiceErrorKind::Down
    } else if err.contains("no_action") {
        ServiceErrorKind::OutOfDate
    } else {
        ServiceErrorKind::Logical
    }
}

/// Workspace **readiness** (UX rework decision 5) — the single state the
/// in-workspace tab bar's chip surfaces, so the panel reads legibly from any
/// tab. Two signals only: is the service reachable & current, and is the
/// workspace indexed. Per decision 8 the graph node count is deliberately NOT
/// part of readiness — it's a metric shown inside the Graph view, not a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// Service up + workspace indexed — green.
    Ready,
    /// A re-index is in flight — amber.
    Indexing,
    /// Service up but the workspace has never been indexed — amber.
    NotIndexed,
    /// The service pipe is down — red.
    ServiceDown,
    /// The service is up but its binary predates the verb (`no_action`) — red.
    OutOfDate,
}

impl Readiness {
    /// Derive readiness from the last service error (if any) and the entered
    /// workspace's index state. A service problem dominates (you can't trust
    /// index state from a down/stale service); otherwise indexing → fresh →
    /// never. A *logical* error (bad_request etc.) is not a readiness-red — it
    /// doesn't mean the service is down — so it falls through to index state.
    fn compute(error: Option<&str>, ws: Option<&WorkspaceSummary>) -> Readiness {
        if let Some(e) = error {
            match classify_service_error(e) {
                ServiceErrorKind::Down => return Readiness::ServiceDown,
                ServiceErrorKind::OutOfDate => return Readiness::OutOfDate,
                ServiceErrorKind::Logical => {}
            }
        }
        match ws {
            Some(w) if w.indexing => Readiness::Indexing,
            Some(w) if w.file_count.unwrap_or(0) > 0 || w.last_indexed_at.is_some() => {
                Readiness::Ready
            }
            // A workspace with no index yet, or no summary loaded — "not indexed".
            _ => Readiness::NotIndexed,
        }
    }

    /// (dot colour, short label) for the chip. Colours are theme tokens: BRAND
    /// = ready, WARNING (amber) = work pending, DANGER (red) = service problem.
    fn chip(self) -> (gpui::Rgba, &'static str) {
        match self {
            Readiness::Ready => (BRAND, "Ready"),
            Readiness::Indexing => (WARNING, "Indexing"),
            Readiness::NotIndexed => (WARNING, "Not indexed"),
            Readiness::ServiceDown => (DANGER, "Service down"),
            Readiness::OutOfDate => (DANGER, "Out of date"),
        }
    }
}

/// Error banner. For a recoverable service error (down / out of date) it shows
/// the graceful-degradation message + a Retry button (re-reads the list); the
/// panel preserves its last-known workspace rows underneath. Other errors
/// render verbatim.
fn error_strip(msg: &str, cx: &mut Context<WorkspacesPanel>) -> gpui::Div {
    let kind = classify_service_error(msg);
    let unavailable = kind != ServiceErrorKind::Logical;
    let text = match kind {
        ServiceErrorKind::Down => {
            "Workspaces service isn't running — showing last-known data.".to_owned()
        }
        ServiceErrorKind::OutOfDate => {
            "Workspaces service is out of date — this feature isn't in your build.".to_owned()
        }
        ServiceErrorKind::Logical => msg.to_owned(),
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
                // flex_1 + min_w_0 lets the text shrink below its content
                // width so a long error (e.g. the missing-model hint) WRAPS
                // inside the box instead of running past the right edge.
                .flex_1()
                .min_w_0()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(text)),
        );

    if unavailable {
        // One-click recovery (decision 7): a down service offers Start; an
        // out-of-date service offers Restart (picks up the rebuilt binary).
        // This replaces the old passive "go start it yourself" sentence.
        match kind {
            ServiceErrorKind::Down => {
                strip = strip.child(action_button(
                    ElementId::Name("workspaces-start-service".into()),
                    "Start service",
                    cx.listener(|_this: &mut WorkspacesPanel, _event, _window, cx| {
                        WorkspacesPanel::spawn_service_control(false, cx);
                    }),
                ));
            }
            ServiceErrorKind::OutOfDate => {
                strip = strip.child(action_button(
                    ElementId::Name("workspaces-restart-service".into()),
                    "Restart service",
                    cx.listener(|_this: &mut WorkspacesPanel, _event, _window, cx| {
                        WorkspacesPanel::spawn_service_control(true, cx);
                    }),
                ));
            }
            ServiceErrorKind::Logical => {}
        }
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

/// Inline "Download <model>" affordance shown beneath the error strip when
/// a re-index failed because an embedding model isn't installed. Offers a
/// one-click pull and, on success, auto-re-indexes — so the user never has
/// to drop to `ollama pull` in a terminal.
///
/// While the pull runs this renders a real, live **progress bar** (filled
/// to the overall percent across all layers) with status text like
/// `pulling 42%`, right here on the panel where Download was clicked — not
/// a spinner. On finish the strip is cleared (success) or shows the error +
/// a "Retry download" button.
fn download_strip(pull: &ModelPull, cx: &mut Context<WorkspacesPanel>) -> gpui::Div {
    let model = pull.model.clone();

    let mut strip = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_2();

    match &pull.phase {
        PullPhase::Downloading(agg) => {
            // Header line: model + overall percent / status.
            strip = strip.child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from(format!(
                        "Downloading '{model}' — {}",
                        agg.label()
                    ))),
            );
            // The actual progress bar: a full-width track with a brand-filled
            // inner whose width is the overall ratio. `relative(ratio)` keeps
            // it responsive to the panel width; clipped to the track so it
            // never overshoots while a frame is mid-update.
            let ratio = agg.overall_ratio().unwrap_or(0.0);
            strip = strip.child(
                div()
                    .w_full()
                    .h(px(6.0))
                    .bg(rgb(pack(SURFACE_900)))
                    .rounded(px(3.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .h(px(6.0))
                            .w(relative(ratio.clamp(0.0, 1.0)))
                            .bg(rgb(pack(BRAND)))
                            .rounded(px(3.0)),
                    ),
            );
        }
        PullPhase::Offered | PullPhase::Failed(_) => {
            let (text, label) = match &pull.phase {
                PullPhase::Offered => (
                    format!("Embedding model '{model}' isn't installed."),
                    "Download model",
                ),
                PullPhase::Failed(msg) => (
                    format!("Download of '{model}' failed: {msg}"),
                    "Retry download",
                ),
                PullPhase::Downloading(_) => unreachable!(),
            };
            strip = strip.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            // Wrap a long failure message inside the box
                            // instead of overflowing past the button/edge.
                            .flex_1()
                            .min_w_0()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(text)),
                    )
                    .child(action_button(
                        ElementId::Name("workspaces-download-model".into()),
                        label,
                        cx.listener(|this: &mut WorkspacesPanel, _event, _window, cx| {
                            this.start_model_pull(cx);
                        }),
                    )),
            );
        }
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
    fn new_lands_on_the_registry_not_in_a_workspace() {
        // UX rework: the panel lands on the Registry home (`entered == None`);
        // the in-workspace tab view only appears after a card is entered.
        let p = WorkspacesPanel::new();
        assert!(
            p.entered.is_none(),
            "fresh panel must land on the Registry, not inside a workspace"
        );
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
        for e in [
            "pipe_unavailable: service 'wylde-workspaces' is not running (pipe not found)",
            "pipe_connect: wylde-workspaces: oops",
            "pipe_timeout: no response from 'wylde-workspaces' within 10s",
        ] {
            assert_eq!(classify_service_error(e), ServiceErrorKind::Down, "{e}");
        }
        // A logical application error is NOT a service-unavailable fallback.
        assert_eq!(
            classify_service_error("bad_request: workspace_id is required"),
            ServiceErrorKind::Logical
        );
        assert_eq!(
            classify_service_error("not_found: workspace \"x\" not found"),
            ServiceErrorKind::Logical
        );
    }

    #[test]
    fn no_action_classifies_as_out_of_date_not_down() {
        // F2: a running service that lacks the verb is OUT OF DATE, not down —
        // so the banner says "update/restart", not "start", the running service.
        let e = "no_action: unknown action workspaces.graph";
        assert_eq!(classify_service_error(e), ServiceErrorKind::OutOfDate);
    }

    #[test]
    fn readiness_service_problems_dominate_index_state() {
        // A down/stale service is red regardless of (untrustworthy) index state.
        let indexed = WorkspaceSummary {
            file_count: Some(10),
            last_indexed_at: Some("2m ago".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            Readiness::compute(Some("pipe_unavailable: down"), Some(&indexed)),
            Readiness::ServiceDown
        );
        assert_eq!(
            Readiness::compute(Some("no_action: unknown action"), Some(&indexed)),
            Readiness::OutOfDate
        );
        // A logical error is NOT readiness-red — it doesn't mean the service is
        // down — so index state still decides.
        assert_eq!(
            Readiness::compute(Some("bad_request: id required"), Some(&indexed)),
            Readiness::Ready
        );
    }

    #[test]
    fn readiness_reflects_index_state_when_service_ok() {
        let indexing = WorkspaceSummary {
            indexing: true,
            ..Default::default()
        };
        assert_eq!(
            Readiness::compute(None, Some(&indexing)),
            Readiness::Indexing
        );

        let fresh = WorkspaceSummary {
            file_count: Some(3),
            ..Default::default()
        };
        assert_eq!(Readiness::compute(None, Some(&fresh)), Readiness::Ready);

        let never = WorkspaceSummary::default();
        assert_eq!(
            Readiness::compute(None, Some(&never)),
            Readiness::NotIndexed
        );
        // No summary loaded yet ⇒ not indexed.
        assert_eq!(Readiness::compute(None, None), Readiness::NotIndexed);
    }

    #[test]
    fn readiness_chip_colours_map_to_theme_tokens() {
        // Ready is brand; pending is amber; service problems are danger-red.
        assert_eq!(Readiness::Ready.chip().0, BRAND);
        assert_eq!(Readiness::Indexing.chip().0, WARNING);
        assert_eq!(Readiness::NotIndexed.chip().0, WARNING);
        assert_eq!(Readiness::ServiceDown.chip().0, DANGER);
        assert_eq!(Readiness::OutOfDate.chip().0, DANGER);
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

    #[test]
    fn reindex_missing_model_offers_download_with_retry_id() {
        // The real error the maintainer hit: the embed step fails because the model
        // isn't installed. The panel must offer an inline download tied to
        // the workspace that failed (for auto-retry).
        let mut p = panel_with_one_indexing_row();
        let reply = serde_json::json!({
            "ok": false, "workspace_id": "ws-a",
            "last_error": "backend has no model named \"nomic-embed-text\" — \
                pull it with: ollama pull nomic-embed-text \
                (model \"nomic-embed-text\" not installed in Ollama)",
        });
        p.apply_reindex_outcome("ws-a", &Ok(reply));
        let pull = p
            .pull
            .as_ref()
            .expect("a missing-model error must offer a download");
        assert_eq!(pull.model, "nomic-embed-text");
        assert_eq!(
            pull.retry_id, "ws-a",
            "auto-retry must target the failed workspace"
        );
        assert!(matches!(pull.phase, PullPhase::Offered));
        // The raw error still shows in the strip above the offer.
        assert!(p.error.as_deref().unwrap().contains("nomic-embed-text"));
    }

    #[test]
    fn reindex_non_model_error_offers_no_download() {
        let mut p = panel_with_one_indexing_row();
        p.apply_reindex_outcome("ws-a", &Err("ollama_unreachable: upstream down".to_owned()));
        assert!(
            p.pull.is_none(),
            "non-model errors must not offer a download"
        );
    }

    #[test]
    fn successful_reindex_clears_a_prior_download_offer() {
        let mut p = panel_with_one_indexing_row();
        p.pull = Some(ModelPull {
            model: "nomic-embed-text".to_owned(),
            retry_id: "ws-a".to_owned(),
            phase: PullPhase::Offered,
        });
        let reply = serde_json::json!({ "ok": true, "file_count": 7, "last_error": null });
        p.apply_reindex_outcome("ws-a", &Ok(reply));
        assert!(
            p.pull.is_none(),
            "a clean reindex retires the download offer"
        );
        assert!(p.error.is_none());
    }

    // ── Windowed readiness-chip tests ────────────────────────────────────
    //
    // The pure `Readiness::compute` cases above test the state machine in
    // isolation. These mount a real `WorkspacesPanel` in a gpui test window,
    // load a scripted MRU list, ENTER a workspace, and assert the chip the
    // in-workspace tab bar renders — proving the `readiness()` *selection*
    // wiring (it picks the entered workspace out of the live list and folds
    // the current service error), not just `compute` in a vacuum.

    use gpui::TestAppContext;
    use wylde_gui_test_support::{BackendGuard, ScriptedBackend};

    /// Mount + first-load the panel against a scripted list (mirrors the
    /// factory's `view()` kick of `spawn_refresh`). Returns the window AND the
    /// install guard — keep the guard alive for the body of the test so the
    /// fake backend stays installed (dropping it clears the thread-local).
    fn mount_with_rows(
        cx: &mut TestAppContext,
        rows: serde_json::Value,
    ) -> (gpui::WindowHandle<WorkspacesPanel>, BackendGuard) {
        let fake = ScriptedBackend::new().on("workspaces.list_mru", rows);
        let guard = fake.install();
        let window = cx.add_window(|_w, cx| {
            let p = WorkspacesPanel::new();
            WorkspacesPanel::spawn_refresh(cx);
            p
        });
        cx.run_until_parked();
        (window, guard)
    }

    #[gpui::test]
    fn readiness_chip_tracks_the_entered_workspace(cx: &mut TestAppContext) {
        let (window, _guard) = mount_with_rows(
            cx,
            serde_json::json!({ "workspaces": [
                {"id":"ws-indexed","folder":"C:/i","file_count":12,"last_indexed_at":1.0,"indexing":false},
                {"id":"ws-fresh","folder":"C:/f","file_count":0,"last_indexed_at":0.0,"indexing":false},
            ]}),
        );

        // Enter the indexed workspace → Ready (green).
        window
            .update(cx, |p, _w, cx| {
                p.enter_workspace("ws-indexed".to_owned(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |p, _w, _cx| {
                assert_eq!(
                    p.readiness(),
                    Readiness::Ready,
                    "an indexed entered workspace reads Ready"
                );
            })
            .unwrap();

        // Switch to the never-indexed one → NotIndexed (amber). The chip
        // follows the entered workspace, not a fixed row.
        window
            .update(cx, |p, _w, cx| p.enter_workspace("ws-fresh".to_owned(), cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |p, _w, _cx| {
                assert_eq!(
                    p.readiness(),
                    Readiness::NotIndexed,
                    "a never-indexed entered workspace reads Not indexed"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn readiness_chip_goes_red_on_a_service_error(cx: &mut TestAppContext) {
        let (window, _guard) = mount_with_rows(
            cx,
            serde_json::json!({ "workspaces": [
                {"id":"ws-a","folder":"C:/a","file_count":9,"last_indexed_at":1.0,"indexing":false},
            ]}),
        );
        window
            .update(cx, |p, _w, cx| p.enter_workspace("ws-a".to_owned(), cx))
            .unwrap();
        cx.run_until_parked();

        // A live pipe-down error dominates the (now untrustworthy) index state.
        window
            .update(cx, |p, _w, _cx| {
                p.error = Some("pipe_unavailable: service not running".to_owned());
                assert_eq!(p.readiness(), Readiness::ServiceDown, "pipe down ⇒ red");
            })
            .unwrap();

        // An out-of-date (no_action) service is red too, but distinctly.
        window
            .update(cx, |p, _w, _cx| {
                p.error = Some("no_action: unknown action workspaces.x".to_owned());
                assert_eq!(
                    p.readiness(),
                    Readiness::OutOfDate,
                    "no_action ⇒ out of date"
                );
            })
            .unwrap();
    }
}
