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
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId,
    FontWeight, IntoElement, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_SUBTLE, BRAND, BRAND_DIM, SURFACE_800, SURFACE_900, TEXT_MUTED,
    TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    activate_workspace, delete_workspace, list_workspaces, reindex_workspace, set_active_workspace,
    WorkspaceSummary,
};

/// Root Workspaces panel.
pub struct WorkspacesPanel {
    pub workspaces: Vec<WorkspaceSummary>,
    pub active_id: Option<String>,
    pub error: Option<String>,
    pub loading: bool,
}

impl WorkspacesPanel {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            active_id: None,
            error: None,
            loading: true,
        }
    }

    /// Factory entry — matches the manifest `factory:` string
    /// (`wylde_panel_workspaces::WorkspacesPanel::view`).
    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            Self::spawn_refresh(cx);
            panel
        })
        .into()
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
            let picked: Option<PathBuf> =
                wylde_gui_pipe::bridged_spawn_blocking(pick_folder).await;
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

    /// Per-row "Re-index" handler — fires a full rebuild and refreshes.
    pub fn spawn_reindex(id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = reindex_workspace(&id).await;
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
        let header = header_row(cx);

        let mut column = div()
            .max_w(px(720.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header);

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
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

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
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
    let border = if is_active { BORDER_DEFAULT } else { BORDER_SUBTLE };
    let title_color = if is_active { TEXT_PRIMARY } else { TEXT_SECONDARY };

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
        if ws.indexing { "Indexing…" } else { "Re-index" },
        cx.listener(move |_this: &mut WorkspacesPanel, _ev, _window, cx| {
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
    let last = ws
        .last_indexed_at
        .clone()
        .unwrap_or_else(|| "never".into());
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

fn error_strip(msg: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
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
}
