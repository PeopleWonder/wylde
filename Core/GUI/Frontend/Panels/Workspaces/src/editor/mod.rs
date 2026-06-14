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

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window,
};
use wylde_gpui_code_editor::{CodeEditor, EditorEvent};
use wylde_theme::colors::{BORDER_SUBTLE, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

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
}

impl EditorTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|ecx| {
            CodeEditor::new(ecx).with_element_key("workspaces-code-editor")
        });
        // React to the editor's own events: track dirty + re-highlight on
        // change; perform the save on the save chord.
        let sub = cx.subscribe(&editor, |this: &mut Self, _ed, event: &EditorEvent, cx| {
            match event {
                EditorEvent::Changed(_) => {
                    if !matches!(this.status, Status::Binary | Status::Oversized) {
                        this.dirty = true;
                        if this.status == Status::Saved {
                            this.status = Status::Ready;
                        }
                        this.spawn_highlight(cx);
                        cx.notify();
                    }
                }
                EditorEvent::SaveRequested => this.save(cx),
            }
        });
        Self {
            editor: Some(editor),
            _sub: Some(sub),
            workspace_id: None,
            rel_path: None,
            mtime: None,
            dirty: false,
            status: Status::Empty,
            load_gen: 0,
            highlight_gen: 0,
        }
    }

    /// Open `path` (workspace-relative) in `workspace_id`, optionally scrolling
    /// to 1-based `line`. Drives the `fs.read` → buffer → highlight pipeline.
    pub fn open(
        &mut self,
        workspace_id: String,
        path: String,
        line: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        self.workspace_id = Some(workspace_id.clone());
        self.rel_path = Some(path.clone());
        self.dirty = false;
        self.status = Status::Loading;
        self.load_gen += 1;
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
        let truncated = v.get("truncated").and_then(|x| x.as_bool()).unwrap_or(false);
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
        self.status = if truncated { Status::Oversized } else { Status::Ready };
        cx.notify();
        self.spawn_highlight(cx);
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
                    if let Some(ed) = &t.editor {
                        ed.update(cx, |e, ec| e.set_decorations(decos, ec));
                    }
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
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
            (Some(ed), true) => div().flex_1().min_h(px(0.0)).child(ed.clone()).into_any_element(),
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

        let _ = TEXT_PRIMARY; // reserved for future status accents
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
}
