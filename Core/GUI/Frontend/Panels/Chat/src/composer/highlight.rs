//! Highlight model + composer theme (Slice F, Build Order §5: "IDE-style
//! underline + accent color + tooltip").
//!
//! [`spans_for`] maps recognition results to [`HighlightSpan`]s — byte
//! ranges with a state-coloured wavy underline (Theme
//! `chat_composer.highlight_underline`: 2 px wavy) and a hover tooltip.
//! The glyph-metrics pass closed the Slice F deferral: [`input_spans`]
//! feeds these straight into the shared `TextInput`'s decoration-run API
//! (`set_highlights`), so the squiggle now paints INSIDE the input,
//! wrap-aware, under the exact glyphs. The chip strip stays — it carries
//! the click affordances the underline doesn't.
//!
//! The `chat_composer` Theme section is parsed here from the same locked
//! Visual Style v1 YAML the graph panel embeds (one asset on disk, two
//! compile-time consumers — the Theme struct itself canonically lives in the
//! Workspaces panel per Appendix B, and the Chat panel deliberately doesn't
//! link that whole crate for a styling struct).

use serde::Deserialize;

use super::WordRecognition;

/// The locked Visual Style v1 (in-repo mirror — single source of truth is
/// the Nextcloud page; re-sync in the same commit when it changes).
const VISUAL_STYLE_V1_YAML: &str = include_str!("../../../Workspaces/assets/visual_style_v1.yaml");

/// A recognized span's visual state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightState {
    /// Recognized, unambiguous.
    Recognized,
    /// Multiple candidates — the soft-alert (`ambiguous_state`) tint.
    Ambiguous,
    /// Curated out of the send — rendered muted.
    Excluded,
}

/// One IDE-style highlight: a byte range in the composer text plus its
/// state and tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub state: HighlightState,
    pub tooltip: String,
}

/// Build highlight spans for the recognized words (unrecognized words get
/// nothing — prose stays prose).
pub fn spans_for(words: &[WordRecognition]) -> Vec<HighlightSpan> {
    words
        .iter()
        .filter(|w| w.is_recognized())
        .map(|w| {
            let state = if w.excluded {
                HighlightState::Excluded
            } else if w.is_ambiguous() {
                HighlightState::Ambiguous
            } else {
                HighlightState::Recognized
            };
            HighlightSpan {
                start: w.token.start,
                end: w.token.end,
                state,
                tooltip: tooltip(w),
            }
        })
        .collect()
}

/// Map the recognition state to the shared input's styled spans — the
/// in-input wavy underline (glyph-metrics pass). Colours are the locked
/// `chat_composer` palette's dark variants (the composer's surfaces are
/// dark, matching the chips): recognized = the tether accent, ambiguous =
/// the soft-alert tint, excluded = the muted chip text.
pub fn input_spans(words: &[WordRecognition]) -> Vec<wylde_gpui_input::HighlightSpan> {
    let t = composer_theme();
    let wavy = t.highlight_underline.style == "wavy";
    let thickness = t.highlight_underline.thickness_px;
    spans_for(words)
        .into_iter()
        .map(|s| {
            let color = gpui::rgb(match s.state {
                HighlightState::Recognized => hex(&t.tether_line.color_dark),
                HighlightState::Ambiguous => hex(&t.per_word_chip.ambiguous_state.background_dark),
                HighlightState::Excluded => hex(&t.per_word_chip.text_dark),
            });
            wylde_gpui_input::HighlightSpan {
                range: s.start..s.end,
                color: None,
                background: None,
                underline: Some(wylde_gpui_input::UnderlineSpec {
                    color,
                    thickness,
                    wavy,
                }),
            }
        })
        .collect()
}

fn tooltip(w: &WordRecognition) -> String {
    if w.excluded {
        return format!("{} — excluded from this message", w.token.text);
    }
    if w.is_ambiguous() {
        return format!(
            "{} — {} possible symbols, click to disambiguate",
            w.token.text,
            w.candidates.len()
        );
    }
    let mut parts = Vec::new();
    if let Some(s) = w.effective_symbol() {
        parts.push(format!("{} · {}:{}", s.name, s.file, s.line));
    }
    if w.anchor_count > 0 {
        parts.push(format!("{} anchor(s)", w.anchor_count));
    }
    parts.join(" · ")
}

// ── chat_composer theme ──────────────────────────────────────────────────

/// `chat_composer.per_word_chip` (+ its `ambiguous_state`) and
/// `highlight_underline` — the slice's consumed sections; serde ignores the
/// rest of the YAML. Defaults equal the locked spec so a stale asset still
/// renders correctly.
#[derive(Clone, Debug, Deserialize)]
pub struct ComposerTheme {
    #[serde(default)]
    pub highlight_underline: UnderlineStyle,
    #[serde(default)]
    pub per_word_chip: PerWordChip,
    /// `chat_composer.tether_line` — the recognition accent (consumed by
    /// the in-input underline now; the bubble layer's tethers next).
    #[serde(default)]
    pub tether_line: TetherLine,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UnderlineStyle {
    #[serde(default = "d_underline_px")]
    pub thickness_px: f32,
    #[serde(default = "d_underline_style")]
    pub style: String,
}

impl Default for UnderlineStyle {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TetherLine {
    #[serde(default = "d_tether_light")]
    pub color_light: String,
    #[serde(default = "d_tether_dark")]
    pub color_dark: String,
    #[serde(default = "d_tether_px")]
    pub thickness_px: f32,
    #[serde(default = "d_tether_opacity")]
    pub opacity: f32,
    #[serde(default = "d_tether_dash")]
    pub dash_pattern: Vec<f32>,
}

impl Default for TetherLine {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PerWordChip {
    #[serde(default = "d_chip_height")]
    pub height_px: f32,
    #[serde(default = "d_chip_pad")]
    pub horizontal_padding_px: f32,
    #[serde(default = "d_chip_radius")]
    pub border_radius_px: f32,
    #[serde(default = "d_chip_bg_light")]
    pub background_light: String,
    #[serde(default = "d_chip_bg_dark")]
    pub background_dark: String,
    #[serde(default = "d_chip_text_light")]
    pub text_light: String,
    #[serde(default = "d_chip_text_dark")]
    pub text_dark: String,
    #[serde(default = "d_chip_font")]
    pub font_size_px: f32,
    #[serde(default)]
    pub ambiguous_state: AmbiguousChip,
}

impl Default for PerWordChip {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct AmbiguousChip {
    #[serde(default = "d_ambig_bg_light")]
    pub background_light: String,
    #[serde(default = "d_ambig_bg_dark")]
    pub background_dark: String,
    #[serde(default = "d_ambig_text_light")]
    pub text_light: String,
    #[serde(default = "d_ambig_text_dark")]
    pub text_dark: String,
    #[serde(default = "d_ambig_prefix")]
    pub icon_prefix: String,
}

impl Default for AmbiguousChip {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults")
    }
}

/// Parse the embedded YAML's `chat_composer` section. Failure (an edited,
/// broken asset) degrades to the locked defaults — composer styling never
/// panics a render.
pub fn composer_theme() -> ComposerTheme {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        chat_composer: Option<ComposerTheme>,
    }
    serde_yaml::from_str::<Root>(VISUAL_STYLE_V1_YAML)
        .ok()
        .and_then(|r| r.chat_composer)
        .unwrap_or_else(|| ComposerTheme {
            highlight_underline: UnderlineStyle::default(),
            per_word_chip: PerWordChip::default(),
            tether_line: TetherLine::default(),
        })
}

/// Resolve a `#RRGGBB` string to a gpui rgb value, magenta on parse failure
/// (an obvious "fix me", matching the graph panel's convention).
pub fn hex(s: &str) -> u32 {
    let h = s.trim().trim_start_matches('#');
    u32::from_str_radix(h, 16).unwrap_or(0xFF00FF)
}

// serde defaults — the locked Visual Style v1 values.
fn d_underline_px() -> f32 {
    2.0
}
fn d_underline_style() -> String {
    "wavy".to_owned()
}
fn d_chip_height() -> f32 {
    18.0
}
fn d_chip_pad() -> f32 {
    6.0
}
fn d_chip_radius() -> f32 {
    4.0
}
fn d_chip_bg_light() -> String {
    "#EDF2F7".to_owned()
}
fn d_chip_bg_dark() -> String {
    "#2D3748".to_owned()
}
fn d_chip_text_light() -> String {
    "#4A5568".to_owned()
}
fn d_chip_text_dark() -> String {
    "#CBD5E0".to_owned()
}
fn d_chip_font() -> f32 {
    10.0
}
fn d_ambig_bg_light() -> String {
    "#FEFCBF".to_owned()
}
fn d_ambig_bg_dark() -> String {
    "#744210".to_owned()
}
fn d_ambig_text_light() -> String {
    "#744210".to_owned()
}
fn d_ambig_text_dark() -> String {
    "#FEFCBF".to_owned()
}
fn d_ambig_prefix() -> String {
    "?".to_owned()
}
fn d_tether_light() -> String {
    "#3182CE".to_owned()
}
fn d_tether_dark() -> String {
    "#4FD1C5".to_owned()
}
fn d_tether_px() -> f32 {
    1.0
}
fn d_tether_opacity() -> f32 {
    0.4
}
fn d_tether_dash() -> Vec<f32> {
    vec![4.0, 4.0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::{SymbolCandidate, TokenKind, TokenSpan};

    fn word(text: &str, candidates: usize, anchors: usize) -> WordRecognition {
        let mut w = WordRecognition::new(TokenSpan {
            text: text.to_owned(),
            start: 4,
            end: 4 + text.len(),
            kind: TokenKind::Identifier,
        });
        w.candidates = (0..candidates)
            .map(|i| SymbolCandidate {
                id: format!("{text}-{i}"),
                name: text.to_owned(),
                kind: "Function".to_owned(),
                file: format!("src/{text}.rs"),
                line: 12,
                module_path: String::new(),
                score: 1.0,
            })
            .collect();
        w.anchor_count = anchors;
        w
    }

    #[test]
    fn spans_carry_state_range_and_tooltip() {
        let mut excluded = word("gone", 1, 0);
        excluded.excluded = true;
        let words = vec![
            word("plain", 0, 0),
            word("sym", 1, 1),
            word("ambig", 3, 0),
            excluded,
        ];
        let spans = spans_for(&words);
        assert_eq!(spans.len(), 3, "prose word gets no span");
        assert_eq!(spans[0].state, HighlightState::Recognized);
        assert_eq!((spans[0].start, spans[0].end), (4, 7));
        assert!(spans[0].tooltip.contains("src/sym.rs:12"));
        assert!(spans[0].tooltip.contains("1 anchor"));
        assert_eq!(spans[1].state, HighlightState::Ambiguous);
        assert!(spans[1].tooltip.contains("3 possible symbols"));
        assert_eq!(spans[2].state, HighlightState::Excluded);
        assert!(spans[2].tooltip.contains("excluded"));
    }

    #[test]
    fn input_spans_carry_themed_wavy_underlines() {
        let words = vec![word("sym", 1, 0), word("ambig", 3, 0)];
        let spans = input_spans(&words);
        assert_eq!(spans.len(), 2);
        for s in &spans {
            let u = s.underline.expect("underlined");
            assert!(u.wavy, "locked spec: wavy");
            assert_eq!(u.thickness, 2.0, "locked spec: 2px");
            assert!(s.color.is_none() && s.background.is_none());
        }
        assert_eq!((spans[0].range.start, spans[0].range.end), (4, 7));
        // Recognized rides the tether accent; ambiguous the alert tint.
        assert_ne!(
            spans[0].underline.unwrap().color,
            spans[1].underline.unwrap().color
        );
    }

    #[test]
    fn embedded_theme_section_matches_locked_spec() {
        let t = composer_theme();
        assert_eq!(t.highlight_underline.thickness_px, 2.0);
        assert_eq!(t.highlight_underline.style, "wavy");
        assert_eq!(hex(&t.tether_line.color_dark), 0x4FD1C5);
        assert_eq!(hex(&t.tether_line.color_light), 0x3182CE);
        assert_eq!(t.tether_line.thickness_px, 1.0);
        let chip = &t.per_word_chip;
        assert_eq!(chip.height_px, 18.0);
        assert_eq!(chip.horizontal_padding_px, 6.0);
        assert_eq!(chip.border_radius_px, 4.0);
        assert_eq!(chip.font_size_px, 10.0);
        assert_eq!(hex(&chip.background_dark), 0x2D3748);
        assert_eq!(chip.ambiguous_state.icon_prefix, "?");
        assert_eq!(hex(&chip.ambiguous_state.background_dark), 0x744210);
    }

    #[test]
    fn hex_parses_and_screams_on_garbage() {
        assert_eq!(hex("#EDF2F7"), 0xEDF2F7);
        assert_eq!(hex("not-a-color"), 0xFF00FF);
    }
}
