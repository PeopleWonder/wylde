//! wylde-changelog — the lazy-loaded changelog viewer behind the shell's
//! bottom-left update pill (#196).
//!
//! ## What it shows
//!
//! An ongoing changelog, **newest version first**, each version's notes
//! separated from the next by a divider line, rendered a page at a time so
//! "theoretically the whole changelog" can be scrolled without materialising
//! thousands of gpui elements up front ([`ChangelogView`] owns the paging).
//!
//! ## Where the data comes from — and the privacy posture
//!
//! The **bundled** [`bundled`] `CHANGELOG.md` (embedded with `include_str!`)
//! is the source: **zero network calls**, fully local, and it already carries
//! the complete history up to the shipped build. That is the deliberate,
//! privacy-first choice — Wylde is all-local, and a changelog viewer must not
//! be the one surface that quietly phones home.
//!
//! The one thing the bundled file *cannot* contain is the notes for a release
//! that is **newer than this build** — which is exactly the release the pill is
//! advertising. Those ride in on [`HeadlineRelease`], built from the
//! `UpdateInfo` the updater's opted-in startup check **already fetched**. So the
//! newest section is shown with no *new* outbound request: the network call, if
//! any, happened earlier under the user's automatic-check consent, not on open.
//!
//! The alternative — paginating the GitHub Releases API on scroll — would extend
//! beyond the build but requires a consent-gated fetch each time; it is
//! deliberately **not** taken here (see the crate's issue, #196).
//!
//! ## Also here: the pill-visibility policy
//!
//! [`pill_visible`] is the pure gate for the bottom-left update **pill** that
//! opens this viewer. It lives in this crate (not the pipe crate) on purpose:
//! the GUI workspace's CI runs test *targets* only for the panel-walk crate set,
//! so hosting the pill's re-appear-on-newer guarantee here is what makes it an
//! actually-executed gate rather than a compiled-but-unrun test.

mod view;

pub use view::{ChangelogView, HeadlineRelease, PAGE_SIZE};

/// Whether the bottom-left update **pill** (#196) should be visible this frame.
///
/// The pill mirrors the sidebar dot's gate — it only exists when a check has
/// resolved an update (`update_available`) — but adds a per-version dismissal:
/// once the user clicks "Ignore" on version *V*, the pill hides for *V*. The
/// dismissal is keyed on the exact version string, so when a **newer** release
/// *V'* is later resolved (`available_version != dismissed_version`) the pill
/// **re-appears**. That is the whole point of the guarantee: "Ignore" silences
/// *this* update, never all future ones.
///
/// Pure over its inputs so the re-appear-on-newer behaviour is unit-tested
/// without a window, the pipe, or the network — and, being in a panel-walk
/// crate, it is actually *run* in CI (the pipe workspace's own tests are not).
pub fn pill_visible(
    update_available: bool,
    available_version: Option<&str>,
    dismissed_version: Option<&str>,
) -> bool {
    if !update_available {
        return false;
    }
    match (available_version, dismissed_version) {
        // Dismissed the exact version being offered ⇒ hide. Any other pairing —
        // a newer available version, or no dismissal recorded — shows.
        (Some(available), Some(dismissed)) => available != dismissed,
        _ => true,
    }
}

/// One version's changelog entry: the version string, the raw heading line,
/// and the markdown body between this `## ` heading and the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogSection {
    /// The version token pulled from the heading — `0.2.0-beta.1` from
    /// `## [0.2.0-beta.1] — unreleased`, or the whole heading (trimmed) when
    /// there are no brackets to key off. Used to dedupe the headline against
    /// the bundled sections.
    pub version: String,
    /// The heading text after `## ` (e.g. `[0.2.0-beta.1] — unreleased`),
    /// rendered verbatim as the section title.
    pub heading: String,
    /// The markdown body between this heading and the next, trailing
    /// whitespace trimmed. May be empty.
    pub body: String,
}

/// The canonical, hand-curated `CHANGELOG.md` at the repo root, embedded at
/// compile time. This is the whole "bundled changelog" — no I/O, no network,
/// available even fully offline.
pub fn bundled() -> &'static str {
    include_str!("../../../../../CHANGELOG.md")
}

/// Parse a Keep-a-Changelog markdown document into its `## [version]`
/// sections, in document order (newest first, matching the file convention).
///
/// Pure over its input so the parse is unit-tested against fixtures without
/// gpui or the real file. Everything before the first `## ` heading (the
/// `# Changelog` title + preamble) is ignored; each `## ` opens a new section
/// and every following line accretes into its body until the next `## `.
pub fn parse_changelog(md: &str) -> Vec<ChangelogSection> {
    let mut out: Vec<ChangelogSection> = Vec::new();
    let mut cur: Option<ChangelogSection> = None;

    for line in md.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(sec) = cur.take() {
                out.push(finalize(sec));
            }
            let heading = rest.trim().to_string();
            let version = extract_version(&heading);
            cur = Some(ChangelogSection {
                version,
                heading,
                body: String::new(),
            });
        } else if let Some(sec) = cur.as_mut() {
            sec.body.push_str(line);
            sec.body.push('\n');
        }
        // Lines before the first `## ` (the level-1 title + preamble) have no
        // open section to attach to, so they're dropped — the preamble isn't a
        // version and shouldn't render as one.
    }
    if let Some(sec) = cur.take() {
        out.push(finalize(sec));
    }
    out
}

/// Trim the accumulated body's trailing blank lines so the divider between
/// sections doesn't float below a stack of empty lines.
fn finalize(mut sec: ChangelogSection) -> ChangelogSection {
    let trimmed = sec.body.trim_end();
    sec.body.truncate(trimmed.len());
    sec
}

/// Pull the version token out of a heading. `[0.2.0-beta.1] — unreleased`
/// yields `0.2.0-beta.1`; a bracket-less heading yields the trimmed heading
/// itself (so an odd `## ` line still gets a stable, non-empty key).
fn extract_version(heading: &str) -> String {
    match (heading.find('['), heading.find(']')) {
        (Some(open), Some(close)) if close > open + 1 => heading[open + 1..close].trim().to_string(),
        _ => heading.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# Changelog

Some preamble that is not a version and must not render as a section.

## [0.3.0] — 2026-08-01

### Added
- The newest thing.

## [0.2.0-beta.1] — unreleased

### Added
- An earlier thing.

### Fixed
- A bug.

## [0.1.0-alpha.1] — 2026-06-04

The very first tag.
";

    #[test]
    fn parses_every_version_section_newest_first() {
        let secs = parse_changelog(FIXTURE);
        assert_eq!(secs.len(), 3, "one section per `## ` heading");
        // Document order is preserved, which for Keep-a-Changelog is newest
        // first — the property the viewer relies on to show the top section.
        assert_eq!(secs[0].version, "0.3.0");
        assert_eq!(secs[1].version, "0.2.0-beta.1");
        assert_eq!(secs[2].version, "0.1.0-alpha.1");
    }

    #[test]
    fn preamble_before_first_heading_is_dropped() {
        let secs = parse_changelog(FIXTURE);
        // No section body carries the preamble sentence.
        assert!(secs.iter().all(|s| !s.body.contains("preamble")));
        // The `# Changelog` title never becomes a section.
        assert!(secs.iter().all(|s| s.version != "Changelog"));
    }

    #[test]
    fn section_body_captures_until_next_heading() {
        let secs = parse_changelog(FIXTURE);
        let beta = &secs[1];
        assert!(beta.body.contains("An earlier thing"));
        assert!(beta.body.contains("A bug"));
        // ...but not the next section's content.
        assert!(!beta.body.contains("The very first tag"));
        // Trailing blank lines are trimmed so dividers sit flush.
        assert!(!beta.body.ends_with('\n'));
    }

    #[test]
    fn extract_version_handles_missing_brackets() {
        assert_eq!(extract_version("[1.2.3] — x"), "1.2.3");
        assert_eq!(extract_version("[Unreleased]"), "Unreleased");
        assert_eq!(extract_version("no brackets here"), "no brackets here");
        assert_eq!(extract_version("[]"), "[]"); // empty brackets → keep raw
    }

    #[test]
    fn empty_or_headingless_input_yields_no_sections() {
        assert!(parse_changelog("").is_empty());
        assert!(parse_changelog("# Changelog\n\njust preamble, no versions").is_empty());
    }

    #[test]
    fn pill_hidden_when_no_update_available() {
        // No resolved update ⇒ no pill, regardless of dismissal state.
        assert!(!pill_visible(false, None, None));
        assert!(!pill_visible(false, Some("0.3.0"), None));
        assert!(!pill_visible(false, Some("0.3.0"), Some("0.2.0")));
    }

    #[test]
    fn pill_shown_when_available_and_not_dismissed() {
        assert!(pill_visible(true, Some("0.3.0"), None));
        // Available with no version info still shows (nothing to compare away).
        assert!(pill_visible(true, None, None));
    }

    #[test]
    fn pill_hidden_when_current_version_is_dismissed() {
        // Ignore on the exact version being offered ⇒ hide.
        assert!(!pill_visible(true, Some("0.3.0"), Some("0.3.0")));
    }

    #[test]
    fn pill_reappears_when_a_newer_version_is_available() {
        // THE guarantee (#196 G2): "Ignore" on 0.3.0 must not silence 0.3.1.
        // A newer resolved version differs from the dismissed one ⇒ the pill
        // comes back. Ignore silences THIS update, never all future ones.
        assert!(pill_visible(true, Some("0.3.1"), Some("0.3.0")));
    }

    #[test]
    fn bundled_changelog_parses_to_at_least_one_section() {
        // Guards the real embedded file + the include_str! path: if the repo
        // CHANGELOG.md moves or loses its `## [version]` shape, this fails
        // instead of the viewer silently showing an empty list.
        let secs = parse_changelog(bundled());
        assert!(
            !secs.is_empty(),
            "bundled CHANGELOG.md should parse to >=1 version section"
        );
        // The current unreleased line must be present and keyed correctly.
        assert!(
            secs.iter().any(|s| s.version == "0.2.0-beta.1"),
            "expected the 0.2.0-beta.1 section in the bundled changelog"
        );
    }
}
