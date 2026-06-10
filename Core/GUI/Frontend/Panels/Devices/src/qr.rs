//! QR matrix rendering for the pairing card.
//!
//! gpui at `b3d93d44` has no SVG element or raster-image element a
//! panel can stick into a `div` tree.  We sidestep both by treating
//! the QR as a grid of coloured cells — one tiny `div` per module —
//! and lay them out with `flex_row` / `flex_col`.  The visual is the
//! same as a rendered SVG and the implementation needs zero new
//! asset deps.
//!
//! Module count for a v1-M QR is 21×21; the higher version we tend to
//! emit for `wylde://pair?code=…` is 29×29 (v3-M).  At 6 px / module
//! that's a ~174 px square — readable from arm's length on a phone
//! camera, which is the design target the slice spec calls out.

use gpui::{div, prelude::*, px, rgb, SharedString};
use qrcode::types::Color;
use qrcode::{EcLevel, QrCode};

/// Pixel size of one QR module on the rendered grid.  Small enough
/// that a v3 code stays under ~200 px square (fits in the card
/// without dominating it); large enough that an arm's-length phone
/// camera reliably resolves the corners.
pub const MODULE_PX: f32 = 6.0;

/// White padding modules around the matrix.  QR readers need a "quiet
/// zone" of at least 4 light modules to lock; less than that and the
/// scan rate plummets.
pub const QUIET_MODULES: usize = 4;

/// Convenience wrapper around a QR matrix the View can render
/// directly.  Always succeeds with a non-empty matrix: if the input
/// payload happens to overflow `qrcode`'s addressable size we fall
/// back to a single light module so the View renders a small placeholder
/// rather than panicking.
#[derive(Debug, Clone)]
pub struct QrMatrix {
    /// Module grid in row-major order, NOT including the quiet zone.
    /// `true` = dark module (drawn black), `false` = light (drawn white).
    pub modules: Vec<bool>,
    /// Side length in modules — the matrix is square so `modules.len()
    /// == size * size`.
    pub size: usize,
}

impl QrMatrix {
    /// Encode `payload` (any UTF-8 string — typically a `wylde://pair`
    /// URI) at error-correction level M.  Level M tolerates ~15 % of
    /// the code being obscured by smudges / glare, which is the
    /// canonical mobile-scan choice.
    pub fn encode(payload: &str) -> Self {
        match QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M) {
            Ok(code) => {
                let size = code.width();
                // `QrCode::to_colors` returns the matrix in row-major
                // `Color` order — Dark / Light.  Project to `bool` so
                // the View doesn't need to know about the qrcode crate.
                let modules: Vec<bool> = code
                    .to_colors()
                    .into_iter()
                    .map(|c| matches!(c, Color::Dark))
                    .collect();
                Self { modules, size }
            }
            Err(_) => Self {
                modules: vec![false],
                size: 1,
            },
        }
    }

    /// True when the QR call fell back to a 1×1 light module — used by
    /// the View to render "QR unavailable" placeholder text instead of
    /// painting a useless single-cell grid.
    pub fn is_placeholder(&self) -> bool {
        self.size <= 1
    }

    /// Module at `(row, col)`, zero-based.  Returns `false` (light)
    /// for out-of-range coordinates so the caller can naively iterate
    /// `0..total` for the rendered grid.
    pub fn module(&self, row: usize, col: usize) -> bool {
        if row >= self.size || col >= self.size {
            return false;
        }
        self.modules
            .get(row * self.size + col)
            .copied()
            .unwrap_or(false)
    }
}

/// Build a gpui element that renders `matrix` as a flex column of
/// rows.  The element ships its own white background + quiet-zone
/// padding so it can be dropped directly into a card body without
/// extra wrappers.
pub fn render_matrix(matrix: &QrMatrix) -> gpui::Div {
    let total = matrix.size + 2 * QUIET_MODULES;
    let dimension = px(total as f32 * MODULE_PX);

    let mut grid = div()
        .w(dimension)
        .h(dimension)
        .bg(rgb(0xff_ff_ff))
        .flex()
        .flex_col();

    if matrix.is_placeholder() {
        // Single light module — emit a centred glyph so the user knows
        // the slot is intentionally empty rather than rendering a 6 px
        // white square that looks broken.
        return grid.items_center().justify_center().child(
            div()
                .text_color(rgb(0x44_44_44))
                .text_size(px(12.0))
                .child(SharedString::from("QR unavailable")),
        );
    }

    for r in 0..total {
        let mut row = div().flex().flex_row().w(dimension).h(px(MODULE_PX));
        for c in 0..total {
            let dark = if (QUIET_MODULES..QUIET_MODULES + matrix.size).contains(&r)
                && (QUIET_MODULES..QUIET_MODULES + matrix.size).contains(&c)
            {
                matrix.module(r - QUIET_MODULES, c - QUIET_MODULES)
            } else {
                false
            };
            let cell = div().w(px(MODULE_PX)).h(px(MODULE_PX)).bg(rgb(if dark {
                0x00_00_00
            } else {
                0xff_ff_ff
            }));
            row = row.child(cell);
        }
        grid = grid.child(row);
    }
    grid
}

/// Build the URI the QR encodes for `code`.  Centralised here so the
/// View and the tests can both rely on the canonical scheme; if the Wylde user
/// later changes the mobile app's pair handler this is the single
/// place to update.
pub fn pair_uri(code: &str) -> String {
    format!("wylde://pair?code={code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_uri_uses_canonical_scheme() {
        assert_eq!(pair_uri("123456"), "wylde://pair?code=123456");
    }

    #[test]
    fn encode_six_digit_pin_produces_non_trivial_matrix() {
        let m = QrMatrix::encode(&pair_uri("123456"));
        // v1 QR is 21 modules; our pair URI fits comfortably in v1-M.
        assert!(!m.is_placeholder());
        assert!(m.size >= 21);
        assert_eq!(m.modules.len(), m.size * m.size);
        // QR finder patterns are 7×7 squares of dark modules in three
        // corners — the top-left finder is at (0..7, 0..7).  We don't
        // assert the exact shape, but the corner must contain dark
        // cells, which gives a cheap sanity that the encoder didn't
        // produce an all-light matrix.
        assert!((0..7).any(|r| (0..7).any(|c| m.module(r, c))));
    }

    #[test]
    fn module_out_of_range_returns_false() {
        let m = QrMatrix::encode(&pair_uri("000000"));
        assert!(!m.module(m.size, 0));
        assert!(!m.module(0, m.size));
        assert!(!m.module(usize::MAX, usize::MAX));
    }

    #[test]
    fn quiet_zone_constants_meet_qr_spec_minimum() {
        // QR spec calls for >= 4 module quiet zone on all sides.
        // Keep this as a tripwire — a future tweak that drops it
        // below 4 surfaces here rather than as silent unreadable
        // scans in production.  `const { ... }` so clippy doesn't
        // flag the constant-only assert.
        const _: () = assert!(QUIET_MODULES >= 4);
    }

    #[test]
    fn module_px_yields_readable_render() {
        // Lower bound of 4 px / module matches the qrcode crate's
        // recommendation for paper-print readability; phones tolerate
        // a bit less but the slice spec wants "readable from arm's
        // length on a phone screen".
        const _: () = assert!(MODULE_PX >= 4.0);
    }
}
