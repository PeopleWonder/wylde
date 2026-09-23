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

/// One roster binary matched to its release assets. Every member of the
/// stack carries its own detached signature — there is no bundle signature,
/// so no binary can ride in unverified behind another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackAsset {
    /// Canonical service name, e.g. `wylde-gateway`.
    pub name: String,
    /// File name to write into the staged stack directory, e.g.
    /// `wylde-gateway.exe`.
    pub image: String,
    pub binary: ReleaseAsset,
    pub signature: ReleaseAsset,
}

/// A concrete, installable update: the selected release plus every resolved
/// binary in the stack.
///
/// This used to hold a single `binary`/`signature` pair, which is precisely
/// why the updater could only ever ship `wylde-gui` — the lifecycle daemon
/// and every backend service had nowhere to live in this type (#97).
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
    /// Every binary this update carries, in roster order.
    pub assets: Vec<StackAsset>,
}

impl UpdateInfo {
    /// Total download size across the whole stack, for progress display.
    pub fn total_size(&self) -> u64 {
        self.assets.iter().map(|a| a.binary.size).sum()
    }

    /// Look up one member by canonical service name.
    pub fn asset(&self, name: &str) -> Option<&StackAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
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

/// Resolve every roster binary to its release assets.
///
/// **Registry-driven, not literal.** The set of binaries looked for comes
/// from [`wylde_stack::roster`] — the in-tree core tier plus whatever the
/// `Services/` bucket currently holds — so a service added to the tree is
/// carried by the updater with no edit to this function. The old
/// implementation matched `a.name.starts_with("wylde-gui")`, which is why
/// the entire backend was invisible to the update path.
///
/// Two tiers of strictness, and the difference is deliberate:
///
/// * **In-tree binaries are REQUIRED.** A release that is missing one is
///   rejected wholesale ([`UpdateError::NoAsset`]) rather than installed
///   partially. "GUI new, daemon stale" must not be a reachable state, so a
///   half-populated release fails the check instead of half-applying.
/// * **Bucket siblings are OPTIONAL.** A third-party service under
///   `Services/<name>/` is not something a Wylde release can be expected to
///   publish; it is carried when the release happens to contain it and
///   skipped otherwise.
///
/// Every resolved binary must have its `.minisig` sibling — a signed-but-one
/// release is rejected ([`UpdateError::NoSignature`]), preserving the
/// fail-closed guarantee per binary rather than per release.
pub fn pick_assets(
    release: &Release,
    roster: &[wylde_stack::StackBinary],
) -> Result<Vec<StackAsset>, UpdateError> {
    let mut out = Vec::with_capacity(roster.len());
    for entry in roster {
        let asset_name = entry.asset_name();
        let Some(binary) = release.assets.iter().find(|a| a.name == asset_name) else {
            if entry.is_sibling() {
                // Optional: an out-of-tree sibling this release doesn't ship.
                continue;
            }
            return Err(UpdateError::NoAsset);
        };
        let sig_name = entry.signature_name();
        let signature = release
            .assets
            .iter()
            .find(|a| a.name == sig_name)
            .ok_or(UpdateError::NoSignature)?;
        out.push(StackAsset {
            name: entry.name.clone(),
            image: entry.image.clone(),
            binary: binary.clone(),
            signature: signature.clone(),
        });
    }
    Ok(out)
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

    let assets = pick_assets(release, &wylde_stack::roster())?;
    Ok(UpdateStatus::Available(UpdateInfo {
        version: latest.to_string(),
        tag: release.tag_name.clone(),
        notes: release.body.clone(),
        html_url: release.html_url.clone(),
        assets,
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

    /// A release carrying the WHOLE stack — every roster binary plus its
    /// signature. Built from the live roster so the fixture cannot drift out
    /// of step with what the updater requires.
    fn rel(tag: &str, prerelease: bool, draft: bool) -> Release {
        let mut assets = Vec::new();
        for entry in wylde_stack::roster() {
            assets.push(asset(&entry.asset_name()));
            assets.push(asset(&entry.signature_name()));
        }
        Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            body: format!("notes for {tag}"),
            html_url: format!("https://example.test/releases/{tag}"),
            assets,
        }
    }

    /// A release that carries ONLY the GUI — exactly what the updater used to
    /// accept, and what it must now refuse.
    fn gui_only_rel(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            body: String::new(),
            html_url: String::new(),
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
                assert!(info.notes.contains("v1.2.0"));
                assert!(info.assets.iter().all(|a| a.binary.name.ends_with(".exe")));
                assert!(info
                    .assets
                    .iter()
                    .all(|a| a.signature.name.ends_with(".minisig")));
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
    fn pick_assets_requires_a_signature_sibling_for_every_binary() {
        let mut release = rel("v1.2.0", false, false);
        // Drop ONE .minisig — the gateway's. Fail-closed is per binary, so
        // the whole release must be refused even though everything else is
        // properly signed.
        let victim = format!("wylde-gateway-{}.exe.minisig", wylde_stack::RELEASE_TARGET);
        release.assets.retain(|a| a.name != victim);
        assert!(matches!(
            pick_assets(&release, &wylde_stack::roster()),
            Err(UpdateError::NoSignature)
        ));
    }

    #[test]
    fn pick_assets_requires_a_binary() {
        let release = Release {
            tag_name: "v1.2.0".into(),
            draft: false,
            prerelease: false,
            body: String::new(),
            html_url: String::new(),
            assets: vec![asset("README.txt"), asset("checksums.sha256")],
        };
        assert!(matches!(
            pick_assets(&release, &wylde_stack::roster()),
            Err(UpdateError::NoAsset)
        ));
    }

    /// **The #97 regression test.** A release carrying only `wylde-gui` is
    /// exactly what the old `pick_asset` happily installed, leaving the
    /// daemon and every backend service stale underneath a new GUI. It must
    /// now be rejected outright rather than applied partially.
    #[test]
    fn a_gui_only_release_is_refused_rather_than_half_installed() {
        let release = gui_only_rel("v1.2.0");
        assert!(
            matches!(
                pick_assets(&release, &wylde_stack::roster()),
                Err(UpdateError::NoAsset)
            ),
            "a release with no daemon and no backend services must not install; that is the GUI-new/backend-stale skew #92 cause #2 and #97 exist to eliminate"
        );
    }

    /// The positive half: a whole-stack release resolves the daemon and the
    /// backend services, not just the shell.
    #[test]
    fn pick_assets_carries_the_daemon_and_the_backend() {
        let release = rel("v1.2.0", false, false);
        let picked = pick_assets(&release, &wylde_stack::roster()).unwrap();
        let names: Vec<&str> = picked.iter().map(|a| a.name.as_str()).collect();

        for required in [
            wylde_stack::service_name::GUI,
            wylde_stack::service_name::LIFECYCLE,
            wylde_stack::service_name::GATEWAY,
            wylde_stack::service_name::HARNESS,
            wylde_stack::service_name::WORKSPACES,
        ] {
            assert!(
                names.contains(&required),
                "{required} is not carried by the update: {names:?}"
            );
        }
        for a in &picked {
            assert_eq!(a.signature.name, format!("{}.minisig", a.binary.name));
        }
    }

    /// An out-of-tree sibling the release doesn't publish is skipped, not
    /// fatal — Wylde's release cannot be expected to carry a third party's
    /// service. (In-tree binaries stay required; that is the asymmetry.)
    #[test]
    fn an_unpublished_bucket_sibling_is_skipped_not_fatal() {
        let release = rel("v1.2.0", false, false);
        let mut roster = wylde_stack::roster();
        roster.push(wylde_stack::StackBinary {
            name: "wylde-acme".into(),
            image: "wylde-acme.exe".into(),
            tier: wylde_stack::Tier::Service,
            folder: Some(std::path::PathBuf::from("Services/acme")),
        });

        let picked = pick_assets(&release, &roster).unwrap();
        assert!(!picked.iter().any(|a| a.name == "wylde-acme"));
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
