//! Typography — font family, named text sizes, named weights.
//!
//! The Svelte side falls back to the OS font stack when Inter isn't
//! installed locally (app.css line 46 — `font-family: 'Inter',
//! ui-sans-serif, system-ui, sans-serif;`).  The gpui-side plan §4.2
//! bundles Inter via `include_bytes!` for predictable cross-platform
//! rendering, but the foundation slice does NOT yet ship the TTF
//! assets — see `Shell/src/assets.rs::install_fonts` for the loader
//! shim and `assets/fonts/README.md` for the drop-in instructions.
//!
//! Until the TTFs land we request "Inter" by name and lean on the
//! system font stack the same way the CSS does today.  Cross-platform
//! parity with the Svelte alpha is preserved; predictability waits for
//! the bundling slice.

/// Family name passed to gpui's text system.  Matching the Svelte
/// alpha's first-choice family keeps visual identity intact when a
/// user has Inter installed locally.
pub const FAMILY_INTER: &str = "Inter";

/// Fallback family list, requested in order if `Inter` isn't resolvable.
/// gpui's text system already has its own platform fallback chain — we
/// keep this list aligned with the Svelte side's CSS for parity when
/// debugging a "wrong font is rendering" report.
pub const FALLBACK_FAMILIES: &[&str] = &[
    "Segoe UI",    // Windows system sans
    "SF Pro Text", // macOS system sans
    "Helvetica Neue",
    "Arial",
    "sans-serif",
];

/// Monospace family for code surfaces (the `wylde-gpui-code-editor` element
/// and any inline code chrome). Requested by name; gpui's text system falls
/// back through [`MONO_FALLBACK_FAMILIES`] / its own platform chain when the
/// first choice isn't installed. Cascadia Mono ships with modern Windows;
/// the fallbacks cover macOS/Linux and end in the generic `monospace` so
/// resolution always succeeds.
pub const FAMILY_MONO: &str = "Cascadia Mono";

/// Fallback monospace families, in request order.
pub const MONO_FALLBACK_FAMILIES: &[&str] = &[
    "Consolas",         // Windows
    "SF Mono",          // macOS
    "Menlo",            // macOS
    "DejaVu Sans Mono", // Linux
    "Liberation Mono",  // Linux
    "monospace",        // generic — always resolvable
];

/// Named text sizes in pixels at the default DPI.  Map roughly to the
/// Tailwind scale used on the Svelte side (text-xs / text-sm / text-base
/// / text-lg / text-xl).  Tailwind's scale is rem-based; gpui's text
/// system takes pixels.  The conversion assumes 16 px = 1 rem (the
/// browser default that Tailwind targets), which is what the existing
/// Svelte app produces at default DPI.
pub mod size {
    pub const MICRO: f32 = 10.0; // .nav-group-label (`text-micro` in app.css)
    pub const XS: f32 = 12.0; // text-xs / .badge / .input
    pub const SM: f32 = 14.0; // text-sm / .btn / .label
    pub const BASE: f32 = 16.0; // body default
    pub const LG: f32 = 18.0; // text-lg
    pub const XL: f32 = 20.0; // text-xl / panel titles
    pub const XXL: f32 = 24.0; // text-2xl / dashboard headers
}

/// Weight names match the OpenType weight axis the Inter TTF exposes.
/// Mirroring CSS so a panel author can reach for the obvious name.
pub mod weight {
    /// Inter Regular (400).
    pub const REGULAR: u16 = 400;
    /// Inter Medium (500) — most button labels.
    pub const MEDIUM: u16 = 500;
    /// Inter Semibold (600) — emphasised text.
    pub const SEMIBOLD: u16 = 600;
    /// Inter Bold (700) — sparingly, for nav-group labels.
    pub const BOLD: u16 = 700;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback list must be non-empty and contain at least one
    /// generic family so gpui's text system can always resolve
    /// *something* on first paint.
    #[test]
    fn fallback_families_have_a_generic() {
        assert!(!FALLBACK_FAMILIES.is_empty());
        assert!(
            FALLBACK_FAMILIES
                .iter()
                .any(|f| matches!(*f, "sans-serif" | "serif" | "monospace")),
            "fallback list must include a generic family so resolution always succeeds",
        );
    }

    #[test]
    fn mono_fallback_ends_in_generic_monospace() {
        assert!(!FAMILY_MONO.is_empty());
        assert_eq!(
            MONO_FALLBACK_FAMILIES.last(),
            Some(&"monospace"),
            "mono fallback must end in the generic `monospace` so resolution always succeeds",
        );
    }

    /// Sanity-check the size ladder: each named step is larger than
    /// the previous.  Catches an accidental swap during a refactor.
    #[test]
    fn size_ladder_is_monotone() {
        let ladder = [
            size::MICRO,
            size::XS,
            size::SM,
            size::BASE,
            size::LG,
            size::XL,
            size::XXL,
        ];
        for pair in ladder.windows(2) {
            assert!(
                pair[0] < pair[1],
                "typography size ladder must increase; {} >= {} failed",
                pair[0],
                pair[1],
            );
        }
    }

    /// Inter weight axis is 100..=900; named weights stay inside.
    #[test]
    fn weights_inside_inter_axis() {
        for w in [
            weight::REGULAR,
            weight::MEDIUM,
            weight::SEMIBOLD,
            weight::BOLD,
        ] {
            assert!((100..=900).contains(&w), "{w} outside Inter weight axis");
        }
    }
}
