//! Painted-frame layout snapshot + glyph-metrics queries.
//!
//! After each paint the element publishes a [`LayoutSnapshot`] of the **visible
//! (culled) lines** back onto the view. It describes the LAST painted frame —
//! callers treat it as best-effort-stale by one frame, the same contract the
//! `wylde-gpui-input` metrics API established. Mouse handlers use
//! [`LayoutSnapshot::index_at_point`] (window point → byte offset); the future
//! LSP-hover / bubble-tether work uses [`LayoutSnapshot::caret_rect`] /
//! [`LayoutSnapshot::rects_for_range`] (byte offset/range → window-absolute
//! rects). Only visible lines are shaped (the large-file headroom from OQ-3),
//! so geometry for an off-screen offset answers `None`.

use std::ops::Range;

use gpui::{point, px, Bounds, Pixels, Point, WrappedLine};

/// One shaped, currently-visible logical line (no soft-wrap: code lines
/// scroll horizontally rather than wrap, so each is a single `WrappedLine`).
pub(crate) struct VisibleLine {
    /// 0-based logical line index in the buffer.
    pub(crate) line_ix: usize,
    /// Byte offset in the buffer where this line begins.
    pub(crate) start_offset: usize,
    /// Line length in bytes (excluding the trailing newline).
    pub(crate) byte_len: usize,
    /// The shaped glyphs for this line.
    pub(crate) shaped: WrappedLine,
}

/// The post-paint snapshot the metrics API reads.
pub(crate) struct LayoutSnapshot {
    pub(crate) lines: Vec<VisibleLine>,
    /// Window-absolute bounds of the visible text area (right of the gutter).
    pub(crate) text_bounds: Bounds<Pixels>,
    pub(crate) line_height: Pixels,
    pub(crate) scroll_top: Pixels,
    pub(crate) scroll_left: Pixels,
}

impl LayoutSnapshot {
    /// Window-absolute top-left of logical line `line_ix` (accounting for
    /// scroll), regardless of visibility.
    fn line_origin(&self, line_ix: usize) -> Point<Pixels> {
        point(
            self.text_bounds.origin.x - self.scroll_left,
            self.text_bounds.origin.y + self.line_height * line_ix as f32 - self.scroll_top,
        )
    }

    fn find_visible(&self, line_ix: usize) -> Option<&VisibleLine> {
        self.lines.iter().find(|l| l.line_ix == line_ix)
    }

    /// The visible line containing byte `offset`, if any.
    fn line_for_offset(&self, offset: usize) -> Option<&VisibleLine> {
        self.lines
            .iter()
            .find(|l| offset >= l.start_offset && offset <= l.start_offset + l.byte_len)
    }

    /// Window-absolute caret rect for byte `offset`. `None` if the offset's
    /// line isn't currently visible.
    pub(crate) fn caret_rect(&self, offset: usize, caret_width: f32) -> Option<Bounds<Pixels>> {
        let line = self.line_for_offset(offset)?;
        let local = offset - line.start_offset;
        let pos = line.shaped.position_for_index(local, self.line_height)?;
        let origin = self.line_origin(line.line_ix);
        Some(Bounds::new(
            point(origin.x + pos.x, origin.y + pos.y),
            gpui::size(px(caret_width), self.line_height),
        ))
    }

    /// Window-absolute rects covering byte `range` — one per visible line the
    /// range touches. Off-screen portions are simply omitted.
    pub(crate) fn rects_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut out = Vec::new();
        if range.start >= range.end {
            return out;
        }
        for line in &self.lines {
            let line_start = line.start_offset;
            let line_end = line.start_offset + line.byte_len;
            let s = range.start.max(line_start);
            let e = range.end.min(line_end);
            // Include a line fully inside the range even when s==e (empty line).
            let covers_empty = range.start <= line_start && range.end >= line_end;
            if s > e || (s == e && !covers_empty) {
                continue;
            }
            let Some(p1) = line.shaped.position_for_index(s - line_start, self.line_height) else {
                continue;
            };
            let Some(p2) = line.shaped.position_for_index(e - line_start, self.line_height) else {
                continue;
            };
            let origin = self.line_origin(line.line_ix);
            out.push(Bounds::from_corners(
                point(origin.x + p1.x, origin.y),
                point(origin.x + p2.x, origin.y + self.line_height),
            ));
        }
        out
    }

    /// Byte offset closest to a window-absolute point. `None` only when there
    /// are no visible lines at all.
    pub(crate) fn index_at_point(&self, pos: Point<Pixels>) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        // Local coordinates inside the scrolled content space.
        let local_y = pos.y - self.text_bounds.origin.y + self.scroll_top;
        let local_x = pos.x - self.text_bounds.origin.x + self.scroll_left;
        let target_line = (local_y.max(px(0.0)) / self.line_height).floor() as usize;

        // Clamp to the visible set: above → first, below → last.
        let first = self.lines.first().unwrap();
        let last = self.lines.last().unwrap();
        let line = if target_line <= first.line_ix {
            first
        } else if target_line >= last.line_ix {
            last
        } else {
            self.find_visible(target_line).unwrap_or(last)
        };

        let in_line = point(local_x.max(px(0.0)), px(0.0));
        let local_ix = line
            .shaped
            .index_for_position(in_line, self.line_height)
            .unwrap_or_else(|closest| closest);
        Some(line.start_offset + local_ix.min(line.byte_len))
    }
}
