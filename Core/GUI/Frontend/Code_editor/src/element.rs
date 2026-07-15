//! The custom gpui [`Element`] that renders the code editor.
//!
//! Built fresh each frame by [`crate::CodeEditor::render`]; reads buffer /
//! scroll / decorations / config from the entity. Unlike `wylde-gpui-input`
//! (which grows to content height and leans on a parent's
//! `overflow_y_scroll`), this element **fills its slot and owns its own
//! vertical scroll with viewport culling** — it shapes only the logical lines
//! visible in the viewport, the large-file headroom OQ-3 = Option B was chosen
//! for. It paints a line-number gutter, the visible text with decoration runs,
//! the selection, and the caret, then publishes a [`LayoutSnapshot`] for the
//! metrics API + mouse hit-testing.

use gpui::{
    fill, point, px, relative, size, App, AvailableSpace, Bounds, Element, ElementId,
    GlobalElementId, Hsla, IntoElement, LayoutId, PaintQuad, Pixels, SharedString, Style,
    TextAlign, TextRun, TextStyle, WrappedLine,
};
use wylde_theme::colors::{BORDER_EMPHASIS, SURFACE_950, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY};

use crate::decoration::{build_line_runs, decorations_for_line};
use crate::metrics::{LayoutSnapshot, VisibleLine};
use crate::CodeEditor;

/// Caret quad width.
pub(crate) const CARET_WIDTH: f32 = 2.0;
/// Horizontal padding inside the gutter, each side.
const GUTTER_PAD: f32 = 8.0;
/// Padding between gutter and text.
const TEXT_PAD_LEFT: f32 = 8.0;
/// Extra rows shaped beyond the strict viewport (smoother wheel scroll).
const OVERSCAN_ROWS: usize = 1;

pub(crate) struct EditorElement {
    pub(crate) editor: gpui::Entity<CodeEditor>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct EditorPrepaint {
    snapshot: LayoutSnapshot,
    /// Background quads painted behind glyphs (selection, current-line).
    backgrounds: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
    gutter_bg: Option<PaintQuad>,
    /// Shaped line-number glyphs + their window-absolute origin.
    gutter_numbers: Vec<(gpui::Point<Pixels>, WrappedLine)>,
    line_height: Pixels,
}

fn definite(space: AvailableSpace) -> Option<Pixels> {
    match space {
        AvailableSpace::Definite(p) => Some(p),
        _ => None,
    }
}

/// Decimal digit count of `n` (≥ 1).
fn digits(n: usize) -> usize {
    let mut n = n.max(1);
    let mut d = 0;
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

/// Shape a single short line of `text` in one colour. Returns the first
/// (only) wrapped line, or `None` for empty text.
fn shape_solid(
    text: &str,
    color: Hsla,
    style: &TextStyle,
    font_size: Pixels,
    window: &mut gpui::Window,
) -> Option<WrappedLine> {
    if text.is_empty() {
        return None;
    }
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_owned()),
            font_size,
            &[run],
            None,
            None,
        )
        .ok()
        .and_then(|v| v.into_iter().next())
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Fill the slot in both axes — the element scrolls internally rather
        // than growing to content height.
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        let layout_id = window.request_measured_layout(style, move |known, available, _w, _cx| {
            let w = known.width.or(definite(available.width)).unwrap_or(px(0.0));
            let h = known
                .height
                .or(definite(available.height))
                .unwrap_or(px(0.0));
            size(w, h)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        window: &mut gpui::Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style.line_height_in_pixels(window.rem_size());

        let editor = self.editor.read(cx);
        let focused = editor.focus_handle.is_focused(window);
        let show_gutter = editor.show_gutter;
        let text = editor.buffer.text().to_owned();
        let line_count = editor.buffer.line_count();
        let cursor = editor.buffer.cursor();
        let selection = editor.buffer.selection();
        let decorations = editor.decorations.clone();

        // Clamp scroll to content extents now that we know the viewport.
        let total_height = line_height * line_count as f32;
        let max_scroll_top = (total_height - bounds.size.height).max(px(0.0));
        let scroll_top = editor.scroll_top.clamp(px(0.0), max_scroll_top);
        let scroll_left = editor.scroll_left.max(px(0.0));

        // Gutter width from the widest line number.
        let gutter_width = if show_gutter {
            let dc = digits(line_count);
            // Approximate digit advance via a shaped sample (monospace → exact).
            let sample = "0".repeat(dc);
            let w = shape_solid(&sample, TEXT_MUTED.into(), &style, font_size, window)
                .map(|l| l.width())
                .unwrap_or(font_size * (dc as f32 * 0.6));
            w + px(GUTTER_PAD * 2.0)
        } else {
            px(0.0)
        };

        let text_bounds = Bounds::new(
            point(
                bounds.origin.x + gutter_width + px(TEXT_PAD_LEFT),
                bounds.origin.y,
            ),
            size(
                (bounds.size.width - gutter_width - px(TEXT_PAD_LEFT)).max(px(0.0)),
                bounds.size.height,
            ),
        );

        // Visible line range (with a little overscan).
        let first_line = (scroll_top / line_height).floor() as usize;
        let visible_rows = (bounds.size.height / line_height).ceil() as usize + 1 + OVERSCAN_ROWS;
        let first_line = first_line.saturating_sub(OVERSCAN_ROWS);
        let last_line =
            (first_line + visible_rows + OVERSCAN_ROWS).min(line_count.saturating_sub(1));

        // Precompute logical line byte ranges once.
        let line_spans = line_byte_spans(&text);

        let mut visible: Vec<VisibleLine> = Vec::new();
        let mut gutter_numbers: Vec<(gpui::Point<Pixels>, WrappedLine)> = Vec::new();
        for line_ix in first_line..=last_line {
            let (ls, le) = line_spans
                .get(line_ix)
                .copied()
                .unwrap_or((text.len(), text.len()));
            let line_text = &text[ls..le];
            let line_y = text_bounds.origin.y + line_height * line_ix as f32 - scroll_top;

            // Text glyphs for this line.
            let local_decos = decorations_for_line(&decorations, ls, le);
            let runs = build_line_runs(line_text, &style, &local_decos);
            let shaped = shape_line_runs(line_text, font_size, &runs, window);
            if let Some(shaped) = shaped {
                visible.push(VisibleLine {
                    line_ix,
                    start_offset: ls,
                    byte_len: le - ls,
                    shaped,
                });
            } else {
                // Empty line — still register it (zero-width) so the caret and
                // hit-testing resolve on blank lines.
                if let Some(empty) = shape_line_runs(" ", font_size, &[blank_run(&style)], window) {
                    // Use a real shaped space but record byte_len 0 so columns
                    // beyond the (empty) content clamp to the line start.
                    visible.push(VisibleLine {
                        line_ix,
                        start_offset: ls,
                        byte_len: 0,
                        shaped: empty,
                    });
                }
            }

            // Gutter number.
            if show_gutter {
                let num = (line_ix + 1).to_string();
                let color: Hsla = if cursor >= ls && cursor <= le {
                    TEXT_SECONDARY.into() // current line accent
                } else {
                    TEXT_MUTED.into()
                };
                if let Some(g) = shape_solid(&num, color, &style, font_size, window) {
                    // Right-align inside the gutter.
                    let x = bounds.origin.x + gutter_width - px(GUTTER_PAD) - g.width();
                    gutter_numbers.push((point(x, line_y), g));
                }
            }
        }

        let snapshot = LayoutSnapshot {
            lines: visible,
            text_bounds,
            line_height,
            scroll_top,
            scroll_left,
        };

        // Selection + current-line backgrounds (behind glyphs).
        let mut backgrounds = Vec::new();
        if let Some((s, e)) = selection {
            for rect in snapshot.rects_for_range(s..e) {
                backgrounds.push(fill(rect, BORDER_EMPHASIS));
            }
        }

        // Caret when focused.
        let caret = if focused {
            snapshot
                .caret_rect(cursor, CARET_WIDTH)
                .map(|r| fill(r, TEXT_PRIMARY))
        } else {
            None
        };

        let gutter_bg = if show_gutter {
            Some(fill(
                Bounds::new(bounds.origin, size(gutter_width, bounds.size.height)),
                SURFACE_950,
            ))
        } else {
            None
        };

        // Persist the clamped scroll + the painted viewport so the view's
        // wheel handler, page-motion, and scroll-to-caret all agree.
        self.editor.update(cx, |ed, _| {
            ed.scroll_top = scroll_top;
            ed.scroll_left = scroll_left;
            ed.note_viewport(bounds.size.height, line_height);
        });

        EditorPrepaint {
            snapshot,
            backgrounds,
            caret,
            gutter_bg,
            gutter_numbers,
            line_height,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        // Selection / current-line backgrounds first.
        for quad in prepaint.backgrounds.drain(..) {
            window.paint_quad(quad);
        }

        // Text glyphs.
        for line in &prepaint.snapshot.lines {
            let origin = point(
                prepaint.snapshot.text_bounds.origin.x - prepaint.snapshot.scroll_left,
                prepaint.snapshot.text_bounds.origin.y + prepaint.line_height * line.line_ix as f32
                    - prepaint.snapshot.scroll_top,
            );
            let _ = line.shaped.paint(
                origin,
                prepaint.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        // Caret above text.
        if let Some(caret) = prepaint.caret.take() {
            window.paint_quad(caret);
        }

        // Gutter on top (covers any horizontally-scrolled glyphs).
        if let Some(bg) = prepaint.gutter_bg.take() {
            window.paint_quad(bg);
        }
        for (origin, num) in &prepaint.gutter_numbers {
            let _ = num.paint(
                *origin,
                prepaint.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        // Publish the painted-frame snapshot for metrics + hit-testing.
        let placeholder = LayoutSnapshot {
            lines: Vec::new(),
            text_bounds: prepaint.snapshot.text_bounds,
            line_height: prepaint.line_height,
            scroll_top: prepaint.snapshot.scroll_top,
            scroll_left: prepaint.snapshot.scroll_left,
        };
        let snapshot = std::mem::replace(&mut prepaint.snapshot, placeholder);
        self.editor.update(cx, |ed, _| {
            ed.last_layout = Some(snapshot);
        });
    }
}

fn blank_run(style: &TextStyle) -> TextRun {
    TextRun {
        len: 1,
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Shape a line's `runs` (already summing to `text.len()`). `None` for empty.
fn shape_line_runs(
    text: &str,
    font_size: Pixels,
    runs: &[TextRun],
    window: &mut gpui::Window,
) -> Option<WrappedLine> {
    if text.is_empty() {
        return None;
    }
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_owned()),
            font_size,
            runs,
            None,
            None,
        )
        .ok()
        .and_then(|v| v.into_iter().next())
}

/// `(start, end)` byte spans of each logical line (newline excluded). Always
/// at least one span; a trailing newline yields a final empty span.
pub(crate) fn line_byte_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    out.push((start, text.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_counts_decimal_places() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(999), 3);
        assert_eq!(digits(1000), 4);
    }

    #[test]
    fn line_byte_spans_split_with_trailing_empty() {
        assert_eq!(line_byte_spans(""), vec![(0, 0)]);
        assert_eq!(line_byte_spans("abc"), vec![(0, 3)]);
        assert_eq!(line_byte_spans("ab\ncd"), vec![(0, 2), (3, 5)]);
        assert_eq!(line_byte_spans("ab\n"), vec![(0, 2), (3, 3)]);
    }
}
