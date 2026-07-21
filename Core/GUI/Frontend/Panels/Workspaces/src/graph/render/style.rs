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

/// `graph_panel.breadcrumb_bar` (C-navigation) — the scope-trail strip across
/// the top of the graph canvas. Defaults equal the locked Visual Style v1
/// values so an older asset (or a load failure) still renders to spec.
#[derive(Clone, Debug, Deserialize)]
pub struct BreadcrumbBarStyle {
    #[serde(default = "white")]
    pub background_light: String,
    #[serde(default = "default_crumb_bg_dark")]
    pub background_dark: String,
    #[serde(default = "default_crumb_text_light")]
    pub text_light: String,
    #[serde(default = "default_crumb_text_dark")]
    pub text_dark: String,
    #[serde(default = "default_separator_glyph")]
    pub separator_glyph: String,
    #[serde(default = "default_crumb_height")]
    pub height_px: f32,
    #[serde(default = "default_crumb_font")]
    pub font_size_px: f32,
}

impl Default for BreadcrumbBarStyle {
    fn default() -> Self {
        // The locked spec values — used only when the YAML omits the section.
        serde_yaml::from_str("{}").expect("empty mapping fills serde defaults")  // INVARIANT: parses the literal "{}" to fill serde defaults — cannot fail. wylde-check: panel-panic-allowed
    }
}

impl BreadcrumbBarStyle {
    pub fn background(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.background_dark
        } else {
            &self.background_light
        })
    }

    pub fn text(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.text_dark
        } else {
            &self.text_light
        })
    }
}

/// `graph_panel.exit_edges` (C-navigation) — how edges that leave the scoped
/// cluster fade out at the boundary, and the destination-label chip styling.
#[derive(Clone, Debug, Deserialize)]
pub struct ExitEdgeStyle {
    #[serde(default = "default_fade_distance")]
    pub fade_distance_px: f32,
    #[serde(default = "default_exit_label_bg_light")]
    pub label_background_light: String,
    #[serde(default = "default_exit_label_bg_dark")]
    pub label_background_dark: String,
    #[serde(default = "default_exit_label_text_light")]
    pub label_text_light: String,
    #[serde(default = "default_exit_label_text_dark")]
    pub label_text_dark: String,
    #[serde(default = "default_exit_label_font")]
    pub label_font_size_px: f32,
}

impl Default for ExitEdgeStyle {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("empty mapping fills serde defaults")  // INVARIANT: parses the literal "{}" to fill serde defaults — cannot fail. wylde-check: panel-panic-allowed
    }
}

impl ExitEdgeStyle {
    pub fn label_background(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.label_background_dark
        } else {
            &self.label_background_light
        })
    }

    pub fn label_text(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.label_text_dark
        } else {
            &self.label_text_light
        })
    }
}

/// `graph_panel.cluster_boundary` (C-cluster) — the faint outline drawn
/// around an expanded-in-place cluster's members.
#[derive(Clone, Debug, Deserialize)]
pub struct ClusterBoundaryStyle {
    #[serde(default)]
    pub style: String,
    #[serde(default = "default_boundary_border_light")]
    pub border_color_light: String,
    #[serde(default = "default_boundary_border_dark")]
    pub border_color_dark: String,
    #[serde(default = "default_border_width")]
    pub border_width_px: f32,
    #[serde(default = "default_boundary_radius")]
    pub corner_radius_px: f32,
    #[serde(default = "default_boundary_fill_light")]
    pub fill_color_light: String,
    #[serde(default = "default_boundary_fill_dark")]
    pub fill_color_dark: String,
    #[serde(default = "default_fill_opacity")]
    pub fill_opacity: f32,
}

impl Default for ClusterBoundaryStyle {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("empty mapping fills serde defaults")  // INVARIANT: parses the literal "{}" to fill serde defaults — cannot fail. wylde-check: panel-panic-allowed
    }
}

impl ClusterBoundaryStyle {
    pub fn border(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.border_color_dark
        } else {
            &self.border_color_light
        })
    }

    pub fn fill(&self, dark: bool) -> Color {
        let c = Color::parse_or_fallback(if dark {
            &self.fill_color_dark
        } else {
            &self.fill_color_light
        });
        c.with_alpha(c.a * self.fill_opacity.clamp(0.0, 1.0))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GraphPanelStyle {
    pub background: PanelBackground,
    /// Breadcrumb bar styling (C-navigation). `default` so pre-C-navigation
    /// assets still parse; the defaults equal the locked spec.
    #[serde(default)]
    pub breadcrumb_bar: BreadcrumbBarStyle,
    /// Exit-edge fade + label styling (C-navigation).
    #[serde(default)]
    pub exit_edges: ExitEdgeStyle,
    /// Expanded-cluster boundary outline (C-cluster).
    #[serde(default)]
    pub cluster_boundary: ClusterBoundaryStyle,
}

/// `ui_chrome.context_menu` — the right-click menu chrome (C-cluster uses it
/// for Expand/Collapse Cluster; Phase 4 surfaces reuse it). Only the fields
/// the graph panel consumes are modelled.
#[derive(Clone, Debug, Deserialize)]
pub struct ContextMenuStyle {
    #[serde(default = "white")]
    pub background_light: String,
    #[serde(default = "default_crumb_bg_dark")]
    pub background_dark: String,
    #[serde(default = "default_menu_radius")]
    pub border_radius_px: f32,
    #[serde(default = "default_menu_item_height")]
    pub item_height_px: f32,
    #[serde(default = "default_menu_item_padding")]
    pub item_padding_px: f32,
    #[serde(default = "default_menu_hover_light")]
    pub item_hover_background_light: String,
    #[serde(default = "default_menu_hover_dark")]
    pub item_hover_background_dark: String,
    #[serde(default = "default_crumb_font")]
    pub font_size_px: f32,
}

impl Default for ContextMenuStyle {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("empty mapping fills serde defaults")  // INVARIANT: parses the literal "{}" to fill serde defaults — cannot fail. wylde-check: panel-panic-allowed
    }
}

impl ContextMenuStyle {
    pub fn background(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.background_dark
        } else {
            &self.background_light
        })
    }

    pub fn item_hover(&self, dark: bool) -> Color {
        Color::parse_or_fallback(if dark {
            &self.item_hover_background_dark
        } else {
            &self.item_hover_background_light
        })
    }
}

/// `ui_chrome.*` — shared UI chrome the graph panel consumes.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UiChromeStyle {
    #[serde(default)]
    pub context_menu: ContextMenuStyle,
}

/// One `animations.*` entry — a duration + cubic-bézier easing. Only the
/// fields C-layout needs are modelled; the YAML's `description` is ignored.
/// `easing` is `[x1, y1, x2, y2]` (CSS cubic-bezier control points).
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct AnimationSpec {
    #[serde(default)]
    pub duration_ms: f32,
    #[serde(default)]
    pub easing: [f32; 4],
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
    /// Animation durations + easings (`animations.*`). C-layout reads
    /// `graph_layout_swap`; absent in older assets → empty map (callers fall
    /// back to the locked spec value).
    #[serde(default)]
    pub animations: HashMap<String, AnimationSpec>,
    /// Shared UI chrome (`ui_chrome.*`); the graph panel reads
    /// `context_menu` (C-cluster).
    #[serde(default)]
    pub ui_chrome: UiChromeStyle,
}

/// Env var naming the on-disk Visual Style YAML for **dev theme
/// hot-reload**. Honoured only in debug builds (`cfg!(debug_assertions)`);
/// release builds always parse the embedded asset — the `include_str!`
/// path is byte-for-byte what shipped.
pub const THEME_PATH_ENV: &str = "WYLDE_THEME_PATH";

/// Dev-only: the on-disk YAML to hot-reload from. `None` in release
/// builds, when the env var is unset/blank, or when the file is
/// unreadable — every miss falls back to the embedded asset, so a broken
/// dev setup degrades to exactly the shipped behaviour.
fn dev_theme_yaml() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let path = std::env::var(THEME_PATH_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())?;
    std::fs::read_to_string(path).ok()
}

/// Dev-only: the watched YAML's mtime, when hot-reload is active. The
/// graph panel polls this to know when to re-parse + repaint (no `notify`
/// dependency — a 500 ms mtime poll is plenty for a human-in-the-loop
/// tweak cycle).
pub fn dev_theme_mtime() -> Option<std::time::SystemTime> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let path = std::env::var(THEME_PATH_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())?;
    std::fs::metadata(path).ok()?.modified().ok()
}

impl Theme {
    /// Parse the embedded Visual Style v1 YAML. `Err` only if the asset is
    /// malformed — which is a compile-time-fixed bug, not a runtime
    /// condition; the panel calls [`Theme::load_v1`] which logs + fails soft.
    pub fn from_embedded() -> Result<Theme, serde_yaml::Error> {
        Theme::from_yaml(VISUAL_STYLE_V1_YAML)
    }

    /// Parse any Visual Style YAML string — the embedded asset and the dev
    /// hot-reload path share this one parser, so they can never drift.
    pub fn from_yaml(yaml: &str) -> Result<Theme, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    /// Load the theme for the panel. Release builds (and dev builds without
    /// [`THEME_PATH_ENV`]) parse the embedded locked asset, exactly as
    /// before. Dev builds with the env set read the YAML **from disk**, so
    /// a saved tweak re-applies live (the panel's hot-reload poll calls
    /// back in here); a disk parse error logs and falls back to embedded —
    /// a typo mid-edit never blanks the graph.
    pub fn load_v1() -> Result<Theme, String> {
        if let Some(yaml) = dev_theme_yaml() {
            match Theme::from_yaml(&yaml) {
                Ok(t) => return Ok(t),
                Err(e) => {
                    eprintln!(
                        "[theme-hot] on-disk YAML parse error (falling back to embedded): {e}"
                    );
                }
            }
        }
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

    /// Look up an `animations.*` spec by key (e.g. `"graph_layout_swap"`).
    pub fn animation(&self, key: &str) -> Option<&AnimationSpec> {
        self.animations.get(key)
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
fn default_crumb_bg_dark() -> String {
    "#141B24".to_owned()
}
fn default_crumb_text_light() -> String {
    "#2D3748".to_owned()
}
fn default_crumb_text_dark() -> String {
    "#E2E8F0".to_owned()
}
fn default_separator_glyph() -> String {
    "›".to_owned()
}
fn default_crumb_height() -> f32 {
    36.0
}
fn default_crumb_font() -> f32 {
    12.0
}
fn default_fade_distance() -> f32 {
    40.0
}
fn default_exit_label_bg_light() -> String {
    "#EDF2F7".to_owned()
}
fn default_exit_label_bg_dark() -> String {
    "#1A202C".to_owned()
}
fn default_exit_label_text_light() -> String {
    "#4A5568".to_owned()
}
fn default_exit_label_text_dark() -> String {
    "#E2E8F0".to_owned()
}
fn default_exit_label_font() -> f32 {
    11.0
}
fn default_boundary_border_light() -> String {
    "rgba(0, 0, 0, 0.08)".to_owned()
}
fn default_boundary_border_dark() -> String {
    "rgba(255, 255, 255, 0.08)".to_owned()
}
fn default_boundary_radius() -> f32 {
    12.0
}
fn default_boundary_fill_light() -> String {
    "rgba(0, 0, 0, 0.01)".to_owned()
}
fn default_boundary_fill_dark() -> String {
    "rgba(255, 255, 255, 0.01)".to_owned()
}
fn default_fill_opacity() -> f32 {
    1.0
}
fn default_menu_radius() -> f32 {
    8.0
}
fn default_menu_item_height() -> f32 {
    28.0
}
fn default_menu_item_padding() -> f32 {
    10.0
}
fn default_menu_hover_light() -> String {
    "#EDF2F7".to_owned()
}
fn default_menu_hover_dark() -> String {
    "#2D3748".to_owned()
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
    fn from_yaml_applies_a_tweaked_value_and_falls_back_on_garbage() {
        // The dev hot-reload contract: the SAME parser handles disk YAML,
        // so a saved tweak lands exactly as the embedded asset would.
        let tweaked = VISUAL_STYLE_V1_YAML.replacen(
            "module_palette:",
            "module_palette_tweak_marker: 1\nmodule_palette:",
            1,
        );
        assert!(Theme::from_yaml(&tweaked).is_ok(), "unknown keys ignored");
        // A real value change parses and is visible (the related_to edge's
        // light colour — the `color_light:` form is unique to that key).
        let recolored = VISUAL_STYLE_V1_YAML.replacen(
            "color_light: \"#B83280\"",
            "color_light: \"#112233\"",
            1,
        );
        assert_ne!(recolored, VISUAL_STYLE_V1_YAML, "fixture changed something");
        let t = Theme::from_yaml(&recolored).expect("tweaked YAML parses");
        let edge = t.edges.get("related_to").expect("related_to edge");
        assert_eq!(
            edge.color(false),
            Color::parse("#112233").unwrap(),
            "the disk tweak is what renders"
        );
        // Mid-edit garbage is an Err the loader falls back from — never a
        // panic in the paint path.
        assert!(Theme::from_yaml("nodes: [unterminated").is_err());
    }

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
    fn graph_layout_swap_animation_matches_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let swap = t.animation("graph_layout_swap").expect("locked swap anim");
        assert_eq!(swap.duration_ms, 500.0);
        assert_eq!(swap.easing, [0.77, 0.0, 0.175, 1.0]);
        assert!(t.animation("nope").is_none());
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
    fn breadcrumb_bar_matches_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let bb = &t.graph_panel.breadcrumb_bar;
        assert_eq!(bb.height_px, 36.0);
        assert_eq!(bb.font_size_px, 12.0);
        assert_eq!(bb.separator_glyph, "›");
        // #141B24 dark background, #E2E8F0 dark text.
        let bg = bb.background(true);
        assert!((bg.r - 0x14 as f32 / 255.0).abs() < 1e-3);
        let fg = bb.text(true);
        assert!((fg.r - 0xE2 as f32 / 255.0).abs() < 1e-3);
        assert_ne!(bb.background(true), bb.background(false));
    }

    #[test]
    fn exit_edges_match_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let xe = &t.graph_panel.exit_edges;
        assert_eq!(xe.fade_distance_px, 40.0);
        assert_eq!(xe.label_font_size_px, 11.0);
        // #1A202C dark chip, #E2E8F0 dark label text.
        let bg = xe.label_background(true);
        assert!((bg.r - 0x1A as f32 / 255.0).abs() < 1e-3);
        assert_ne!(xe.label_text(true), xe.label_text(false));
    }

    #[test]
    fn nav_section_defaults_equal_locked_spec_when_yaml_omits_them() {
        // Pre-C-navigation asset compatibility: an empty mapping yields the
        // locked values, so a stale asset still renders to spec.
        let bb = BreadcrumbBarStyle::default();
        assert_eq!(bb.height_px, 36.0);
        assert_eq!(bb.separator_glyph, "›");
        let xe = ExitEdgeStyle::default();
        assert_eq!(xe.fade_distance_px, 40.0);
        assert_eq!(xe.label_font_size_px, 11.0);
    }

    #[test]
    fn cluster_boundary_matches_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let cb = &t.graph_panel.cluster_boundary;
        assert_eq!(cb.style, "faint_outline");
        assert_eq!(cb.border_width_px, 1.0);
        assert_eq!(cb.corner_radius_px, 12.0);
        let b = cb.border(true);
        assert!(
            (b.a - 0.08).abs() < 1e-3,
            "faint white outline in dark mode"
        );
        let f = cb.fill(true);
        assert!((f.a - 0.01).abs() < 1e-3);
    }

    #[test]
    fn context_menu_matches_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let m = &t.ui_chrome.context_menu;
        assert_eq!(m.border_radius_px, 8.0);
        assert_eq!(m.item_height_px, 28.0);
        assert_eq!(m.item_padding_px, 10.0);
        assert_eq!(m.font_size_px, 12.0);
        // #141B24 dark background.
        let bg = m.background(true);
        assert!((bg.r - 0x14 as f32 / 255.0).abs() < 1e-3);
        assert_ne!(m.item_hover(true), m.item_hover(false));
    }

    #[test]
    fn cluster_expand_animation_matches_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let a = t.animation("cluster_expand_in_place").expect("locked anim");
        assert_eq!(a.duration_ms, 300.0);
        assert_eq!(a.easing, [0.16, 1.0, 0.3, 1.0]);
    }

    #[test]
    fn zoom_animations_match_locked_spec() {
        let t = Theme::load_v1().unwrap();
        let zin = t.animation("graph_zoom_into_cluster").expect("locked anim");
        assert_eq!(zin.duration_ms, 400.0);
        assert_eq!(zin.easing, [0.645, 0.045, 0.355, 1.0]);
        let zout = t.animation("graph_zoom_out").expect("locked anim");
        assert_eq!(zout.duration_ms, 380.0);
        assert_eq!(zout.easing, [0.645, 0.045, 0.355, 1.0]);
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
