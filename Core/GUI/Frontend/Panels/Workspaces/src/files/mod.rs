//! The Files tab — a lazy file-tree of the active workspace's folder.
//!
//! **S2 (this slice) ships the tab shell only.** The real tree — lazy
//! per-directory expansion backed by `workspaces.fs.list_dir` (OQ-4), rows
//! that click through to `WorkspacesPanel::open_in_editor`, re-rooting on
//! `set_active`, and the graceful-degrade banner — lands in S5. Until then
//! this renders a placeholder so the tab is present in the locked layout and
//! the panel wiring (eager entity, render arm) is exercised.

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, IntoElement, Render, SharedString, Window};
use wylde_theme::colors::{BORDER_SUBTLE, TEXT_MUTED, TEXT_PRIMARY};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::workspaces_panel::pack;

/// The Files tab view. S2 is a placeholder; S5 grows it into the tree-state
/// machine (expanded set, per-node children cache, loading flags) + the async
/// `fs.list_dir` wiring.
pub struct FilesTab {}

impl FilesTab {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {}
    }
}

impl Render for FilesTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workspaces-files-tab")
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
                    .child(SharedString::from("Files")),
            )
            .child(
                div()
                    .text_size(px(size::MICRO))
                    .text_color(rgb(pack(TEXT_MUTED)))
                    .child(SharedString::from(
                        "Lazy workspace file-tree lands in S5 (workspaces.fs.list_dir).",
                    )),
            )
    }
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
