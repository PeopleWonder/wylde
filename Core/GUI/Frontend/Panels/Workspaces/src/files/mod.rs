//! The Files tab — a lazy file-tree of the active workspace's folder (IDE S5).
//!
//! Backed by `workspaces.fs.list_dir` (jailed, S1): children are fetched on
//! first expand (OQ-4), cached per directory, and the tree re-roots when the
//! active workspace changes. File rows emit [`FileOpenEvent`]; the panel
//! subscribes and drives `open_in_editor` (so the editor opens in its tab).
//! Ignored entries (`.git`, `target`, dotfiles, binaries) are shown but dimmed
//! (OQ-7). Service-down surfaces a banner + Retry, preserving the last tree.

pub mod ipc;

use std::collections::{HashMap, HashSet};

use gpui::{
    div, prelude::*, px, rgb, Context, EventEmitter, FontWeight, IntoElement, MouseButton,
    MouseDownEvent, Render, SharedString, Window,
};
use wylde_theme::colors::{BORDER_SUBTLE, BRAND, SURFACE_800, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;
use ipc::{Entry, Kind};

/// Per-depth indent (px).
const INDENT: f32 = 14.0;

/// Emitted when the user clicks a file row — the panel opens it in the editor.
#[derive(Clone, Debug)]
pub enum FileOpenEvent {
    Open(String),
}

/// One flattened, renderable tree row.
struct Row {
    entry: Entry,
    depth: usize,
    expanded: bool,
    loading: bool,
}

/// The Files tab view.
pub struct FilesTab {
    workspace_id: Option<String>,
    /// Listed children keyed by directory rel-path (`""` = root).
    children: HashMap<String, Vec<Entry>>,
    expanded: HashSet<String>,
    loading: HashSet<String>,
    error: Option<String>,
    /// True once the root listing has come back at least once.
    loaded_root: bool,
}

impl FilesTab {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut tab = Self {
            workspace_id: None,
            children: HashMap::new(),
            expanded: HashSet::new(),
            loading: HashSet::new(),
            error: None,
            loaded_root: false,
        };
        tab.reload(cx);
        tab
    }

    /// Re-discover the active workspace and (re)load its root listing. Called
    /// on mount, on the Retry/Refresh button, and by the panel after a
    /// workspace switch.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.children.clear();
        self.expanded.clear();
        self.loading.clear();
        self.error = None;
        self.loaded_root = false;
        cx.notify();

        cx.spawn(async move |this, app| {
            let ws = ipc::active_workspace_id().await;
            let ws_id = match ws {
                Ok(Some(id)) => id,
                Ok(None) => {
                    let _ = this.update(app, |t, cx| {
                        t.workspace_id = None;
                        t.loaded_root = true;
                        cx.notify();
                    });
                    return;
                }
                Err(e) => {
                    let _ = this.update(app, |t, cx| {
                        t.error = Some(e);
                        cx.notify();
                    });
                    return;
                }
            };
            let listing = ipc::list_dir(&ws_id, "").await;
            let _ = this.update(app, |t, cx| {
                t.workspace_id = Some(ws_id);
                t.loaded_root = true;
                match listing {
                    Ok(entries) => {
                        t.children.insert(String::new(), entries);
                        t.error = None;
                    }
                    Err(e) => t.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Expand/collapse a directory; fetch its children on first expand.
    fn toggle_dir(&mut self, rel: String, cx: &mut Context<Self>) {
        if self.expanded.contains(&rel) {
            self.expanded.remove(&rel);
            cx.notify();
            return;
        }
        self.expanded.insert(rel.clone());
        if !self.children.contains_key(&rel) {
            self.fetch_dir(rel, cx);
        }
        cx.notify();
    }

    fn fetch_dir(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_id.clone() else {
            return;
        };
        self.loading.insert(rel.clone());
        cx.spawn(async move |this, app| {
            let listing = ipc::list_dir(&ws, &rel).await;
            let _ = this.update(app, |t, cx| {
                t.loading.remove(&rel);
                match listing {
                    Ok(entries) => {
                        t.children.insert(rel.clone(), entries);
                        t.error = None;
                    }
                    Err(e) => t.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// DFS-flatten the visible tree into renderable rows.
    fn flatten(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.flatten_into("", 0, &mut out);
        out
    }

    fn flatten_into(&self, dir: &str, depth: usize, out: &mut Vec<Row>) {
        let Some(entries) = self.children.get(dir) else {
            return;
        };
        for entry in entries {
            let is_dir = entry.kind == Kind::Dir;
            let expanded = is_dir && self.expanded.contains(&entry.rel_path);
            out.push(Row {
                entry: entry.clone(),
                depth,
                expanded,
                loading: self.loading.contains(&entry.rel_path),
            });
            if expanded {
                self.flatten_into(&entry.rel_path, depth + 1, out);
            }
        }
    }
}

impl EventEmitter<FileOpenEvent> for FilesTab {}

impl Render for FilesTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("workspaces-files-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(FAMILY_INTER)
            .border_t_1()
            .border_color(rgb(pack(BORDER_SUBTLE)));

        // Header: title + Refresh.
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
                .child(
                    div()
                        .text_size(px(size::SM))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .child(SharedString::from("Files")),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("files-refresh")
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.0))
                        .bg(rgb(pack(SURFACE_800)))
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .cursor_pointer()
                        .child(SharedString::from("Refresh"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, _w, cx| {
                                this.reload(cx);
                            }),
                        ),
                ),
        );

        // Body.
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(size::XS))
                            .text_color(rgb(0xE5_73_73))
                            .child(SharedString::from(format!(
                                "Couldn't list files — {err}"
                            ))),
                    )
                    .child(
                        div()
                            .id("files-retry")
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.0))
                            .bg(rgb(pack(BRAND)))
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .cursor_pointer()
                            .child(SharedString::from("Retry"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev: &MouseDownEvent, _w, cx| {
                                    this.reload(cx);
                                }),
                            ),
                    ),
            );
            return root;
        }

        if self.workspace_id.is_none() && self.loaded_root {
            return root.child(
                div()
                    .p_3()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(
                        "No active workspace. Add or switch to one in the Registry tab.",
                    )),
            );
        }

        if !self.loaded_root {
            return root.child(
                div()
                    .p_3()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from("Loading…")),
            );
        }

        let rows = self.flatten();
        let mut list = div()
            .id("files-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .py_1();
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from("(empty)")),
            );
        }
        for (i, row) in rows.into_iter().enumerate() {
            list = list.child(file_row(i, row, cx));
        }
        root.child(list)
    }
}

/// One tree row: indented, with a folder caret or a file dot, dimmed when
/// ignored. Dirs toggle; files emit an open event.
fn file_row(i: usize, row: Row, cx: &mut Context<FilesTab>) -> impl IntoElement {
    let is_dir = row.entry.kind == Kind::Dir;
    let glyph = if is_dir {
        if row.loading {
            "…"
        } else if row.expanded {
            "▾"
        } else {
            "▸"
        }
    } else if row.entry.kind == Kind::Symlink {
        "↳"
    } else {
        "·"
    };
    let color = if row.entry.ignored {
        TEXT_MUTED
    } else if is_dir {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };
    let rel = row.entry.rel_path.clone();
    let indent = px(8.0 + INDENT * row.depth as f32);

    div()
        .id(("file-row", i))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .pl(indent)
        .pr_2()
        .py_0p5()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(pack(SURFACE_800))))
        .text_size(px(size::XS))
        .text_color(rgb(pack(color)))
        .child(
            div()
                .w(px(12.0))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(glyph)),
        )
        .child(SharedString::from(row.entry.name.clone()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev: &MouseDownEvent, _w, cx| {
                if is_dir {
                    this.toggle_dir(rel.clone(), cx);
                } else {
                    cx.emit(FileOpenEvent::Open(rel.clone()));
                }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<FilesTab>();
    }
}
