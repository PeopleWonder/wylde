//! Decoration runs — the editor's styling layer.
//!
//! A [`Decoration`] tags a byte range with a foreground colour, optional
//! background, and optional underline (straight or wavy). This is the single
//! mechanism the editor uses for **syntax highlighting** (S4 maps
//! `treesitter.highlight` classes → themed colours) and later for **LSP
//! diagnostics** (S9 paints wavy red/yellow underlines under error/warning
//! ranges). gpui draws (wavy) underlines itself, wrap-aware, so a squiggle
//! needs no custom painting — exactly the proven `wylde-gpui-input`
//! `HighlightSpan` mechanism, re-implemented here as a sibling.
//!
//! The pure segmentation/run-building logic lives here so it is testable
//! without a window; the element calls [`build_line_runs`] per visible line.

use std::ops::Range;

use gpui::{px, Hsla, Rgba, TextRun, TextStyle, UnderlineStyle};

/// An underline under a decorated range. `wavy` gives the IDE squiggle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Underline {
    pub color: Rgba,
    pub thickness: f32,
    pub wavy: bool,
}

/// One styled byte range. Later decorations win on overlap (so a diagnostic
/// underline layered over a syntax colour keeps the colour and adds the
/// squiggle when only `underline` is set — see [`Decoration::underline_only`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Decoration {
    /// Byte range in the *whole buffer* (the element rebases it per line).
    pub range: Range<usize>,
    pub color: Option<Rgba>,
    pub background: Option<Rgba>,
    pub underline: Option<Underline>,
}

impl Decoration {
    /// A foreground-colour-only decoration (the syntax-highlight common case).
    pub fn color(range: Range<usize>, color: Rgba) -> Self {
        Self {
            range,
            color: Some(color),
            background: None,
            underline: None,
        }
    }

    /// A wavy-underline-only decoration (the diagnostic common case) — leaves
    /// the underlying syntax colour intact.
    pub fn underline_only(range: Range<usize>, color: Rgba) -> Self {
        Self {
            range,
            color: None,
            background: None,
            underline: Some(Underline {
                color,
                thickness: 1.0,
                wavy: true,
            }),
        }
    }
}

/// Split `0..len` into segments tagged with the index of the **colour/bg**
/// decoration covering it and the index of the **underline** decoration
/// covering it (kept separate so an underline-only diagnostic doesn't erase a
/// syntax colour). Later decorations win on overlap. Pure.
pub(crate) fn segment(
    len: usize,
    decos: &[Decoration],
) -> Vec<(Range<usize>, Option<usize>, Option<usize>)> {
    let mut boundaries: Vec<usize> = vec![0, len];
    for d in decos {
        boundaries.push(d.range.start.min(len));
        boundaries.push(d.range.end.min(len));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut out = Vec::new();
    for w in boundaries.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let fill = decos
            .iter()
            .enumerate()
            .rev()
            .find(|(_, d)| {
                (d.color.is_some() || d.background.is_some())
                    && d.range.start <= a
                    && d.range.end >= b
            })
            .map(|(i, _)| i);
        let under = decos
            .iter()
            .enumerate()
            .rev()
            .find(|(_, d)| d.underline.is_some() && d.range.start <= a && d.range.end >= b)
            .map(|(i, _)| i);
        out.push((a..b, fill, under));
    }
    out
}

/// Build the shaped-text run list for one line's `text` under `style`,
/// applying decorations whose ranges have already been rebased to
/// line-local byte offsets (`0..text.len()`). Run lengths sum to
/// `text.len()` exactly — `shape_text` requires full coverage.
pub(crate) fn build_line_runs(text: &str, style: &TextStyle, decos: &[Decoration]) -> Vec<TextRun> {
    let base = TextRun {
        len: 0,
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    if decos.is_empty() || text.is_empty() {
        return vec![TextRun {
            len: text.len(),
            ..base
        }];
    }
    segment(text.len(), decos)
        .into_iter()
        .map(|(range, fill_ix, under_ix)| {
            let mut run = TextRun {
                len: range.end - range.start,
                ..base.clone()
            };
            if let Some(ix) = fill_ix {
                let d = &decos[ix];
                if let Some(c) = d.color {
                    run.color = rgba_to_hsla(c);
                }
                if let Some(bg) = d.background {
                    run.background_color = Some(rgba_to_hsla(bg));
                }
            }
            if let Some(ix) = under_ix {
                if let Some(u) = decos[ix].underline {
                    run.underline = Some(UnderlineStyle {
                        color: Some(rgba_to_hsla(u.color)),
                        thickness: px(u.thickness),
                        wavy: u.wavy,
                    });
                }
            }
            run
        })
        .collect()
}

fn rgba_to_hsla(c: Rgba) -> Hsla {
    c.into()
}

/// Rebase whole-buffer decorations onto a single line spanning
/// `line_start..line_end` (byte offsets), returning line-local decorations
/// clamped to `0..(line_end-line_start)`. Decorations not touching the line
/// are dropped. Pure — the element calls this per visible line.
pub(crate) fn decorations_for_line(
    decos: &[Decoration],
    line_start: usize,
    line_end: usize,
) -> Vec<Decoration> {
    let mut out = Vec::new();
    for d in decos {
        let s = d.range.start.max(line_start);
        let e = d.range.end.min(line_end);
        if s >= e {
            continue;
        }
        out.push(Decoration {
            range: (s - line_start)..(e - line_start),
            color: d.color,
            background: d.background,
            underline: d.underline,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Rgba {
        Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }
    }

    #[test]
    fn segments_cover_exactly_and_later_wins() {
        let decos = vec![
            Decoration::color(0..6, red()),
            Decoration::color(4..10, red()),
        ];
        let segs = segment(10, &decos);
        let mut cursor = 0;
        for (r, _, _) in &segs {
            assert_eq!(r.start, cursor);
            cursor = r.end;
        }
        assert_eq!(cursor, 10);
        // 4..6 overlapped → later decoration (index 1) wins the fill.
        assert!(segs.iter().any(|(r, f, _)| *r == (4..6) && *f == Some(1)));
    }

    #[test]
    fn underline_only_keeps_separate_fill() {
        // A syntax colour 0..10 + a diagnostic underline 3..6: the underline
        // segment keeps the colour fill index (0), adds the underline index.
        let decos = vec![
            Decoration::color(0..10, red()),
            Decoration::underline_only(3..6, red()),
        ];
        let segs = segment(10, &decos);
        let mid = segs.iter().find(|(r, _, _)| *r == (3..6)).unwrap();
        assert_eq!(mid.1, Some(0), "fill is still the syntax colour");
        assert_eq!(mid.2, Some(1), "underline is the diagnostic");
    }

    #[test]
    fn empty_decos_one_run() {
        let style = TextStyle::default();
        let runs = build_line_runs("abc", &style, &[]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 3);
    }

    #[test]
    fn runs_sum_to_text_len() {
        let style = TextStyle::default();
        let decos = vec![Decoration::color(1..2, red())];
        let runs = build_line_runs("abcd", &style, &decos);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), 4);
    }

    #[test]
    fn line_rebase_clamps_and_drops() {
        let decos = vec![
            Decoration::color(2..8, red()),     // spans into the line
            Decoration::color(100..110, red()), // far away → dropped
        ];
        // Line spans buffer bytes 4..10 → local 0..6.
        let local = decorations_for_line(&decos, 4, 10);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].range, 0..4); // (4..8) - 4
    }
}
