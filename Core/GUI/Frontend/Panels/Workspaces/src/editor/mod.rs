//! The Editor tab — the code editor living **inside** Workspaces (OQ-8: a tab
//! only). IDE S4: wires the dedicated `wylde-gpui-code-editor` element to the
//! jailed `workspaces.fs.*` verbs (S1) for load/save, and to
//! `treesitter.highlight` (live inline-source variant) for syntax colours.
//!
//! Behaviours:
//!   * **Load** (`open`) — `fs.read`; binary files are refused (null-byte
//!     heuristic, OQ-7) and oversized files open read-only with a banner;
//!     otherwise the buffer loads silently (no spurious dirty) and scrolls to
//!     the requested line.
//!   * **Save** — Ctrl/Cmd+S in the editor emits `SaveRequested`; we `fs.write`
//!     with the last-known mtime for optimistic concurrency (OQ-6). The
//!     existing watcher re-indexes the saved file; the write path never
//!     re-enqueues itself, so no save→reindex loop.
//!   * **Highlight** — debounced after load / edit / save, over the editor's
//!     in-memory buffer (so unsaved edits colour correctly).
//!   * **Graceful degrade** — service/sidecar errors surface as a banner; a
//!     missing highlighter just leaves the text plain.

pub mod highlight_map;
pub mod ipc;
pub mod lsp_decor;

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, Render, SharedString, Subscription, Window,
};
use wylde_gpui_code_editor::{CodeEditor, EditorEvent};
use wylde_theme::colors::{
    BORDER_SUBTLE, BRAND, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER, FAMILY_MONO};

use crate::workspaces_panel::pack;
use wylde_gui_controls::control;

/// Debounce before (re)highlighting after an edit.
const HIGHLIGHT_DEBOUNCE_MS: u64 = 250;
/// Warning colour for the error banner.
const ERROR_RGB: u32 = 0xE5_73_73;

/// A request to open a file at an optional 1-based line — the payload of the
/// cross-tab open channel ([`crate::workspaces_panel::WorkspacesPanel::open_in_editor`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    pub path: String,
    pub line: Option<u32>,
}

/// Load/save status surfaced as a one-line banner above the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    /// Nothing open yet.
    Empty,
    /// Reading the file.
    Loading,
    /// Editing normally.
    Ready,
    /// Just saved (cleared on the next edit).
    Saved,
    /// Binary file — refused; the editor shows nothing editable (OQ-7).
    Binary,
    /// Oversized — opened read-only with the truncated head (OQ-7).
    Oversized,
    /// The on-disk file changed since we read it (optimistic-concurrency miss).
    Conflict,
    /// Load/save failed (service down, jail breach, etc.).
    Error(String),
}

/// The Editor tab view.
pub struct EditorTab {
    /// The code-editor element. `None` only in test-only construction.
    editor: Option<Entity<CodeEditor>>,
    _sub: Option<Subscription>,
    /// Active workspace the open file is scoped to.
    workspace_id: Option<String>,
    /// Absolute path of the active workspace's folder (LSP root + file URIs).
    folder: Option<String>,
    /// Workspace-relative path of the open file.
    rel_path: Option<String>,
    /// mtime from the last read/write — optimistic-concurrency token.
    mtime: Option<f64>,
    dirty: bool,
    status: Status,
    /// Bumped on each open/save so a stale async result can't clobber a newer
    /// one (the user opened another file while a read was in flight).
    load_gen: u64,
    /// Bumped on each highlight request so a stale highlight is ignored.
    highlight_gen: u64,
    // ── LSP (S9) ─────────────────────────────────────────────────────────
    /// Tree-sitter syntax decorations (fills) — kept apart from diagnostics so
    /// either can update independently; combined before `set_decorations`.
    syntax_decos: Vec<wylde_gpui_code_editor::Decoration>,
    /// LSP diagnostic underlines (squiggles).
    diag_decos: Vec<wylde_gpui_code_editor::Decoration>,
    /// LSP document version (didChange).
    lsp_version: i64,
    /// Debounce/staleness guard for the LSP change→diagnostics cycle.
    lsp_gen: u64,
    /// Current completion items `(label, detail)` from the last Ctrl+Space.
    completions: Vec<(String, Option<String>)>,
    /// Current hover text (F1), shown in a dismissable strip.
    hover: Option<String>,
}

impl EditorTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|ecx| CodeEditor::new(ecx).with_element_key("workspaces-code-editor"));
        // React to the editor's own events: track dirty + re-highlight on
        // change; perform the save on the save chord.
        let sub = cx.subscribe(
            &editor,
            |this: &mut Self, _ed, event: &EditorEvent, cx| match event {
                EditorEvent::Changed(_) => {
                    if !matches!(this.status, Status::Binary | Status::Oversized) {
                        this.dirty = true;
                        if this.status == Status::Saved {
                            this.status = Status::Ready;
                        }
                        this.spawn_highlight(cx);
                        this.spawn_lsp_sync(cx);
                        cx.notify();
                    }
                }
                EditorEvent::SaveRequested => this.save(cx),
                EditorEvent::CompletionRequested { line, character } => {
                    this.request_completion(*line, *character, cx);
                }
                EditorEvent::HoverRequested { line, character } => {
                    this.request_hover(*line, *character, cx);
                }
            },
        );
        Self {
            editor: Some(editor),
            _sub: Some(sub),
            workspace_id: None,
            folder: None,
            rel_path: None,
            mtime: None,
            dirty: false,
            status: Status::Empty,
            load_gen: 0,
            highlight_gen: 0,
            syntax_decos: Vec::new(),
            diag_decos: Vec::new(),
            lsp_version: 1,
            lsp_gen: 0,
            completions: Vec::new(),
            hover: None,
        }
    }

    /// Open `path` (workspace-relative) in `workspace_id` rooted at the
    /// absolute `folder`, optionally scrolling to 1-based `line`. Drives the
    /// `fs.read` → buffer → highlight (+ LSP) pipeline.
    pub fn open(
        &mut self,
        workspace_id: String,
        folder: String,
        path: String,
        line: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        self.workspace_id = Some(workspace_id.clone());
        self.folder = (!folder.is_empty()).then_some(folder);
        self.rel_path = Some(path.clone());
        self.dirty = false;
        self.status = Status::Loading;
        self.load_gen += 1;
        // Reset LSP/decoration state for the new file.
        self.syntax_decos.clear();
        self.diag_decos.clear();
        self.completions.clear();
        self.hover = None;
        self.lsp_version = 1;
        let gen = self.load_gen;
        cx.notify();

        cx.spawn(async move |this, app| {
            let result = ipc::read_file(&workspace_id, &path).await;
            let _ = this.update(app, |t, cx| {
                if t.load_gen != gen {
                    return; // a newer open superseded this read
                }
                match result {
                    Ok(v) => t.apply_loaded(v, line, cx),
                    Err(e) => {
                        t.status = Status::Error(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn apply_loaded(&mut self, v: serde_json::Value, line: Option<u32>, cx: &mut Context<Self>) {
        let binary = v.get("binary").and_then(|x| x.as_bool()).unwrap_or(false);
        let truncated = v
            .get("truncated")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let content = v
            .get("content")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned();
        self.mtime = v.get("mtime").and_then(|x| x.as_f64());

        if binary {
            self.status = Status::Binary;
            if let Some(ed) = &self.editor {
                ed.update(cx, |e, ec| {
                    e.set_text_silent(String::new(), ec);
                    e.set_read_only(true, ec);
                    e.clear_decorations(ec);
                });
            }
            cx.notify();
            return;
        }

        let read_only = truncated; // oversized → read-only (OQ-7)
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, ec| {
                e.set_read_only(read_only, ec);
                e.set_text_silent(content, ec);
                if let Some(l) = line {
                    e.scroll_to_line(l as usize, ec);
                }
            });
        }
        self.dirty = false;
        self.status = if truncated {
            Status::Oversized
        } else {
            Status::Ready
        };
        cx.notify();
        self.spawn_highlight(cx);
        // Open the document in the LSP (best-effort; rust files only). The
        // content is read from the editor so it matches exactly what loaded.
        if !read_only {
            self.spawn_lsp_open(cx);
        }
    }

    /// Absolute path of the open file (`folder` + relative path), if known.
    fn abs_path(&self) -> Option<String> {
        let (folder, rel) = (self.folder.as_ref()?, self.rel_path.as_ref()?);
        Some(format!("{}/{}", folder.trim_end_matches(['/', '\\']), rel))
    }

    /// Is the open file a Rust source file? (LSP = rust-analyzer only, S8.)
    fn is_rust(&self) -> bool {
        self.rel_path
            .as_deref()
            .map(|p| p.ends_with(".rs"))
            .unwrap_or(false)
    }

    /// Set the editor's decorations to syntax fills + diagnostic underlines
    /// combined (diagnostics after, so underline-only layers over the colour).
    fn set_combined_decorations(&self, cx: &mut Context<Self>) {
        let Some(ed) = &self.editor else { return };
        let mut all = self.syntax_decos.clone();
        all.extend(self.diag_decos.clone());
        ed.update(cx, |e, ec| e.set_decorations(all, ec));
    }

    /// Open the current document in the LSP and fetch its first diagnostics.
    fn spawn_lsp_open(&mut self, cx: &mut Context<Self>) {
        if !self.is_rust() {
            return;
        }
        let (Some(folder), Some(abs), Some(ed)) =
            (self.folder.clone(), self.abs_path(), self.editor.clone())
        else {
            return;
        };
        let text = ed.read(cx).text().to_owned();
        cx.spawn(async move |this, app| {
            // Best-effort: an Err just means no LSP (degrade to plain text).
            if ipc::lsp_open(&folder, &abs, &text).await.is_err() {
                return;
            }
            Self::fetch_diagnostics(this, app, abs).await;
        })
        .detach();
    }

    /// Debounced LSP didChange + diagnostics refresh on edit.
    fn spawn_lsp_sync(&mut self, cx: &mut Context<Self>) {
        if !self.is_rust() {
            return;
        }
        let Some(abs) = self.abs_path() else {
            return;
        };
        self.lsp_version += 1;
        let version = self.lsp_version;
        self.lsp_gen += 1;
        let gen = self.lsp_gen;
        cx.spawn(async move |this, app| {
            app.background_executor()
                .timer(Duration::from_millis(HIGHLIGHT_DEBOUNCE_MS))
                .await;
            // Bail if a newer edit superseded this one.
            let stale = this.update(app, |t, _| t.lsp_gen != gen).unwrap_or(true);
            if stale {
                return;
            }
            let text = match this.update(app, |t, cx| {
                t.editor.as_ref().map(|e| e.read(cx).text().to_owned())
            }) {
                Ok(Some(t)) => t,
                _ => return,
            };
            if ipc::lsp_change(&abs, &text, version).await.is_err() {
                return;
            }
            // rust-analyzer needs a beat to recompute; then pull diagnostics.
            app.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            Self::fetch_diagnostics(this, app, abs).await;
        })
        .detach();
    }

    /// Fetch + apply diagnostics for `abs` (shared by open + sync).
    async fn fetch_diagnostics(
        this: gpui::WeakEntity<Self>,
        app: &mut gpui::AsyncApp,
        abs: String,
    ) {
        let Ok(reply) = ipc::lsp_diagnostics(&abs).await else {
            return;
        };
        let _ = this.update(app, |t, cx| {
            let text = t
                .editor
                .as_ref()
                .map(|e| e.read(cx).text().to_owned())
                .unwrap_or_default();
            t.diag_decos = lsp_decor::diagnostics_to_decorations(&text, &reply);
            t.set_combined_decorations(cx);
        });
    }

    /// Ctrl+Space → fetch completions at the position into the strip.
    fn request_completion(&mut self, line: u32, character: u32, cx: &mut Context<Self>) {
        if !self.is_rust() {
            return;
        }
        let Some(abs) = self.abs_path() else { return };
        self.hover = None;
        cx.spawn(async move |this, app| {
            let Ok(reply) = ipc::lsp_completion(&abs, line, character).await else {
                return;
            };
            let items: Vec<(String, Option<String>)> = reply
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|it| {
                            let label = it.get("label").and_then(|v| v.as_str())?.to_owned();
                            let detail =
                                it.get("detail").and_then(|v| v.as_str()).map(str::to_owned);
                            Some((label, detail))
                        })
                        .take(20)
                        .collect()
                })
                .unwrap_or_default();
            let _ = this.update(app, |t, cx| {
                t.completions = items;
                cx.notify();
            });
        })
        .detach();
    }

    /// F1 → fetch hover info at the position into the strip.
    fn request_hover(&mut self, line: u32, character: u32, cx: &mut Context<Self>) {
        if !self.is_rust() {
            return;
        }
        let Some(abs) = self.abs_path() else { return };
        self.completions.clear();
        cx.spawn(async move |this, app| {
            let Ok(reply) = ipc::lsp_hover(&abs, line, character).await else {
                return;
            };
            let contents = reply
                .get("contents")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let _ = this.update(app, |t, cx| {
                t.hover = (!contents.trim().is_empty()).then_some(contents);
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a chosen completion: insert its label at the caret.
    fn apply_completion(&mut self, label: String, cx: &mut Context<Self>) {
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, ec| e.insert_text(&label, ec));
        }
        self.completions.clear();
        cx.notify();
    }

    /// Save the buffer via `fs.write` (optimistic concurrency on mtime).
    fn save(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Binary) {
            return;
        }
        let (Some(ws), Some(path), Some(ed)) = (
            self.workspace_id.clone(),
            self.rel_path.clone(),
            self.editor.clone(),
        ) else {
            return;
        };
        if ed.read(cx).is_read_only() {
            return; // oversized/binary read-only — nothing to save
        }
        let content = ed.read(cx).text().to_owned();
        let expected = self.mtime;
        self.load_gen += 1;
        let gen = self.load_gen;

        cx.spawn(async move |this, app| {
            let result = ipc::write_file(&ws, &path, &content, expected).await;
            let _ = this.update(app, |t, cx| {
                if t.load_gen != gen {
                    return;
                }
                match result {
                    Ok(v) => {
                        t.mtime = v.get("mtime").and_then(|x| x.as_f64());
                        t.dirty = false;
                        t.status = Status::Saved;
                        t.spawn_highlight(cx);
                    }
                    // Error string is "<code>: <message>" (wylde_gui_pipe).
                    Err(e) if e.starts_with("conflict") => t.status = Status::Conflict,
                    Err(e) => t.status = Status::Error(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// (Re)highlight the editor's current buffer after a debounce, ignoring the
    /// result if a newer highlight (or a different file) supersedes it.
    fn spawn_highlight(&mut self, cx: &mut Context<Self>) {
        if self.editor.is_none() || self.rel_path.is_none() {
            return;
        }
        let Some(path) = self.rel_path.clone() else {
            return;
        };
        self.highlight_gen += 1;
        let gen = self.highlight_gen;

        cx.spawn(async move |this, app| {
            app.background_executor()
                .timer(Duration::from_millis(HIGHLIGHT_DEBOUNCE_MS))
                .await;
            // Read the freshest buffer text iff this is still the latest request.
            let content = match this.update(app, |t, cx| {
                if t.highlight_gen != gen {
                    return None;
                }
                t.editor.as_ref().map(|e| e.read(cx).text().to_owned())
            }) {
                Ok(Some(c)) => c,
                _ => return,
            };
            // `path` carries the extension; the sidecar highlights `content`
            // inline (never reads disk), so unsaved edits colour correctly. An
            // unsupported language / sidecar-down `Err` just leaves text plain.
            if let Ok(reply) = ipc::highlight(&path, &content).await {
                let decos = highlight_map::decorations_from_reply(&reply);
                let _ = this.update(app, |t, cx| {
                    if t.highlight_gen != gen {
                        return;
                    }
                    t.syntax_decos = decos;
                    t.set_combined_decorations(cx);
                });
            }
        })
        .detach();
    }

    fn banner(&self) -> Option<(String, bool)> {
        // (text, is_error)
        let name = self.rel_path.clone().unwrap_or_default();
        match &self.status {
            Status::Empty => None,
            Status::Loading => Some((format!("Opening {name}…"), false)),
            Status::Ready => {
                let dirt = if self.dirty { " · ●" } else { "" };
                Some((format!("{name}{dirt}"), false))
            }
            Status::Saved => Some((format!("{name} · saved"), false)),
            Status::Binary => Some((
                format!("{name} — binary file, not shown. Open it in an external tool."),
                true,
            )),
            Status::Oversized => Some((
                format!("{name} — file too large; opened read-only (truncated)."),
                true,
            )),
            Status::Conflict => Some((
                format!("{name} — changed on disk since you opened it. Re-open to load the new version."),
                true,
            )),
            Status::Error(e) => Some((format!("{name} — couldn't load/save: {e}"), true)),
        }
    }
}

impl Render for EditorTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            // wylde-check: control-ok: the tab root is a layout container, not a
            // click-button — the completion rows, hover-dismiss and the editor
            // body are the controls.
            .id("workspaces-editor-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(FAMILY_INTER)
            .border_t_1()
            .border_color(rgb(pack(BORDER_SUBTLE)));

        // Banner row.
        if let Some((text, is_error)) = self.banner() {
            let color = if is_error {
                rgb(ERROR_RGB)
            } else {
                rgb(pack(TEXT_SECONDARY))
            };
            root = root.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(pack(BORDER_SUBTLE)))
                    .text_size(px(size::XS))
                    .text_color(color)
                    .child(SharedString::from(text)),
            );
        } else {
            root = root.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(
                        "Pick a file in the Files tab to open it here.",
                    )),
            );
        }

        // The editor element fills the rest. Empty / binary → a placeholder.
        let show_editor = !matches!(self.status, Status::Empty | Status::Binary);
        let body = match (&self.editor, show_editor) {
            (Some(ed), true) => div()
                .flex_1()
                .min_h(px(0.0))
                .child(ed.clone())
                .into_any_element(),
            _ => div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(size::SM))
                        .font_weight(FontWeight(weight::REGULAR as f32))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(match self.status {
                            Status::Binary => "Binary file — nothing to edit.",
                            _ => "No file open.",
                        })),
                )
                .into_any_element(),
        };

        // Hover strip (F1) — dismissable.
        if let Some(hover) = self.hover.clone() {
            root = root.child(
                // wylde-check: control-ok: the hover strip is a text container,
                // not a click-button — the ✕ dismiss inside it is the control.
                div()
                    .id("editor-hover-strip")
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(pack(BORDER_SUBTLE)))
                    .bg(rgb(pack(SURFACE_800)))
                    .child(
                        div()
                            .flex_1()
                            .font_family(FAMILY_MONO)
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(hover)),
                    )
                    .child(
                        control(div(), "editor-hover-dismiss")
                            .cursor_pointer()
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(TEXT_MUTED)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev: &MouseDownEvent, _w, cx| {
                                    this.hover = None;
                                    cx.notify();
                                }),
                            )
                            .child(SharedString::from("✕")),
                    ),
            );
        }

        // Completion list (Ctrl+Space) — click a row to insert it.
        if !self.completions.is_empty() {
            let mut list = control(div(), "editor-completions")
                .flex()
                .flex_col()
                .max_h(px(160.0))
                .overflow_y_scroll()
                .border_b_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .bg(rgb(pack(SURFACE_800)));
            for (i, (label, detail)) in self.completions.clone().into_iter().enumerate() {
                let insert = label.clone();
                let mut row = control(div(), ("editor-completion", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_0p5()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(pack(BRAND))))
                    .font_family(FAMILY_MONO)
                    .text_size(px(size::XS))
                    .child(
                        div()
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(label)),
                    );
                if let Some(d) = detail {
                    row = row.child(
                        div()
                            .text_color(rgb(pack(TEXT_MUTED)))
                            .child(SharedString::from(d)),
                    );
                }
                row = row.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                        this.apply_completion(insert.clone(), cx);
                    }),
                );
                list = list.child(row);
            }
            root = root.child(list);
        }

        root.child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_round_trips() {
        let r = OpenRequest {
            path: "src/main.rs".into(),
            line: Some(42),
        };
        assert_eq!(r.path, "src/main.rs");
        assert_eq!(r.line, Some(42));
    }

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<EditorTab>();
    }

    // ── Windowed load/save flow ──────────────────────────────────────────
    //
    // Mount a real EditorTab (with its CodeEditor child) in a gpui test window
    // and drive `open` / `save` through the scripted fake backend at the
    // `wylde_gui_pipe::call` seam — asserting the buffer + status AND the
    // fs.read / fs.write verb payloads. No live wylde-workspaces stack runs.

    use gpui::TestAppContext;
    use wylde_gui_test_support::{BackendGuard, ScriptedBackend};

    fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<EditorTab> {
        cx.add_window(|_w, cx| EditorTab::new(cx))
    }

    /// Read the live editor buffer text.
    fn buffer_text(window: &gpui::WindowHandle<EditorTab>, cx: &mut TestAppContext) -> String {
        window
            .update(cx, |t, _w, cx| {
                t.editor.as_ref().unwrap().read(cx).text().to_owned()
            })
            .unwrap()
    }

    #[gpui::test]
    fn open_loads_content_and_marks_ready(cx: &mut TestAppContext) {
        let fake = ScriptedBackend::new().on(
            "workspaces.fs.read",
            serde_json::json!({
                "content": "hello = 1\n", "binary": false, "truncated": false, "mtime": 100.0,
            }),
        );
        let _guard: BackendGuard = fake.clone().install();
        let window = mount(cx);
        cx.run_until_parked();

        // Use a non-Rust file so the load path doesn't fan out into the LSP.
        window
            .update(cx, |t, _w, cx| {
                t.open(
                    "ws-a".into(),
                    "C:/code/a".into(),
                    "config.toml".into(),
                    None,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();

        window
            .update(cx, |t, _w, _cx| {
                assert_eq!(
                    t.status,
                    Status::Ready,
                    "a clean read lands the editor in Ready"
                );
                assert_eq!(
                    t.mtime,
                    Some(100.0),
                    "the read's mtime is retained for save concurrency"
                );
                assert!(!t.dirty, "a silent load must not look dirty");
            })
            .unwrap();
        assert_eq!(
            buffer_text(&window, cx),
            "hello = 1\n",
            "the file content reaches the buffer"
        );

        let read = fake
            .last_call_for("workspaces.fs.read")
            .expect("open must fs.read");
        assert_eq!(read.payload_str("workspace_id").as_deref(), Some("ws-a"));
        assert_eq!(read.payload_str("path").as_deref(), Some("config.toml"));
    }

    #[gpui::test]
    fn open_binary_file_is_refused(cx: &mut TestAppContext) {
        let fake =
            ScriptedBackend::new().on("workspaces.fs.read", serde_json::json!({ "binary": true }));
        let _guard = fake.install();
        let window = mount(cx);
        cx.run_until_parked();
        window
            .update(cx, |t, _w, cx| {
                t.open(
                    "ws-a".into(),
                    "C:/code/a".into(),
                    "logo.png".into(),
                    None,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _w, _cx| {
                assert_eq!(t.status, Status::Binary, "a binary file is refused (OQ-7)");
            })
            .unwrap();
    }

    #[gpui::test]
    fn open_oversized_file_is_read_only(cx: &mut TestAppContext) {
        let fake = ScriptedBackend::new().on(
            "workspaces.fs.read",
            serde_json::json!({ "content": "head…", "truncated": true, "mtime": 5.0 }),
        );
        let _guard = fake.install();
        let window = mount(cx);
        cx.run_until_parked();
        window
            .update(cx, |t, _w, cx| {
                t.open(
                    "ws-a".into(),
                    "C:/code/a".into(),
                    "huge.txt".into(),
                    None,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _w, cx| {
                assert_eq!(
                    t.status,
                    Status::Oversized,
                    "an oversized file opens read-only"
                );
                assert!(
                    t.editor.as_ref().unwrap().read(cx).is_read_only(),
                    "the editor is read-only for a truncated open"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn save_writes_the_buffer_with_the_expected_mtime(cx: &mut TestAppContext) {
        let fake = ScriptedBackend::new()
            .on(
                "workspaces.fs.read",
                serde_json::json!({ "content": "v1", "binary": false, "truncated": false, "mtime": 100.0 }),
            )
            .on("workspaces.fs.write", serde_json::json!({ "mtime": 200.0, "size_bytes": 2 }));
        let _guard = fake.clone().install();
        let window = mount(cx);
        cx.run_until_parked();
        window
            .update(cx, |t, _w, cx| {
                t.open(
                    "ws-a".into(),
                    "C:/code/a".into(),
                    "notes.txt".into(),
                    None,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();

        // Ctrl/Cmd+S → SaveRequested → save().
        window.update(cx, |t, _w, cx| t.save(cx)).unwrap();
        cx.run_until_parked();

        window
            .update(cx, |t, _w, _cx| {
                assert_eq!(t.status, Status::Saved, "a clean write lands in Saved");
                assert_eq!(
                    t.mtime,
                    Some(200.0),
                    "the fresh mtime from the write is adopted"
                );
                assert!(!t.dirty, "a successful save clears dirty");
            })
            .unwrap();

        let write = fake
            .last_call_for("workspaces.fs.write")
            .expect("save must fs.write");
        assert_eq!(write.payload_str("workspace_id").as_deref(), Some("ws-a"));
        assert_eq!(write.payload_str("path").as_deref(), Some("notes.txt"));
        assert_eq!(
            write.payload_str("content").as_deref(),
            Some("v1"),
            "the live buffer is written"
        );
        assert_eq!(
            write.payload.get("expected_mtime").and_then(|v| v.as_f64()),
            Some(100.0),
            "the read's mtime rides as the optimistic-concurrency token (OQ-6)"
        );
    }

    #[gpui::test]
    fn save_conflict_surfaces_as_a_conflict_status(cx: &mut TestAppContext) {
        let fake = ScriptedBackend::new()
            .on(
                "workspaces.fs.read",
                serde_json::json!({ "content": "v1", "binary": false, "truncated": false, "mtime": 100.0 }),
            )
            .on_err("workspaces.fs.write", "conflict: file changed on disk since read");
        let _guard = fake.install();
        let window = mount(cx);
        cx.run_until_parked();
        window
            .update(cx, |t, _w, cx| {
                t.open(
                    "ws-a".into(),
                    "C:/code/a".into(),
                    "notes.txt".into(),
                    None,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |t, _w, cx| t.save(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _w, _cx| {
                assert_eq!(
                    t.status,
                    Status::Conflict,
                    "an optimistic-concurrency miss surfaces as Conflict, not a generic error"
                );
            })
            .unwrap();
    }
}

#[cfg(test)]
mod control_walk {
    //! L7 **control**-walk — the Editor tab (issue #247).
    //!
    //! In-crate: the completion list and the hover strip are populated by
    //! private fields (`completions`, `hover`) that only the LSP round-trip
    //! sets, with no public seam — so the walk drives them directly. Runs in
    //! CI via the `panel-walk` alias (`cargo test -p wylde-panel-workspaces`,
    //! which runs the lib unit tests too).

    use super::EditorTab;
    use gpui::TestAppContext;
    use wylde_gui_test_support::control_walk::ControlWalk;
    use wylde_gui_test_support::ScriptedBackend;

    #[gpui::test]
    fn every_editor_control_does_something(cx: &mut TestAppContext) {
        // No backend traffic: the completion/hover frames are seeded directly.
        let fake = ScriptedBackend::new();
        let _guard = fake.clone().install();
        let window = cx.add_window(|_w, cx| EditorTab::new(cx));
        cx.run_until_parked();

        ControlWalk::new(window, &fake)
            .fingerprint(|t: &EditorTab| {
                format!(
                    "comp={} hover={} dirty={}",
                    t.completions.len(),
                    t.hover.is_some(),
                    t.dirty,
                )
            })
            .reset(|t: &mut EditorTab, _w, cx| {
                // Both overlays up in every frame: the hover strip (its dismiss
                // ✕) and the completion list (its rows insert + clear). Set
                // directly — in-crate — so each rebase re-establishes them.
                t.hover = Some("fn foo() -> Result<(), Error>".to_string());
                t.completions = vec![
                    ("foo".to_string(), Some("fn foo()".to_string())),
                    ("foobar".to_string(), None),
                ];
                cx.notify();
            })
            .sources(&[include_str!("mod.rs")])
            .run(cx)
            .assert_every_control_lives()
            .assert_covers_every_literal_id();
    }
}
