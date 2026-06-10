//! The shaped-text element — TextInput's render core (glyph-metrics slice).
//!
//! Replaces the old div-per-line/span-per-run rendering with ONE custom
//! [`Element`] that shapes the buffer through gpui's text system
//! (`window.text_system().shape_text(...)`) and paints the resulting
//! [`WrappedLine`]s directly — the same machinery gpui's own editor uses.
//! That makes the renderer the **single source of truth for glyph
//! geometry**, which buys, at once:
//!
//!   * true soft-wrap with an inline caret (the old span-row limitation),
//!   * pixel-exact selection highlights across wrapped rows,
//!   * click-to-position / drag-to-select (point → byte index),
//!   * the glyph-metrics API ([`crate::TextInput::rects_for_range`] /
//!     `index_at_point`) the composer underline and the bubble layer
//!     anchor to,
//!   * styled highlight spans ([`crate::HighlightSpan`]) painted as
//!     decoration runs — gpui draws (wavy) underlines itself, wrap-aware,
//!     so an IDE-style squiggle needs no custom painting.
//!
//! After paint, the element publishes a [`LayoutInfo`] snapshot (shaped
//! lines + window-absolute bounds) back onto the entity; the metrics API
//! reads that snapshot. It describes the LAST painted frame — callers
//! treat it as best-effort-stale by one frame, which is exactly the
//! contract mouse handlers and tether painting need.

use std::ops::Range;

use gpui::{
    fill, point, px, relative, size, App, AvailableSpace, Bounds, Element, ElementId, Entity,
    GlobalElementId, IntoElement, LayoutId, PaintQuad, Pixels, Point, SharedString, Style,
    TextAlign, TextRun, TextStyle, UnderlineStyle, Window, WrappedLine,
};
use wylde_theme::colors::{BORDER_EMPHASIS, TEXT_MUTED, TEXT_PRIMARY};

use crate::{HighlightSpan, TextInput};

/// The caret quad's width.
const CARET_WIDTH: f32 = 2.0;

/// The post-paint layout snapshot the metrics API reads.
pub struct LayoutInfo {
    /// One shaped (possibly soft-wrapped) line per logical line.
    pub(crate) lines: Vec<WrappedLine>,
    /// Byte offset where each logical line starts (same length as `lines`).
    pub(crate) line_starts: Vec<usize>,
    /// Window-absolute bounds of the painted text area.
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) line_height: Pixels,
    /// True when the painted text was the placeholder (empty buffer) — the
    /// metrics then describe placeholder glyphs, so range queries answer
    /// `None`/empty instead of lying about user text.
    pub(crate) placeholder: bool,
}

impl LayoutInfo {
    /// Byte length of the text this layout describes.
    fn len(&self) -> usize {
        let (Some(start), Some(line)) = (self.line_starts.last(), self.lines.last()) else {
            return 0;
        };
        start + line.len()
    }

    /// Which logical line holds byte `offset` (clamped to the last line).
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        }
    }

    /// Y offset (relative to the text origin) where logical line `ix` begins.
    fn line_y(&self, ix: usize) -> Pixels {
        let mut y = px(0.0);
        for line in self.lines.iter().take(ix) {
            y += line.size(self.line_height).height;
        }
        y
    }

    /// Window-absolute caret rect for byte `offset`.
    pub(crate) fn caret_rect(&self, offset: usize) -> Option<Bounds<Pixels>> {
        let ix = self.line_of(offset);
        let line = self.lines.get(ix)?;
        let local = offset - self.line_starts[ix];
        let pos = line.position_for_index(local, self.line_height)?;
        Some(Bounds::new(
            point(
                self.bounds.origin.x + pos.x,
                self.bounds.origin.y + self.line_y(ix) + pos.y,
            ),
            size(px(CARET_WIDTH), self.line_height),
        ))
    }

    /// Window-absolute rects covering byte `range` — one rect per visual
    /// row touched. Intermediate fully-covered rows extend to the line's
    /// laid-out width.
    pub(crate) fn rects_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut out = Vec::new();
        if range.start >= range.end || self.placeholder {
            return out;
        }
        for (ix, line) in self.lines.iter().enumerate() {
            let line_start = self.line_starts[ix];
            let line_end = line_start + line.len();
            let s = range.start.max(line_start);
            let e = range.end.min(line_end);
            if s >= e && !(s == e && range.start <= line_start && range.end >= line_end) {
                continue;
            }
            let Some(p1) = line.position_for_index(s - line_start, self.line_height) else {
                continue;
            };
            let Some(p2) = line.position_for_index(e - line_start, self.line_height) else {
                continue;
            };
            let base = point(self.bounds.origin.x, self.bounds.origin.y + self.line_y(ix));
            if p1.y == p2.y {
                // One visual row.
                out.push(Bounds::from_corners(
                    point(base.x + p1.x, base.y + p1.y),
                    point(base.x + p2.x, base.y + p1.y + self.line_height),
                ));
            } else {
                let row_width = line.width();
                // First row: from p1 to the wrap edge.
                out.push(Bounds::from_corners(
                    point(base.x + p1.x, base.y + p1.y),
                    point(base.x + row_width, base.y + p1.y + self.line_height),
                ));
                // Full middle rows.
                let mut y = p1.y + self.line_height;
                while y < p2.y {
                    out.push(Bounds::from_corners(
                        point(base.x, base.y + y),
                        point(base.x + row_width, base.y + y + self.line_height),
                    ));
                    y += self.line_height;
                }
                // Last row: from the left edge to p2.
                out.push(Bounds::from_corners(
                    point(base.x, base.y + p2.y),
                    point(base.x + p2.x, base.y + p2.y + self.line_height),
                ));
            }
        }
        out
    }

    /// Byte offset closest to a window-absolute `pos`. `None` only when
    /// there is no layout at all.
    pub(crate) fn index_at_point(&self, pos: Point<Pixels>) -> Option<usize> {
        if self.placeholder {
            return Some(0);
        }
        let local = point(pos.x - self.bounds.origin.x, pos.y - self.bounds.origin.y);
        if local.y < px(0.0) {
            return Some(0);
        }
        let mut y = px(0.0);
        for (ix, line) in self.lines.iter().enumerate() {
            let h = line.size(self.line_height).height;
            if local.y < y + h || ix == self.lines.len() - 1 {
                let in_line = point(local.x, (local.y - y).max(px(0.0)));
                let local_ix = line
                    .index_for_position(in_line, self.line_height)
                    .unwrap_or_else(|closest| closest);
                return Some((self.line_starts[ix] + local_ix).min(self.len()));
            }
            y += h;
        }
        Some(self.len())
    }
}

/// Byte ranges of the logical lines in `text` (newline bytes excluded);
/// always at least one range, and a trailing `\n` yields a final empty
/// line — matching how the buffer's caret addresses a blank last line.
pub(crate) fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(start..i);
            start = i + 1;
        }
    }
    out.push(start..text.len());
    out
}

/// Split `0..len` into segments, each tagged with the index of the
/// highlight span covering it (later spans win on overlap). Pure — the
/// run builder maps segments onto `TextRun`s.
pub(crate) fn segment_spans(
    len: usize,
    spans: &[HighlightSpan],
) -> Vec<(Range<usize>, Option<usize>)> {
    let mut boundaries: Vec<usize> = vec![0, len];
    for s in spans {
        boundaries.push(s.range.start.min(len));
        boundaries.push(s.range.end.min(len));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut out = Vec::new();
    for w in boundaries.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let covering = spans
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.range.start <= a && s.range.end >= b)
            .map(|(i, _)| i);
        out.push((a..b, covering));
    }
    out
}

/// Build the shaped-text run list for `text` under `style`, applying the
/// highlight spans. Run lengths always sum to `text.len()` exactly —
/// `shape_text` requires full coverage.
fn build_runs(text: &str, style: &TextStyle, spans: &[HighlightSpan]) -> Vec<TextRun> {
    let base = TextRun {
        len: 0,
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    if spans.is_empty() {
        return vec![TextRun {
            len: text.len(),
            ..base
        }];
    }
    segment_spans(text.len(), spans)
        .into_iter()
        .map(|(range, span_ix)| {
            let mut run = TextRun {
                len: range.end - range.start,
                ..base.clone()
            };
            if let Some(ix) = span_ix {
                let span = &spans[ix];
                if let Some(c) = span.color {
                    run.color = c.into();
                }
                if let Some(bg) = span.background {
                    run.background_color = Some(bg.into());
                }
                if let Some(u) = span.underline {
                    run.underline = Some(UnderlineStyle {
                        color: Some(u.color.into()),
                        thickness: px(u.thickness),
                        wavy: u.wavy,
                    });
                }
            }
            run
        })
        .collect()
}

/// Shape the input's display text (buffer, or muted placeholder when
/// empty) at `wrap_width`. Returns the shaped lines + their logical
/// starts + whether the placeholder was shown.
fn shape(
    input: &TextInput,
    style: &TextStyle,
    wrap_width: Option<Pixels>,
    window: &Window,
) -> (Vec<WrappedLine>, Vec<usize>, bool) {
    let (text, spans, placeholder): (SharedString, &[HighlightSpan], bool) =
        if input.buffer.is_empty() {
            (input.placeholder.clone(), &[], true)
        } else {
            (
                SharedString::from(input.buffer.text().to_owned()),
                input.highlights.as_slice(),
                false,
            )
        };

    let mut style = style.clone();
    if placeholder {
        style.color = TEXT_MUTED.into();
    }

    let font_size = style.font_size.to_pixels(window.rem_size());
    let runs = build_runs(&text, &style, spans);
    let line_starts = line_ranges(&text).iter().map(|r| r.start).collect();
    let lines = window
        .text_system()
        .shape_text(text, font_size, &runs, wrap_width, None)
        .map(|sv| sv.into_iter().collect::<Vec<_>>())
        .unwrap_or_default();
    (lines, line_starts, placeholder)
}

/// The custom element. Built fresh each frame by `TextInput::render`.
pub(crate) struct TextArea {
    pub(crate) input: Entity<TextInput>,
}

pub(crate) struct TextAreaPrepaint {
    lines: Vec<WrappedLine>,
    line_starts: Vec<usize>,
    line_height: Pixels,
    placeholder: bool,
    selection: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
}

impl IntoElement for TextArea {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextArea {
    type RequestLayoutState = ();
    type PrepaintState = TextAreaPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        // Height is content-measured: shape at the offered width and sum
        // the wrapped-line heights, so soft-wrap grows the input and the
        // parent's `overflow_y_scroll` can do its job.
        let entity = self.input.clone();
        // Capture the resolved text style NOW — the ancestor div's
        // font/size refinements are on the stack during request_layout
        // (the same capture gpui's own `Text` element does); the measure
        // closure runs later, outside the styled tree walk.
        let text_style = window.text_style();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                });
                let line_height = text_style.line_height_in_pixels(window.rem_size());
                let input = entity.read(cx);
                let (lines, _, _) = shape(input, &text_style, width, window);
                let height = lines
                    .iter()
                    .map(|l| l.size(line_height).height)
                    .fold(px(0.0), |a, b| a + b)
                    .max(line_height);
                size(width.unwrap_or(px(0.0)), height)
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let line_height = style.line_height_in_pixels(window.rem_size());
        let input = self.input.read(cx);
        let focused = input.focus_handle.is_focused(window);
        let cursor = input.buffer.cursor();
        let selection = input.buffer.selection();

        let (lines, line_starts, placeholder) =
            shape(input, &style, Some(bounds.size.width), window);

        // Geometry queries ride a transient LayoutInfo over the freshly
        // shaped lines (same code path the public metrics API uses).
        let info = LayoutInfo {
            lines,
            line_starts,
            bounds,
            line_height,
            placeholder,
        };

        // Selection highlight quads (behind the glyphs).
        let mut selection_quads = Vec::new();
        if !placeholder {
            if let Some((s, e)) = selection {
                for rect in info.rects_for_range(s..e) {
                    selection_quads.push(fill(rect, BORDER_EMPHASIS));
                }
            }
        }

        // Solid caret whenever focused — on the placeholder it sits at
        // column 0, preserving the old empty-input affordance.
        let caret = if focused {
            let caret_offset = if placeholder { 0 } else { cursor };
            info.caret_rect(caret_offset).map(|r| fill(r, TEXT_PRIMARY))
        } else {
            None
        };

        let LayoutInfo {
            lines, line_starts, ..
        } = info;
        TextAreaPrepaint {
            lines,
            line_starts,
            line_height,
            placeholder,
            selection: selection_quads,
            caret,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }

        let mut y = px(0.0);
        for line in &prepaint.lines {
            let origin = point(bounds.origin.x, bounds.origin.y + y);
            let _ = line.paint(
                origin,
                prepaint.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            y += line.size(prepaint.line_height).height;
        }

        if let Some(caret) = prepaint.caret.take() {
            window.paint_quad(caret);
        }

        // Publish the painted-frame snapshot for the metrics API + mouse
        // handlers (one-frame-stale by contract; see module docs).
        let info = LayoutInfo {
            lines: std::mem::take(&mut prepaint.lines),
            line_starts: std::mem::take(&mut prepaint.line_starts),
            bounds,
            line_height: prepaint.line_height,
            placeholder: prepaint.placeholder,
        };
        self.input.update(cx, |input, _| {
            input.last_layout = Some(info);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UnderlineSpec;

    fn span(range: Range<usize>) -> HighlightSpan {
        HighlightSpan {
            range,
            color: None,
            background: None,
            underline: Some(UnderlineSpec {
                color: gpui::Rgba {
                    r: 1.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                thickness: 1.0,
                wavy: true,
            }),
        }
    }

    #[test]
    fn line_ranges_split_on_newlines_with_trailing_empty() {
        assert_eq!(line_ranges(""), vec![0..0]);
        assert_eq!(line_ranges("abc"), vec![0..3]);
        assert_eq!(line_ranges("ab\ncd"), vec![0..2, 3..5]);
        // Trailing newline → a final empty line the caret can sit on.
        assert_eq!(line_ranges("ab\n"), vec![0..2, 3..3]);
        assert_eq!(line_ranges("\n\n"), vec![0..0, 1..1, 2..2]);
    }

    #[test]
    fn segments_cover_everything_exactly_once() {
        let spans = vec![span(2..5), span(8..10)];
        let segs = segment_spans(12, &spans);
        // Contiguous cover of 0..12.
        let mut cursor = 0;
        for (r, _) in &segs {
            assert_eq!(r.start, cursor);
            cursor = r.end;
        }
        assert_eq!(cursor, 12);
        // The highlighted stretches carry their span index.
        assert!(segs.iter().any(|(r, s)| *r == (2..5) && *s == Some(0)));
        assert!(segs.iter().any(|(r, s)| *r == (8..10) && *s == Some(1)));
        assert!(segs.iter().any(|(r, s)| *r == (0..2) && s.is_none()));
    }

    #[test]
    fn segments_clamp_out_of_range_and_let_later_spans_win() {
        // A span past EOF clamps; overlapping spans → the later one wins.
        let spans = vec![span(0..6), span(4..20)];
        let segs = segment_spans(10, &spans);
        let mut cursor = 0;
        for (r, _) in &segs {
            assert_eq!(r.start, cursor);
            cursor = r.end;
        }
        assert_eq!(cursor, 10);
        // 4..6 is covered by both; the later span (index 1) wins.
        assert!(segs.iter().any(|(r, s)| *r == (4..6) && *s == Some(1)));
        assert!(segs.iter().any(|(r, s)| *r == (6..10) && *s == Some(1)));
        assert!(segs.iter().any(|(r, s)| *r == (0..4) && *s == Some(0)));
    }

    #[test]
    fn segments_empty_spans_yield_one_uncovered_segment() {
        let segs = segment_spans(5, &[]);
        assert_eq!(segs, vec![(0..5, None)]);
        assert!(segment_spans(0, &[]).is_empty());
    }
}
