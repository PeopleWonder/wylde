//! Colour tokens — gpui `Rgba` constants mirroring `Core/GUI/src/app.css`
//! :root.  The Svelte side reads CSS variables like `var(--surface-800)`;
//! the gpui side reads `wylde_theme::colors::SURFACE_800`.  Same numbers.
//!
//! Translucent tokens (`BORDER_*`) use `rgba_from_hex` so the hex literal
//! stays readable next to the alpha; the helper composes the same value
//! the gpui `rgba()` constructor would.

use gpui::{rgb, rgba, Rgba};

// ── Surface scale ────────────────────────────────────────────────────
//
// Deep-navy series the dashboard, sidebar, and cards sit on.  Numbers
// roughly correspond to Tailwind shade weight (950 = darkest, 400 =
// lightest) and match the CSS in app.css line 7–15.

pub const SURFACE_950: Rgba = rgb_const(0x060a12);
pub const SURFACE_900: Rgba = rgb_const(0x0a0e17);
pub const SURFACE_800: Rgba = rgb_const(0x0d1320);
pub const SURFACE_750: Rgba = rgb_const(0x111a2e);
pub const SURFACE_700: Rgba = rgb_const(0x182236);
pub const SURFACE_650: Rgba = rgb_const(0x1e2a40);
pub const SURFACE_600: Rgba = rgb_const(0x243450);
pub const SURFACE_500: Rgba = rgb_const(0x3d5070);
pub const SURFACE_400: Rgba = rgb_const(0x5a7294);

// ── Brand + accent ───────────────────────────────────────────────────

pub const BRAND: Rgba = rgb_const(0x0e7490);
pub const BRAND_LIGHT: Rgba = rgb_const(0x06b6d4);
pub const BRAND_DIM: Rgba = rgb_const(0x155e75);
pub const RING: Rgba = BRAND;
pub const ACCENT_CYAN: Rgba = rgb_const(0x0891b2);

// ── Text scale ───────────────────────────────────────────────────────
//
// White-font pass (visual-polish W1): the floor was lifted toward white so
// nothing is hard to read on the deep-navy surfaces, while the *relative*
// ladder (primary > secondary > muted > dim) is preserved so the hierarchy
// doesn't flatten. The "ignored / disabled / placeholder" signal that used
// to ride on the dark `TEXT_MUTED` hue alone is now carried by a NON-colour
// means (italic + opacity + badge) at the consuming sites — see
// `files/mod.rs` (ignored rows) and the Input placeholder/disabled cues —
// so brightening these tokens does not erase it.
pub const TEXT_PRIMARY: Rgba = rgb_const(0xffffff); // body / headings / active — true white
pub const TEXT_SECONDARY: Rgba = rgb_const(0xd6deea); // regular files, metadata — a notch below primary
pub const TEXT_MUTED: Rgba = rgb_const(0x9aa7b8); // muted-but-light — readable, visibly secondary
pub const TEXT_DIM: Rgba = rgb_const(0x7c8aa0); // genuinely de-emphasized

// ── Semantic status ──────────────────────────────────────────────────
//
// Danger/warning hues for error strips and editor diagnostic squiggles (IDE
// S9). Kept as theme tokens so a palette change is one edit, not scattered hex.
pub const DANGER: Rgba = rgb_const(0xe06c75);
pub const WARNING: Rgba = rgb_const(0xe5c07b);

// ── Borders (translucent over deep-navy surface) ─────────────────────
//
// The Svelte version expresses these as `rgba(14, 116, 144, 0.1)` and
// friends.  In gpui we need const-initialisable values; the hex form
// for the colour + an explicit alpha keeps both readable.

pub const BORDER_SUBTLE: Rgba = rgba_const(0x0e7490, 0.10);
pub const BORDER_DEFAULT: Rgba = rgba_const(0x0e7490, 0.16);
pub const BORDER_EMPHASIS: Rgba = rgba_const(0x0e7490, 0.28);
/// Focused-input border ring.  Same brand hue as the other border
/// tokens, but a strong alpha so a focused field reads as a clear (not
/// hairline) outline once rendered alpha-correctly.  Only meaningful for
/// consumers that pass the `Rgba` through to `border_color` directly;
/// the `rgb(pack(..))` idiom used elsewhere drops the alpha.
pub const BORDER_FOCUSED: Rgba = rgba_const(0x0e7490, 0.75);

// ── Syntax-highlight palette (IDE code editor) ───────────────────────
//
// Foreground colours the code editor maps tree-sitter highlight scopes onto
// (the panel owns the scope→token mapping; these are the *tokens*, so a
// palette change touches one file). A dark, One-Dark-adjacent set tuned to sit
// on the SURFACE_950 editor background. Default (unmatched) text uses
// `TEXT_PRIMARY`.
pub mod syntax {
    use super::{rgb_const, Rgba};

    pub const KEYWORD: Rgba = rgb_const(0xc586c0); // control flow, fn, let, pub…
    pub const STRING: Rgba = rgb_const(0x98c379); // string / char literals
    pub const COMMENT: Rgba = rgb_const(0x5c6a76); // comments (muted)
    pub const FUNCTION: Rgba = rgb_const(0x61afef); // fn names, methods
    pub const TYPE: Rgba = rgb_const(0xe5c07b); // types, traits, structs
    pub const NUMBER: Rgba = rgb_const(0xd19a66); // numeric literals
    pub const CONSTANT: Rgba = rgb_const(0xd19a66); // consts, builtins, enums
    pub const VARIABLE: Rgba = rgb_const(0xe2e8f0); // identifiers / properties
    pub const OPERATOR: Rgba = rgb_const(0x94a3b8); // operators
    pub const PUNCTUATION: Rgba = rgb_const(0x94a3b8); // brackets, delimiters
    pub const ATTRIBUTE: Rgba = rgb_const(0x06b6d4); // attributes / decorators
    pub const TAG: Rgba = rgb_const(0xe06c75); // markup tags / labels
}

// ── Const-time constructors ──────────────────────────────────────────
//
// `gpui::rgb`/`rgba` aren't `const fn` in every gpui release.  These
// shims build the same `Rgba` value at const-eval so the tokens above
// are real constants — no runtime initialisation, importable from any
// panel without lock contention.

const fn rgb_const(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const fn rgba_const(hex: u32, alpha: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: alpha,
    }
}

// Suppress the unused-import warning when gpui's helper API changes;
// keeping `rgb` + `rgba` imported documents the intended runtime
// equivalents of the const constructors above.
#[allow(dead_code)]
fn _runtime_equivalents_for_docs() -> (Rgba, Rgba) {
    (rgb(0x0e7490), rgba(0x0e749019))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handful of round-trip checks — if the const constructor ever
    /// breaks (gpui changes the Rgba field layout, someone fat-fingers
    /// a hex literal), one of these fires.
    #[test]
    fn surface_900_matches_app_css() {
        // app.css line 8: `--surface-900: #0a0e17;`
        let c = SURFACE_900;
        assert!((c.r - 0x0a as f32 / 255.0).abs() < 1e-6);
        assert!((c.g - 0x0e as f32 / 255.0).abs() < 1e-6);
        assert!((c.b - 0x17 as f32 / 255.0).abs() < 1e-6);
        assert!((c.a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn brand_matches_app_css() {
        // app.css line 17: `--brand: #0e7490;`
        let c = BRAND;
        assert!((c.r - 0x0e as f32 / 255.0).abs() < 1e-6);
        assert!((c.g - 0x74 as f32 / 255.0).abs() < 1e-6);
        assert!((c.b - 0x90 as f32 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn border_default_uses_translucent_brand() {
        // app.css line 25: `--border-default: rgba(14, 116, 144, 0.16);`
        let c = BORDER_DEFAULT;
        assert!((c.r - 14.0 / 255.0).abs() < 1e-6);
        assert!((c.g - 116.0 / 255.0).abs() < 1e-6);
        assert!((c.b - 144.0 / 255.0).abs() < 1e-6);
        assert!((c.a - 0.16).abs() < 1e-6);
    }

    /// Sanity that the ring colour aliases brand — `--ring-color: #0e7490`.
    #[test]
    fn ring_is_brand() {
        assert_eq!(RING.r, BRAND.r);
        assert_eq!(RING.g, BRAND.g);
        assert_eq!(RING.b, BRAND.b);
        assert_eq!(RING.a, BRAND.a);
    }

    /// White-font pass (W1): the four text tokens were lifted toward white.
    /// Pin the new values so an accidental revert or fat-finger is caught.
    #[test]
    fn text_tokens_lifted_toward_white() {
        let expect = |c: Rgba, hex: u32| {
            assert!((c.r - ((hex >> 16) & 0xff) as f32 / 255.0).abs() < 1e-6);
            assert!((c.g - ((hex >> 8) & 0xff) as f32 / 255.0).abs() < 1e-6);
            assert!((c.b - (hex & 0xff) as f32 / 255.0).abs() < 1e-6);
        };
        expect(TEXT_PRIMARY, 0xffffff);
        expect(TEXT_SECONDARY, 0xd6deea);
        expect(TEXT_MUTED, 0x9aa7b8);
        expect(TEXT_DIM, 0x7c8aa0);
    }

    /// The text ladder must keep its *relative* steps (primary brightest,
    /// dim darkest) so the hierarchy survives the white-font lift — the
    /// whole point of W1 is "lift the floor, keep the ladder".
    #[test]
    fn text_ladder_is_monotone() {
        let lightness = |c: Rgba| c.r + c.g + c.b;
        assert!(lightness(TEXT_PRIMARY) > lightness(TEXT_SECONDARY));
        assert!(lightness(TEXT_SECONDARY) > lightness(TEXT_MUTED));
        assert!(lightness(TEXT_MUTED) > lightness(TEXT_DIM));
    }

    /// Defensive: the entire surface ladder is monotonically darkening
    /// toward 950, matching designer intent.  Catches accidental swaps
    /// of two tokens during a rename.
    #[test]
    fn surface_ladder_is_monotone() {
        let lightness = |c: Rgba| c.r + c.g + c.b;
        let ladder = [
            SURFACE_400,
            SURFACE_500,
            SURFACE_600,
            SURFACE_650,
            SURFACE_700,
            SURFACE_750,
            SURFACE_800,
            SURFACE_900,
            SURFACE_950,
        ];
        for pair in ladder.windows(2) {
            assert!(
                lightness(pair[0]) > lightness(pair[1]),
                "surface ladder must darken; {:?} > {:?} failed",
                pair[0],
                pair[1],
            );
        }
    }
}
