//! Release model + the pure update-selection logic.
//!
//! Everything in this module is network-free and deterministic: parsing
//! the GitHub Releases JSON, filtering by channel, and deciding whether
//! the newest acceptable release is an upgrade over the running binary.
//! The HTTP I/O that feeds [`evaluate`] lives in `lib.rs`; keeping the
//! decision logic here lets the test suite exercise channel selection and
//! version comparison without a live network.

use semver::Version;
use serde::Deserialize;

use crate::UpdateError;

/// Release channel. `Beta` is a superset of `Stable`: it additionally
/// surfaces GitHub *pre-releases*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl Channel {
    /// Canonical lowercase wire string, as persisted in `updater.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    /// Parse the persisted string. Unknown / empty falls back to the
    /// privacy-conservative `Stable` (never offer pre-releases to someone
    /// whose preference we couldn't read).
    pub fn from_str_lossy(s: &str) -> Channel {
        match s.trim().to_ascii_lowercase().as_str() {
            "beta" => Channel::Beta,
            _ => Channel::Stable,
        }
    }

    /// `true` if this channel accepts pre-release builds.
    fn accepts_prerelease(self) -> bool {
        matches!(self, Channel::Beta)
    }
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

/// A GitHub release, trimmed to the fields the updater consumes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub body: String,
    #[serde(default, rename = "html_url")]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// A concrete, installable update: the selected release plus the resolved
/// binary + signature assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Selected release version, normalised (no leading `v`).
    pub version: String,
    /// Raw tag as it appears on GitHub (e.g. `v0.2.0-beta.1`).
    pub tag: String,
    /// Release notes (markdown).
    pub notes: String,
    /// `html_url` of the release page.
    pub html_url: String,
    pub binary: ReleaseAsset,
    pub signature: ReleaseAsset,
}

/// Outcome of an update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The running version is the newest acceptable release (or there are
    /// no releases at all). Carries the running version for display.
    UpToDate { current: String },
    /// A newer release is available and fully resolved (binary + sig).
    Available(UpdateInfo),
}

impl UpdateStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, UpdateStatus::Available(_))
    }
}

/// Strip a leading `v`/`V` and parse as semver.
fn parse_tag(tag: &str) -> Option<Version> {
    let trimmed = tag.trim().trim_start_matches(['v', 'V']);
    Version::parse(trimmed).ok()
}

/// Releases that are *eligible* for `channel`, paired with their parsed
/// version. Drops drafts, channel-excluded pre-releases, and any tag that
/// isn't valid semver (logged, not fatal).
fn candidates(releases: &[Release], channel: Channel) -> Vec<(&Release, Version)> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| channel.accepts_prerelease() || !r.prerelease)
        .filter_map(|r| match parse_tag(&r.tag_name) {
            Some(v) => Some((r, v)),
            None => {
                tracing::warn!(tag = %r.tag_name, "skipping release: tag is not valid semver");
                None
            }
        })
        .collect()
}

/// Pick the highest-versioned eligible release for `channel`.
pub fn select_release(releases: &[Release], channel: Channel) -> Option<&Release> {
    candidates(releases, channel)
        .into_iter()
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(r, _)| r)
}

/// Resolve the binary asset and its `.minisig` sibling from a release.
///
/// The binary is the asset whose name starts with `wylde-gui` and is not
/// itself a `.minisig`; the signature is the asset named
/// `<binary-name>.minisig`. A release missing either is rejected
/// (fail-closed) rather than installed unsigned.
pub fn pick_asset(release: &Release) -> Result<(ReleaseAsset, ReleaseAsset), UpdateError> {
    let binary = release
        .assets
        .iter()
        .find(|a| a.name.starts_with("wylde-gui") && !a.name.ends_with(".minisig"))
        .ok_or(UpdateError::NoAsset)?;
    let sig_name = format!("{}.minisig", binary.name);
    let signature = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .ok_or(UpdateError::NoSignature)?;
    Ok((binary.clone(), signature.clone()))
}

/// The pure core of [`crate::check_for_update`]: given the release list,
/// the channel, and the running version, decide the [`UpdateStatus`].
///
/// `current_version` must be valid semver (the binary's own
/// `CARGO_PKG_VERSION`); a malformed value is an [`UpdateError::Version`]
/// rather than a silent "up to date".
pub fn evaluate(
    releases: &[Release],
    channel: Channel,
    current_version: &str,
) -> Result<UpdateStatus, UpdateError> {
    let current = parse_tag(current_version).ok_or_else(|| {
        UpdateError::Version(format!(
            "current version `{current_version}` is not valid semver"
        ))
    })?;

    let Some(release) = select_release(releases, channel) else {
        // No eligible release at all — nothing to offer.
        return Ok(UpdateStatus::UpToDate {
            current: current.to_string(),
        });
    };

    // select_release only returns releases whose tag already parsed.
    let latest = parse_tag(&release.tag_name).expect("selected release tag re-parses");

    if latest <= current {
        return Ok(UpdateStatus::UpToDate {
            current: current.to_string(),
        });
    }

    let (binary, signature) = pick_asset(release)?;
    Ok(UpdateStatus::Available(UpdateInfo {
        version: latest.to_string(),
        tag: release.tag_name.clone(),
        notes: release.body.clone(),
        html_url: release.html_url.clone(),
        binary,
        signature,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            url: format!("https://example.test/{name}"),
            size: 10,
        }
    }

    /// A release with the standard binary + sig asset pair.
    fn rel(tag: &str, prerelease: bool, draft: bool) -> Release {
        Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            body: format!("notes for {tag}"),
            html_url: format!("https://example.test/releases/{tag}"),
            assets: vec![
                asset("wylde-gui-x86_64-pc-windows-msvc.exe"),
                asset("wylde-gui-x86_64-pc-windows-msvc.exe.minisig"),
            ],
        }
    }

    #[test]
    fn channel_round_trips_through_strings() {
        assert_eq!(Channel::Stable.as_str(), "stable");
        assert_eq!(Channel::Beta.as_str(), "beta");
        assert_eq!(Channel::from_str_lossy("beta"), Channel::Beta);
        assert_eq!(Channel::from_str_lossy("BETA"), Channel::Beta);
        assert_eq!(Channel::from_str_lossy("stable"), Channel::Stable);
        // Unknown / empty falls back to Stable (never silently opt into beta).
        assert_eq!(Channel::from_str_lossy("nightly"), Channel::Stable);
        assert_eq!(Channel::from_str_lossy(""), Channel::Stable);
        assert_eq!(Channel::default(), Channel::Stable);
    }

    #[test]
    fn parse_tag_strips_v_prefix() {
        assert_eq!(parse_tag("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_tag("1.2.3"), Some(Version::new(1, 2, 3)));
        assert!(parse_tag("v1.2.3-beta.1").unwrap().pre.as_str() == "beta.1");
        assert_eq!(parse_tag("not-a-version"), None);
    }

    #[test]
    fn stable_channel_skips_prereleases() {
        let releases = vec![
            rel("v1.0.0", false, false),
            rel("v1.1.0-beta.1", true, false),
        ];
        let chosen = select_release(&releases, Channel::Stable).unwrap();
        assert_eq!(chosen.tag_name, "v1.0.0");
    }

    #[test]
    fn beta_channel_includes_prereleases() {
        let releases = vec![
            rel("v1.0.0", false, false),
            rel("v1.1.0-beta.1", true, false),
        ];
        let chosen = select_release(&releases, Channel::Beta).unwrap();
        assert_eq!(chosen.tag_name, "v1.1.0-beta.1");
    }

    #[test]
    fn drafts_are_never_selected() {
        let releases = vec![rel("v2.0.0", false, true), rel("v1.0.0", false, false)];
        // Draft v2.0.0 is ignored on both channels even though it's higher.
        assert_eq!(
            select_release(&releases, Channel::Stable).unwrap().tag_name,
            "v1.0.0"
        );
        assert_eq!(
            select_release(&releases, Channel::Beta).unwrap().tag_name,
            "v1.0.0"
        );
    }

    #[test]
    fn highest_semver_wins_regardless_of_list_order() {
        let releases = vec![
            rel("v1.0.0", false, false),
            rel("v1.10.0", false, false),
            rel("v1.2.0", false, false),
        ];
        assert_eq!(
            select_release(&releases, Channel::Stable).unwrap().tag_name,
            "v1.10.0"
        );
    }

    #[test]
    fn semver_orders_stable_above_its_prerelease() {
        // 1.1.0 final must beat 1.1.0-beta.2 on the beta channel.
        let releases = vec![
            rel("v1.1.0-beta.2", true, false),
            rel("v1.1.0", false, false),
        ];
        assert_eq!(
            select_release(&releases, Channel::Beta).unwrap().tag_name,
            "v1.1.0"
        );
    }

    #[test]
    fn evaluate_reports_available_when_newer() {
        let releases = vec![rel("v1.2.0", false, false)];
        let status = evaluate(&releases, Channel::Stable, "1.0.0").unwrap();
        match status {
            UpdateStatus::Available(info) => {
                assert_eq!(info.version, "1.2.0");
                assert_eq!(info.tag, "v1.2.0");
                assert!(info.binary.name.ends_with(".exe"));
                assert!(info.signature.name.ends_with(".minisig"));
                assert!(info.notes.contains("v1.2.0"));
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_up_to_date_when_equal_or_older() {
        let releases = vec![rel("v1.0.0", false, false)];
        assert!(matches!(
            evaluate(&releases, Channel::Stable, "1.0.0").unwrap(),
            UpdateStatus::UpToDate { .. }
        ));
        // Running ahead of the published release (dev build) is also "up to date".
        assert!(matches!(
            evaluate(&releases, Channel::Stable, "1.5.0").unwrap(),
            UpdateStatus::UpToDate { .. }
        ));
    }

    #[test]
    fn evaluate_up_to_date_when_no_releases() {
        assert!(matches!(
            evaluate(&[], Channel::Beta, "1.0.0").unwrap(),
            UpdateStatus::UpToDate { current } if current == "1.0.0"
        ));
    }

    #[test]
    fn evaluate_rejects_malformed_current_version() {
        let releases = vec![rel("v1.2.0", false, false)];
        assert!(matches!(
            evaluate(&releases, Channel::Stable, "garbage"),
            Err(UpdateError::Version(_))
        ));
    }

    #[test]
    fn pick_asset_requires_a_signature_sibling() {
        let mut release = rel("v1.2.0", false, false);
        // Drop the .minisig — an unsigned release must be rejected, not installed.
        release.assets.retain(|a| !a.name.ends_with(".minisig"));
        assert!(matches!(
            pick_asset(&release),
            Err(UpdateError::NoSignature)
        ));
    }

    #[test]
    fn pick_asset_requires_a_binary() {
        let release = Release {
            tag_name: "v1.2.0".into(),
            draft: false,
            prerelease: false,
            body: String::new(),
            html_url: String::new(),
            assets: vec![asset("README.txt"), asset("checksums.sha256")],
        };
        assert!(matches!(pick_asset(&release), Err(UpdateError::NoAsset)));
    }

    #[test]
    fn releases_parse_from_github_shape() {
        // Trimmed but realistic GitHub Releases payload.
        let json = r#"[
            {
              "tag_name": "v1.1.0-beta.1",
              "draft": false,
              "prerelease": true,
              "body": "beta notes",
              "html_url": "https://github.com/PeopleWonder/wylde/releases/tag/v1.1.0-beta.1",
              "assets": [
                {"name":"wylde-gui-x86_64-pc-windows-msvc.exe","browser_download_url":"https://x/bin","size":1234},
                {"name":"wylde-gui-x86_64-pc-windows-msvc.exe.minisig","browser_download_url":"https://x/sig","size":10}
              ]
            },
            {
              "tag_name": "v1.0.0",
              "draft": false,
              "prerelease": false,
              "body": "stable notes",
              "html_url": "https://github.com/PeopleWonder/wylde/releases/tag/v1.0.0",
              "assets": []
            }
        ]"#;
        let releases: Vec<Release> = serde_json::from_str(json).unwrap();
        assert_eq!(releases.len(), 2);
        assert!(releases[0].prerelease);
        assert_eq!(releases[0].assets.len(), 2);
        assert_eq!(releases[0].assets[0].size, 1234);
        // Stable channel picks v1.0.0; beta picks the v1.1.0-beta.1.
        assert_eq!(
            select_release(&releases, Channel::Stable).unwrap().tag_name,
            "v1.0.0"
        );
        assert_eq!(
            select_release(&releases, Channel::Beta).unwrap().tag_name,
            "v1.1.0-beta.1"
        );
    }
}
