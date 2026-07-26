//! Per-file outline side card (TBS Slice H) — the first GUI consumer of
//! `treesitter.outline`.
//!
//! Click a graph node that has a source file → the sidecar's nested symbol
//! tree renders in a card over the canvas ("per-file sidebar outline", Build
//! Order Slice H). The tree arrives as `{tree:[{kind,name,line,children}]}`
//! and is flattened to depth-tagged rows for rendering ([`flatten`] — pure,
//! tested without gpui). Esc or ✕ closes; clicking another node re-targets.
//!
//! Chrome comes from the Theme's `ui_chrome.context_menu` + breadcrumb text
//! palette (the cluster-menu precedent) — nothing visual hardcoded. OI-1:
//! an unreachable sidecar (or an unsupported language) shows as one quiet
//! error line in the card, never a dead click.

use gpui::{div, prelude::*, px, AsyncApp, Context, MouseButton, MouseDownEvent, SharedString};
use serde_json::{json, Value};

use super::GraphView;
use crate::graph::paint::to_rgba;
use wylde_gui_controls::control;

const SVC_TREESITTER: &str = "wylde-treesitter";

/// Rows past this cap are summarised as "+N more" — a generated file with
/// thousands of symbols shouldn't build thousands of gpui elements.
const MAX_ROWS: usize = 300;

/// One flattened outline row.
#[derive(Clone, Debug, PartialEq)]
pub struct OutlineRow {
    pub depth: usize,
    pub kind: String,
    pub name: Option<String>,
    pub line: u64,
}

/// The open outline card's state.
pub(crate) struct OutlineState {
    /// Source file the outline describes.
    pub file: String,
    pub rows: Vec<OutlineRow>,
    pub loading: bool,
    pub error: Option<String>,
}

/// Flatten the verb's nested `tree` into depth-tagged rows (preorder).
pub fn flatten(tree: &Value) -> Vec<OutlineRow> {
    fn walk(items: &Value, depth: usize, out: &mut Vec<OutlineRow>) {
        let Some(arr) = items.as_array() else { return };
        for item in arr {
            out.push(OutlineRow {
                depth,
                kind: item
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                line: item.get("line").and_then(Value::as_u64).unwrap_or(0),
            });
            walk(&item["children"], depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(tree, 0, &mut out);
    out
}

impl GraphView {
    /// Open (or re-target) the outline card for `file` and fetch the tree.
    pub(crate) fn open_outline(&mut self, file: String, cx: &mut Context<Self>) {
        self.outline = Some(OutlineState {
            file: file.clone(),
            rows: Vec::new(),
            loading: true,
            error: None,
        });
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = wylde_gui_pipe::call(
                SVC_TREESITTER,
                "POST",
                "/__action__",
                Some(json!({
                    "action": "treesitter.outline",
                    "payload": { "path": file },
                })),
            )
            .await;
            let _ = this.update(app_cx, |view, cx| {
                let Some(state) = view.outline.as_mut().filter(|s| s.file == file) else {
                    return; // re-targeted (or closed) while in flight
                };
                state.loading = false;
                match outcome {
                    Ok(v) => state.rows = flatten(&v["tree"]),
                    Err(e) => state.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn close_outline(&mut self, cx: &mut Context<Self>) {
        if self.outline.take().is_some() {
            cx.notify();
        }
    }

    /// The outline card element (absolute, right edge of the canvas), `None`
    /// when closed or the theme failed to load.
    pub(crate) fn outline_element(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let state = self.outline.as_ref()?;
        let theme = self.theme.as_ref()?;
        let m = &theme.ui_chrome.context_menu;
        let text = to_rgba(theme.graph_panel.breadcrumb_bar.text(self.dark));
        let file_label = std::path::Path::new(&state.file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(state.file.as_str())
            .to_owned();

        let mut card = control(div(), "graph-outline-card")
            .absolute()
            .top_8()
            .right_2()
            .w(px(260.0))
            .max_h(px(420.0))
            .overflow_y_scroll()
            .bg(to_rgba(m.background(self.dark)))
            .rounded(px(m.border_radius_px))
            .text_size(px(m.font_size_px))
            .text_color(text)
            .flex()
            .flex_col()
            .px(px(m.item_padding_px))
            .py_1();

        // Header: filename + close.
        card = card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(m.item_height_px))
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(SharedString::from(format!("Outline — {file_label}"))),
                )
                .child(
                    control(div(), "graph-outline-close")
                        .px_1()
                        .cursor_pointer()
                        .child(SharedString::from("✕"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.close_outline(cx);
                            }),
                        ),
                ),
        );

        if state.loading {
            return Some(card.child(SharedString::from("Loading outline…")));
        }
        if let Some(err) = &state.error {
            return Some(card.child(SharedString::from(format!("Outline unavailable — {err}"))));
        }
        if state.rows.is_empty() {
            return Some(card.child(SharedString::from("No symbols in this file.")));
        }
        for (i, row) in state.rows.iter().take(MAX_ROWS).enumerate() {
            let label = match &row.name {
                Some(n) => format!("{n} · {}", row.line),
                None => format!("({}) · {}", row.kind, row.line),
            };
            card = card.child(
                control(div(), ("graph-outline-row", i))
                    .pl(px(10.0 * row.depth as f32))
                    .child(SharedString::from(label)),
            );
        }
        if state.rows.len() > MAX_ROWS {
            card = card.child(SharedString::from(format!(
                "+{} more",
                state.rows.len() - MAX_ROWS
            )));
        }
        Some(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_preorders_the_tree_with_depths() {
        let tree = json!([
            {"kind": "class_definition", "name": "Widget", "line": 4, "children": [
                {"kind": "function_definition", "name": "render", "line": 5},
                {"kind": "function_definition", "name": "hide", "line": 7},
            ]},
            {"kind": "function_definition", "name": "top", "line": 10},
        ]);
        let rows = flatten(&tree);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].name.as_deref(), Some("Widget"));
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].name.as_deref(), Some("render"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[3].name.as_deref(), Some("top"));
        assert_eq!(rows[3].depth, 0);
        assert_eq!(rows[3].line, 10);
    }

    #[test]
    fn flatten_tolerates_nameless_items_and_junk() {
        let tree = json!([
            {"kind": "impl_item", "name": null, "line": 1, "children": [
                {"kind": "function_item", "name": "go", "line": 2}
            ]},
        ]);
        let rows = flatten(&tree);
        assert_eq!(rows[0].name, None);
        assert_eq!(rows[1].depth, 1);
        assert!(flatten(&json!(null)).is_empty());
        assert!(flatten(&json!([])).is_empty());
    }
}
