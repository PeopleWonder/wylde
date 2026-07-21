//! [`ChangelogView`] — the scrollable, lazy-paged changelog surface.
//!
//! Self-contained: it makes no backend calls, so it mounts and renders under
//! any condition (that's what lets the L7 panel-walk cover it cheaply). The
//! Shell wraps it in a dismissable scrim; on its own it is a complete card.

use gpui::{
    div, prelude::*, px, rgb, Context, FontWeight, IntoElement, MouseButton, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BRAND_LIGHT, SURFACE_800, SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::{bundled, parse_changelog, ChangelogSection};

/// How many version sections one "page" reveals. Small enough that the first
/// paint is a couple of versions, not the whole file; scrolling (or the
/// load-more affordance) reveals the next page on demand.
pub const PAGE_SIZE: usize = 3;

/// The release the pill is advertising — newer than this build, so its notes
/// are **not** in the bundled changelog. Supplied from the `UpdateInfo` the
/// updater already resolved, so surfacing it costs no new network call.
#[derive(Debug, Clone)]
pub struct HeadlineRelease {
    pub version: String,
    pub notes: String,
}

/// The changelog viewer. Owns the parsed sections (newest first) and how many
/// are currently materialised.
pub struct ChangelogView {
    sections: Vec<ChangelogSection>,
    loaded: usize,
    /// Set when there is nothing to show (unparseable / empty changelog and no
    /// headline); rendered as a calm message instead of a blank card.
    empty_message: Option<SharedString>,
}

impl ChangelogView {
    /// Build the viewer from the bundled changelog, optionally prepending the
    /// resolved-but-newer release as the top (newest) section.
    ///
    /// The headline is deduped against the bundled sections by version, so a
    /// release that has *also* landed in the bundled file (a rebuild after the
    /// tag) isn't shown twice.
    pub fn new(headline: Option<HeadlineRelease>, _cx: &mut Context<Self>) -> Self {
        Self::from_source(bundled(), headline)
    }

    /// Construction seam that takes the changelog text directly, so windowed
    /// tests can drive the viewer off a fixture instead of the embedded file.
    pub fn from_source(changelog_md: &str, headline: Option<HeadlineRelease>) -> Self {
        let mut sections = parse_changelog(changelog_md);

        if let Some(h) = headline {
            let version = h.version.trim().to_string();
            if !version.is_empty() {
                // Drop any bundled section for the same version — the headline
                // is the authoritative copy for the release being offered.
                sections.retain(|s| s.version != version);
                let body = if h.notes.trim().is_empty() {
                    "No release notes were provided for this version.".to_string()
                } else {
                    h.notes
                };
                sections.insert(
                    0,
                    ChangelogSection {
                        version: version.clone(),
                        heading: format!("{version} — available now"),
                        body,
                    },
                );
            }
        }

        let empty_message = sections
            .is_empty()
            .then(|| SharedString::from("No changelog is available."));
        let loaded = PAGE_SIZE.min(sections.len());

        Self {
            sections,
            loaded,
            empty_message,
        }
    }

    /// How many sections are currently materialised (rendered).
    pub fn loaded_count(&self) -> usize {
        self.loaded
    }

    /// Total sections available to page through.
    pub fn total_count(&self) -> usize {
        self.sections.len()
    }

    /// Whether more sections remain beyond the loaded page.
    pub fn has_more(&self) -> bool {
        self.loaded < self.sections.len()
    }

    /// Reveal the next page of older versions. Returns `true` if it actually
    /// grew (there was more to load), `false` at the end of the changelog.
    pub fn load_more(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.has_more() {
            return false;
        }
        self.loaded = grow(self.loaded, self.sections.len());
        cx.notify();
        true
    }

    fn scroll_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("wylde-changelog-scroll")
            .flex_1()
            .flex()
            .flex_col()
            .px_5()
            .py_4()
            .overflow_y_scroll()
            // Scrolling down (negative wheel delta) toward the bottom reveals
            // the next page — the "keep scrolling and it lazy-loads" behaviour.
            // The visible load-more row is the deterministic fallback.
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _w, cx| {
                let scrolling_down = match ev.delta {
                    ScrollDelta::Lines(p) => p.y < 0.0,
                    ScrollDelta::Pixels(p) => f32::from(p.y) < 0.0,
                };
                if scrolling_down {
                    this.load_more(cx);
                }
            }));

        if let Some(msg) = &self.empty_message {
            return body.child(empty_state(msg.clone()));
        }

        let shown = self.loaded.min(self.sections.len());
        for (i, sec) in self.sections[..shown].iter().enumerate() {
            if i > 0 {
                body = body.child(divider());
            }
            body = body.child(section_block(sec));
        }

        if self.has_more() {
            body = body.child(load_more_row(self.sections.len() - shown, cx));
        } else if shown > 0 {
            body = body.child(end_row());
        }
        body
    }
}

impl Render for ChangelogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(560.0))
            .max_h(px(620.0))
            .flex()
            .flex_col()
            .bg(rgb(pack(SURFACE_900)))
            .border_1()
            .border_color(rgb(pack(BORDER_DEFAULT)))
            .rounded(px(12.0))
            .overflow_hidden()
            .child(header())
            .child(self.scroll_body(cx))
    }
}

/// Grow `loaded` by one page without overshooting `total`. Pure so the paging
/// arithmetic is unit-tested without a window.
pub fn grow(loaded: usize, total: usize) -> usize {
    (loaded + PAGE_SIZE).min(total)
}

/// Card header — a fixed title strip above the scroll area.
fn header() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_5()
        .py_4()
        .border_b_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::LG))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from("What's new")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from("Newest first — scroll for older versions")),
        )
}

/// One version's block: its heading, then its body rendered line by line.
fn section_block(sec: &ChangelogSection) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .py_1()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::BASE))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .child(SharedString::from(sec.heading.clone())),
        )
        .child(render_body(&sec.body))
}

/// A light-touch markdown renderer for a changelog body: `### ` subheadings,
/// `- ` bullets, blank-line spacing, everything else a paragraph. Inline
/// emphasis markers are stripped for readability (this is intentionally not a
/// full markdown engine — a changelog viewer doesn't need one).
fn render_body(body: &str) -> gpui::Div {
    let mut col = div().flex().flex_col().gap_1();
    for raw in body.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            col = col.child(div().h(px(4.0)));
        } else if let Some(rest) = trimmed.strip_prefix("### ") {
            col = col.child(
                div()
                    .pt_1()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_SECONDARY)))
                    .child(SharedString::from(strip_inline(rest))),
            );
        } else if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(size::SM))
                            .text_color(rgb(pack(TEXT_MUTED)))
                            .child(SharedString::from("•")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::SM))
                            .text_color(rgb(pack(TEXT_SECONDARY)))
                            .child(SharedString::from(strip_inline(rest))),
                    ),
            );
        } else {
            col = col.child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .text_color(rgb(pack(TEXT_SECONDARY)))
                    .child(SharedString::from(strip_inline(trimmed))),
            );
        }
    }
    col
}

/// Divider line between two version sections.
fn divider() -> gpui::Div {
    div()
        .my_4()
        .h(px(1.0))
        .w_full()
        .bg(rgb(pack(BORDER_DEFAULT)))
}

/// Clickable "reveal the next page" row, shown while older versions remain.
fn load_more_row(remaining: usize, cx: &mut Context<ChangelogView>) -> gpui::Stateful<gpui::Div> {
    let label = if remaining == 1 {
        "Show 1 older version".to_string()
    } else {
        format!("Show older versions ▾  ({remaining} more)")
    };
    div()
        .id("wylde-changelog-load-more")
        .mt_4()
        .py_2()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(rgb(pack(SURFACE_800)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .hover(|s| s.bg(rgb(pack(BORDER_DEFAULT))))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _window, cx| {
                this.load_more(cx);
            }),
        )
        .child(SharedString::from(label))
}

/// End-of-changelog marker once every section is loaded.
fn end_row() -> gpui::Div {
    div()
        .mt_4()
        .py_2()
        .flex()
        .justify_center()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("— end of changelog —"))
}

/// Calm empty/error state.
fn empty_state(msg: SharedString) -> gpui::Div {
    div()
        .py_8()
        .flex()
        .justify_center()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(msg)
}

/// Strip the inline markdown emphasis a changelog uses (`**bold**`, backtick
/// code) so it reads cleanly as plain text.
fn strip_inline(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

/// Pack a theme `Rgba` into the `u32` gpui's `rgb()` wants (alpha dropped —
/// gpui composes opacity through its own builders). Mirrors the Shell's shim.
fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# Changelog

## [0.3.0] — 2026-08-01
- newest

## [0.2.0] — 2026-07-01
- middle

## [0.1.0] — 2026-06-01
- oldest

## [0.0.9] — 2026-05-01
- older still

## [0.0.8] — 2026-04-01
- ancient
";

    #[test]
    fn grow_advances_by_a_page_and_saturates() {
        assert_eq!(grow(0, 5), PAGE_SIZE.min(5));
        assert_eq!(grow(3, 5), 5); // 3 + 3 = 6, clamped to 5
        assert_eq!(grow(5, 5), 5); // already at the end
    }

    #[test]
    fn first_page_loads_only_page_size_of_five_sections() {
        let v = ChangelogView::from_source(FIXTURE, None);
        assert_eq!(v.total_count(), 5);
        assert_eq!(v.loaded_count(), PAGE_SIZE);
        assert!(v.has_more());
    }

    #[test]
    fn headline_is_prepended_newest_and_deduped() {
        let v = ChangelogView::from_source(
            FIXTURE,
            Some(HeadlineRelease {
                version: "0.4.0".into(),
                notes: "shiny".into(),
            }),
        );
        // 5 bundled + 1 headline, none deduped.
        assert_eq!(v.total_count(), 6);
        assert_eq!(v.sections[0].version, "0.4.0");
        assert_eq!(v.sections[0].body, "shiny");
    }

    #[test]
    fn headline_matching_a_bundled_version_replaces_not_duplicates() {
        let v = ChangelogView::from_source(
            FIXTURE,
            Some(HeadlineRelease {
                version: "0.3.0".into(),
                notes: "authoritative".into(),
            }),
        );
        // Still 5 total: the bundled 0.3.0 was replaced by the headline copy.
        assert_eq!(v.total_count(), 5);
        assert_eq!(v.sections[0].version, "0.3.0");
        assert_eq!(v.sections[0].body, "authoritative");
        assert_eq!(v.sections.iter().filter(|s| s.version == "0.3.0").count(), 1);
    }

    #[test]
    fn empty_changelog_with_no_headline_reports_empty() {
        let v = ChangelogView::from_source("# Changelog\n\njust preamble", None);
        assert_eq!(v.total_count(), 0);
        assert!(v.empty_message.is_some());
    }

    #[test]
    fn empty_changelog_still_shows_the_headline() {
        // Offline / unparseable bundled file must not swallow the release the
        // pill is advertising.
        let v = ChangelogView::from_source(
            "",
            Some(HeadlineRelease {
                version: "0.4.0".into(),
                notes: "".into(),
            }),
        );
        assert_eq!(v.total_count(), 1);
        assert!(v.empty_message.is_none());
        assert!(v.sections[0].body.contains("No release notes"));
    }
}
