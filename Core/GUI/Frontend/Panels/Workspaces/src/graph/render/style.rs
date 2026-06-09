//! `Theme` — the serializable, locked **Visual Style v1** the renderer
//! consumes (Phase 2.5 deliverable, OI-16). The canonical style lives on
//! Nextcloud ("The Thought Bubble System (visual style, v1)"); an in-repo
//! versioned mirror is embedded at compile time and parsed here.
//!
//! **The renderer reads every colour / size / edge style FROM this struct —
//! it never hardcodes a value** (Build Order §8; Visual Style "Implementation
//! hooks for Phase 3" #3). Hot-reload via Settings is C-settings's concern;
//! C-scaffold loads the theme once at panel mount.
//!
//! Only the sections C-scaffold renders are modelled (sphere shading,
//! language colours, module palette, node-type treatments, edge styles, panel
//! background). serde ignores the rest of the YAML (bubbles / composer /
//! chrome / animations / icons), so later slices extend `Theme` by adding
//! fields — no parser change.

use std::collections::HashMap;

use serde::Deserialize;

/// The locked Visual Style v1, embedded at compile time. Single source of
/// truth is the Nextcloud page; re-sync this asset in the same commit when
/// the page changes.
const VISUAL_STYLE_V1_YAML: &str = include_str!("../../../assets/visual_style_v1.yaml");

/// An RGBA colour in linear 0..=1 components. Parsed from the theme's hex
/// (`#RRGGBB`) and `rgba(r,g,b,a)` strings; the gpui layer converts it to a
/// `gpui::Rgba` at paint time (render/mod.rs holds the conversion-free draw
/// list; the panel does the gpui hand-off).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// A visible fallback (magenta) so a missing/typo'd theme key renders as
    /// an obvious "fix me" rather than silently invisible.
    pub const FALLBACK: Color = Color::rgba(1.0, 0.0, 1.0, 1.0);

    /// Parse a CSS-ish colour string: `#RGB`, `#RRGGBB`, `rgb(r,g,b)`,
    /// `rgba(r,g,b,a)`. Returns `None` on anything unrecognised so callers can
    /// decide on a fallback.
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            return parse_hex(hex);
        }
        if let Some(inner) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")) {
            let inner = inner.strip_suffix(')')?;
            let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
            if parts.len() != 3 && parts.len() != 4 {
                return None;
            }
            let r = parts[0].parse::<f32>().ok()? / 255.0;
            let g = parts[1].parse::<f32>().ok()? / 255.0;
            let b = parts[2].parse::<f32>().ok()? / 255.0;
            let a = if parts.len() == 4 {
                parts[3].parse::<f32>().ok()?
            } else {
                1.0
            };
            return Some(Color::rgba(r, g, b, a));
        }
        None
    }

    /// Parse, falling back to [`Color::FALLBACK`] on failure (logged-free; the
    /// magenta tells you visually). Used where the theme is trusted to carry
    /// a value but we never want to panic in the paint path.
    pub fn parse_or_fallback(s: &str) -> Color {
        Color::parse(s).unwrap_or(Color::FALLBACK)
    }

    /// Scale RGB lightness by `factor` (alpha unchanged). `0.65` produces the
    /// sphere-rim falloff per the theme's `base_color_modifier`.
    pub fn scale_lightness(self, factor: f32) -> Color {
        Color::rgba(
            (self.r * factor).clamp(0.0, 1.0),
            (self.g * factor).clamp(0.0, 1.0),
            (self.b * factor).clamp(0.0, 1.0),
            self.a,
        )
    }

    /// Lerp toward this colour's own grey (luminance) by `1.0 - keep`, i.e.
    /// `keep == 1.0` leaves it untouched and `keep == 0.0` fully desaturates.
    /// Drives the `constant` node-type `saturation_modifier`.
    pub fn desaturate(self, keep: f32) -> Color {
        let lum = 0.299 * self.r + 0.587 * self.g + 0.114 * self.b;
        let mix = |c: f32| lum + (c - lum) * keep.clamp(0.0, 1.0);
        Color::rgba(mix(self.r), mix(self.g), mix(self.b), self.a)
    }

    pub fn with_alpha(self, a: f32) -> Color {
        Color::rgba(self.r, self.g, self.b, a.clamp(0.0, 1.0))
    }
}

impl Default for Color {
    /// Transparent black — the neutral identity for `RenderOutput`'s derived
    /// `Default` (background fields are always overwritten by the renderer).
    fn default() -> Self {
        Color::rgba(0.0, 0.0, 0.0, 0.0)
    }
}

fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.trim();
    let to = |b: &str| u8::from_str_radix(b, 16).ok().map(|v| v as f32 / 255.0);
    match h.len() {
        3 => {
            // #RGB shorthand → duplicate each nibble.
            let r = to(&h[0..1].repeat(2))?;
            let g = to(&h[1..2].repeat(2))?;
            let b = to(&h[2..3].repeat(2))?;
            Some(Color::rgba(r, g, b, 1.0))
        }
        6 => {
            let r = to(&h[0..2])?;
            let g = to(&h[2..4])?;
            let b = to(&h[4..6])?;
            Some(Color::rgba(r, g, b, 1.0))
        }
        8 => {
            let r = to(&h[0..2])?;
            let g = to(&h[2..4])?;
            let b = to(&h[4..6])?;
            let a = to(&h[6..8])?;
            Some(Color::rgba(r, g, b, a))
        }
        _ => None,
    }
}

/// A light/dark colour pair (the recurring `{ light, dark }` YAML shape).
#[derive(Clone, Debug, Deserialize)]
pub struct ColorPair {
    pub light: String,
    pub dark: String,
}

impl ColorPair {
    /// Resolve to a [`Color`] for the active mode.
    pub fn resolve(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark { &self.dark } else { &self.light })
    }
}

/// `sphere.shading` — drives the radial-gradient sphere look.
#[derive(Clone, Debug, Deserialize)]
pub struct SphereShading {
    #[serde(default)]
    pub highlight_position: HighlightPosition,
    #[serde(default = "white")]
    pub highlight_color_light: String,
    #[serde(default = "white")]
    pub highlight_color_dark: String,
    #[serde(default = "default_base_modifier")]
    pub base_color_modifier: f32,
    #[serde(default = "default_specular")]
    pub specular_intensity: f32,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct HighlightPosition {
    pub x: f32,
    pub y: f32,
}

impl Default for HighlightPosition {
    fn default() -> Self {
        // Standard 3D-sphere top-left illumination.
        HighlightPosition { x: -0.25, y: -0.25 }
    }
}

/// `sphere.size_mapping`.
#[derive(Clone, Debug, Deserialize)]
pub struct SizeMapping {
    #[serde(default = "default_min_diameter")]
    pub min_diameter_px: f32,
    #[serde(default = "default_max_diameter")]
    pub max_diameter_px: f32,
    #[serde(default)]
    pub scaling_curve: String,
}

/// `sphere.border`.
#[derive(Clone, Debug, Deserialize)]
pub struct SphereBorder {
    #[serde(default = "default_border_width")]
    pub width_px: f32,
    pub color_light: String,
    pub color_dark: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SphereStyle {
    pub shading: SphereShading,
    pub size_mapping: SizeMapping,
    pub border: SphereBorder,
}

impl SphereStyle {
    pub fn highlight(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.shading.highlight_color_dark
        } else {
            &self.shading.highlight_color_light
        })
    }

    pub fn border_color(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.border.color_dark
        } else {
            &self.border.color_light
        })
    }
}

/// One `node_types.*` entry. Every field is optional — the seven kinds carry
/// different keys (only `module`/`constant` carry a size multiplier, only
/// `constant` carries a saturation modifier, etc.).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeTypeStyle {
    #[serde(default)]
    pub visual_treatment: Option<String>,
    #[serde(default)]
    pub relative_size_multiplier: Option<f32>,
    #[serde(default)]
    pub saturation_modifier: Option<f32>,
}

impl NodeTypeStyle {
    pub fn size_multiplier(&self) -> f32 {
        self.relative_size_multiplier.unwrap_or(1.0)
    }
}

/// One `edges.*` entry (per relationship type).
#[derive(Clone, Debug, Deserialize)]
pub struct EdgeStyle {
    #[serde(default)]
    pub line_style: String,
    pub color_light: String,
    pub color_dark: String,
    #[serde(default = "default_edge_thickness")]
    pub thickness_px: f32,
    /// `[dash_len, gap_len]` for `line_style: dashed` (theme: imports `[6, 4]`).
    #[serde(default)]
    pub dash_pattern: Option<Vec<f32>>,
    /// Gap between dots for `line_style: dotted` (theme: configures `3`).
    #[serde(default)]
    pub dot_spacing_px: Option<f32>,
}

impl EdgeStyle {
    pub fn color(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.color_dark
        } else {
            &self.color_light
        })
    }
}

/// `graph_panel.background`.
#[derive(Clone, Debug, Deserialize)]
pub struct PanelBackground {
    pub color_light: String,
    pub color_dark: String,
    #[serde(default)]
    pub secondary_color_light: Option<String>,
    #[serde(default)]
    pub secondary_color_dark: Option<String>,
}

impl PanelBackground {
    /// Primary (centre) background colour for the active mode.
    pub fn primary(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.color_dark
        } else {
            &self.color_light
        })
    }

    /// Secondary (edge) background colour — falls back to the primary when the
    /// theme omits it.
    pub fn secondary(&self, dark: bool) -> Color {
        let s = if dark {
            self.secondary_color_dark.as_deref()
        } else {
            self.secondary_color_light.as_deref()
        };
        s.and_then(Color::parse)
            .unwrap_or_else(|| self.primary(dark))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GraphPanelStyle {
    pub background: PanelBackground,
}

/// The locked visual style consumed by the renderer. Only the C-scaffold
/// sections are modelled; serde silently drops the rest.
#[derive(Clone, Debug, Deserialize)]
pub struct Theme {
    pub sphere: SphereStyle,
    pub language_colors: HashMap<String, ColorPair>,
    pub module_palette: Vec<ColorPair>,
    pub node_types: HashMap<String, NodeTypeStyle>,
    pub edges: HashMap<String, EdgeStyle>,
    pub graph_panel: GraphPanelStyle,
}

impl Theme {
    /// Parse the embedded Visual Style v1 YAML. `Err` only if the asset is
    /// malformed — which is a compile-time-fixed bug, not a runtime
    /// condition; the panel calls [`Theme::load_v1`] which logs + fails soft.
    pub fn from_embedded() -> Result<Theme, serde_yaml::Error> {
        serde_yaml::from_str(VISUAL_STYLE_V1_YAML)
    }

    /// Load the locked theme for the panel. The embedded asset is validated by
    /// the unit tests, so a parse failure here means the asset was edited and
    /// broken — we surface it via the returned `Result` and let the caller
    /// degrade rather than panicking in a render path.
    pub fn load_v1() -> Result<Theme, String> {
        Theme::from_embedded().map_err(|e| format!("visual_style_v1.yaml parse error: {e}"))
    }

    /// Resolve a node's language colour for the active mode. `lang` is a
    /// `language_colors` key (see `model::node::language_for_path`); falls
    /// back to a hashed module-palette hue when the language is unknown
    /// (file-less external nodes, unrecognised extensions) so every node still
    /// gets a stable, distinct colour.
    pub fn language_color(&self, lang: Option<&str>, fallback_seed: &str, dark: bool) -> Color {
        if let Some(pair) = lang.and_then(|l| self.language_colors.get(l)) {
            return pair.resolve(dark);
        }
        self.module_color(fallback_seed, dark)
    }

    /// A stable module-palette colour for `seed` (hash-indexed). Used for
    /// nodes without a recognised language.
    pub fn module_color(&self, seed: &str, dark: bool) -> Color {
        if self.module_palette.is_empty() {
            return Color::FALLBACK;
        }
        let idx = (fnv1a(seed) as usize) % self.module_palette.len();
        self.module_palette[idx].resolve(dark)
    }

    pub fn node_type(&self, key: &str) -> NodeTypeStyle {
        self.node_types.get(key).cloned().unwrap_or_default()
    }

    pub fn edge_style(&self, key: &str) -> Option<&EdgeStyle> {
        self.edges.get(key)
    }
}

/// Tiny FNV-1a hash for stable, deterministic palette indexing.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ── serde defaults (only used when the YAML omits a field) ──────────────
fn white() -> String {
    "#FFFFFF".to_owned()
}
fn default_base_modifier() -> f32 {
    0.65
}
fn default_specular() -> f32 {
    0.85
}
fn default_min_diameter() -> f32 {
    12.0
}
fn default_max_diameter() -> f32 {
    56.0
}
fn default_border_width() -> f32 {
    1.0
}
fn default_edge_thickness() -> f32 {
    1.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_theme_parses() {
        let t = Theme::load_v1().expect("locked visual_style_v1.yaml must parse");
        // Spot-check the sections C-scaffold renders are populated.
        assert!(!t.language_colors.is_empty());
        assert_eq!(t.module_palette.len(), 12, "12-colour module palette");
        assert!(t.node_types.contains_key("function"));
        assert!(t.node_types.contains_key("module"));
        assert!(t.edges.contains_key("calls"));
        assert!(t.edges.contains_key("inherits"));
    }

    #[test]
    fn sphere_shading_values_match_locked_spec() {
        let t = Theme::load_v1().unwrap();
        assert_eq!(t.sphere.shading.base_color_modifier, 0.65);
        assert_eq!(t.sphere.shading.specular_intensity, 0.85);
        assert_eq!(t.sphere.shading.highlight_position.x, -0.25);
        assert_eq!(t.sphere.shading.highlight_position.y, -0.25);
        assert_eq!(t.sphere.size_mapping.min_diameter_px, 12.0);
        assert_eq!(t.sphere.size_mapping.max_diameter_px, 56.0);
    }

    #[test]
    fn language_colors_resolve_light_and_dark() {
        let t = Theme::load_v1().unwrap();
        // Rust orange — distinct light vs dark.
        let dark = t.language_color(Some("rust"), "x", true);
        let light = t.language_color(Some("rust"), "x", false);
        assert_ne!(dark, light);
        // #E05A2B dark.
        assert!((dark.r - 0xE0 as f32 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn unknown_language_falls_back_to_stable_module_hue() {
        let t = Theme::load_v1().unwrap();
        let a = t.language_color(None, "seed-1", true);
        let b = t.language_color(None, "seed-1", true);
        assert_eq!(a, b, "same seed → same palette colour");
        assert_ne!(a, Color::FALLBACK, "module palette is populated");
    }

    #[test]
    fn node_type_multipliers_match_spec() {
        let t = Theme::load_v1().unwrap();
        assert_eq!(t.node_type("module").size_multiplier(), 1.4);
        assert_eq!(t.node_type("constant").size_multiplier(), 0.75);
        assert_eq!(t.node_type("constant").saturation_modifier, Some(0.4));
        // A kind without a multiplier defaults to 1.0.
        assert_eq!(t.node_type("function").size_multiplier(), 1.0);
    }

    #[test]
    fn edge_styles_resolve_per_rel_type() {
        let t = Theme::load_v1().unwrap();
        let inherits = t.edge_style("inherits").unwrap();
        assert_eq!(inherits.thickness_px, 2.0);
        assert_eq!(inherits.line_style, "solid");
        let imports = t.edge_style("imports").unwrap();
        assert_eq!(imports.line_style, "dashed");
    }

    #[test]
    fn panel_background_is_space_dark() {
        let t = Theme::load_v1().unwrap();
        let bg = t.graph_panel.background.primary(true);
        // #0B0E14 — deep space void.
        assert!(bg.r < 0.1 && bg.g < 0.1 && bg.b < 0.15);
        // Secondary present for the radial-gradient falloff.
        assert_ne!(
            t.graph_panel.background.secondary(true),
            t.graph_panel.background.primary(true)
        );
    }

    #[test]
    fn color_parse_handles_hex_and_rgba() {
        assert_eq!(
            Color::parse("#FFFFFF"),
            Some(Color::rgba(1.0, 1.0, 1.0, 1.0))
        );
        assert_eq!(Color::parse("#000"), Some(Color::rgba(0.0, 0.0, 0.0, 1.0)));
        let semi = Color::parse("rgba(255, 0, 0, 0.5)").unwrap();
        assert!((semi.r - 1.0).abs() < 1e-6 && (semi.a - 0.5).abs() < 1e-6);
        assert_eq!(Color::parse("rgb(0, 128, 255)").unwrap().a, 1.0);
        assert_eq!(Color::parse("not a color"), None);
    }

    #[test]
    fn scale_lightness_and_desaturate() {
        let c = Color::rgba(1.0, 0.0, 0.0, 1.0);
        let rim = c.scale_lightness(0.65);
        assert!((rim.r - 0.65).abs() < 1e-6 && rim.g == 0.0);
        // Full desaturate → grey (all channels equal the luminance).
        let grey = c.desaturate(0.0);
        assert!((grey.r - grey.g).abs() < 1e-6 && (grey.g - grey.b).abs() < 1e-6);
    }
}
