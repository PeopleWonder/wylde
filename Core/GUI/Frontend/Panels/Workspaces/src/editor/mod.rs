//! The Editor tab — a code editor living **inside** Workspaces (IDE OQ-8: a
//! tab only, never a top-level left-nav panel).
//!
//! **S2 (this slice) ships the tab shell + the cross-tab open contract.** The
//! real editing surface — the dedicated `wylde-gpui-code-editor` element
//! (OQ-3 = Option B) wired to `workspaces.fs.read`/`fs.write` with
//! tree-sitter highlighting — lands in S3 (the element crate) + S4 (this tab's
//! body). Until then this renders a placeholder that shows the pending open
//! request, so the Files-tab → `open_in_editor` → tab-switch wiring is live
//! and visibly testable now.

use gpui::{
    div, prelude::*, px, rgb, Context, FontWeight, IntoElement, Render, SharedString, Window,
};
use wylde_theme::colors::{BORDER_SUBTLE, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

/// A request to open a file in the editor at an optional 1-based line. The
/// shared "open this file" channel between the Files tab (and later the graph
/// / composer) and the Editor tab; [`crate::workspaces_panel::WorkspacesPanel::open_in_editor`]
/// drives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    /// Workspace-relative path (the form `workspaces.fs.read` jails + resolves).
    pub path: String,
    /// 1-based line to scroll to / place the caret on, if any.
    pub line: Option<u32>,
}

/// The Editor tab view.
///
/// S2 holds only the current open request; S4 grows this with the editor
/// element entity, dirty state, save-conflict UX, the highlight map, and the
/// binary/oversized banners.
pub struct EditorTab {
    /// The file the editor is (to be) showing, if any.
    pub current: Option<OpenRequest>,
}

impl EditorTab {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { current: None }
    }

    /// Open `path` (optionally scrolling to `line`). S2 records the request;
    /// S4 will load the bytes via `workspaces.fs.read` into the editor element.
    pub fn open(&mut self, path: String, line: Option<u32>, cx: &mut Context<Self>) {
        self.current = Some(OpenRequest { path, line });
        cx.notify();
    }
}

impl Render for EditorTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("workspaces-editor-tab")
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .font_family(FAMILY_INTER)
            .border_t_1()
            .border_color(rgb(pack(BORDER_SUBTLE)))
            .child(
                div()
                    .text_size(px(size::SM))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .child(SharedString::from("Editor")),
            );

        root = match &self.current {
            Some(req) => {
                let where_ = match req.line {
                    Some(l) => format!("{} : {}", req.path, l),
                    None => req.path.clone(),
                };
                root.child(
                    div()
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(format!("Opening {where_}"))),
                )
            }
            None => root.child(
                div()
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(
                        "Pick a file in the Files tab to open it here.",
                    )),
            ),
        };

        root.child(
            div()
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Code editor lands in S3/S4 (dedicated wylde-gpui-code-editor element).",
                )),
        )
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
