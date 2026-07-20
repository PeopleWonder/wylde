//! Wylde in-app self-updater (Phase 12.5).
//!
//! Pulls newer builds of the **whole stack** from the project's **public**
//! GitHub Releases (`PeopleWonder/wylde`), verifies each binary against one
//! embedded minisign/Ed25519 public key, and swaps them as a unit on Windows.
//!
//! Until #97 this crate carried `wylde-gui` and nothing else: `pick_asset`
//! matched a `wylde-gui` literal and `install_update` called
//! `self_replace` on the running executable. The lifecycle daemon and every
//! backend service were therefore never updated — and since most of Wylde's
//! logic is backend, a backend fix could not reach an installed user at all.
//! What ships is now DERIVED from [`wylde_stack::roster`], so a service added
//! to the tree is carried with no edit here.
//!
//! Design rationale lives in `docs/self-updater-design.md`. Properties this
//! crate guarantees:
//!
//! * **Privacy.** The only outbound call is an unauthenticated `GET` to the
//!   public GitHub REST API. No identity, token, or fingerprint is sent.
//! * **Fail-closed verification, per binary.** No code path installs a
//!   binary that hasn't passed [`verify_signature`] against the embedded
//!   key. Each stack member is verified individually — there is no bundle
//!   signature, so nothing rides in unverified behind something else. An
//!   un-keyed (placeholder) build refuses to install at all.
//! * **All-or-nothing application.** The new stack is fully staged and
//!   fully verified in a version directory before anything is switched over;
//!   the switch itself is one atomic pointer move
//!   ([`wylde_stack::current::set_current`]). "GUI new, daemon stale" is not
//!   a reachable state.
//! * **Blocking, runtime-free.** The whole API is synchronous. The GUI,
//!   which has no tokio reactor on its executor, drives it via the Pipe
//!   crate's `bridged_spawn_blocking`.
//!
//! ## Typical flow
//!
//! ```no_run
//! use wylde_updater::{check_for_update, download_release, install_stack, Channel, UpdateStatus};
//!
//! let status = check_for_update(Channel::Stable, env!("CARGO_PKG_VERSION"))?;
//! if let UpdateStatus::Available(info) = status {
//!     let dl = download_release(&info)?;
//!     // install_stack re-verifies every binary before switching over.
//!     install_stack(&info.version, &dl)?;
//!     // ...prompt the user to restart to apply.
//! }
//! # Ok::<(), wylde_updater::UpdateError>(())
//! ```

mod pubkey;
mod release;
mod verify;

use std::io::Read;
use std::path::{Path, PathBuf};

use semver::Version;

pub use pubkey::{has_signing_key, PUBLIC_KEY};
pub use release::{
    pick_assets, Channel, Release, ReleaseAsset, StackAsset, UpdateInfo, UpdateStatus,
};
pub use verify::verify_signature;

/// GitHub REST endpoint for the source repo's releases. Overridable via
/// `WYLDE_UPDATE_RELEASES_URL` for manual end-to-end testing against a
/// fixture server; the default is the live public endpoint.
const DEFAULT_RELEASES_URL: &str =
    "https://api.github.com/repos/PeopleWonder/wylde/releases?per_page=30";

/// `User-Agent` sent on every request. GitHub rejects API calls without
/// one. Carries the crate version so server logs can attribute traffic.
const USER_AGENT: &str = concat!("wylde-updater/", env!("CARGO_PKG_VERSION"));

/// Errors surfaced by the updater. Kept coarse and `Display`-friendly so
/// the Settings panel can show the message verbatim.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Http(String),
    #[error("could not parse GitHub response: {0}")]
    Parse(String),
    #[error("version error: {0}")]
    Version(String),
    /// The release is missing an asset for a binary the stack requires.
    /// Rejected wholesale rather than applied partially — a release that
    /// carries the GUI but not the daemon must not install.
    #[error("release is missing a required stack binary")]
    NoAsset,
    /// The binary asset has no `.minisig` sibling — refused (fail-closed).
    #[error("release binary is unsigned (no .minisig asset)")]
    NoSignature,
    /// The build carries no production signing key (still the placeholder).
    #[error("this build has no signing key embedded; updates are disabled")]
    NoSigningKey,
    #[error("{0}")]
    Verify(String),
    #[error("io error: {0}")]
    Io(String),
}

/// One downloaded stack member: its bytes plus the text of its `.minisig`.
#[derive(Debug, Clone)]
pub struct DownloadedBinary {
    /// Canonical service name, e.g. `wylde-gateway`.
    pub name: String,
    /// File name to write into the staged stack directory.
    pub image: String,
    pub bytes: Vec<u8>,
    pub minisig: String,
}

/// Every binary of a downloaded update. Named for what it is now: the whole
/// stack, not one executable.
#[derive(Debug, Clone, Default)]
pub struct DownloadedRelease {
    pub binaries: Vec<DownloadedBinary>,
}

impl DownloadedRelease {
    pub fn get(&self, name: &str) -> Option<&DownloadedBinary> {
        self.binaries.iter().find(|b| b.name == name)
    }
}

fn releases_url() -> String {
    std::env::var("WYLDE_UPDATE_RELEASES_URL").unwrap_or_else(|_| DEFAULT_RELEASES_URL.to_string())
}

/// Query GitHub Releases and decide whether `current_version` has an
/// update on `channel`. This is the one network call the manual
/// "Check now" button makes.
pub fn check_for_update(
    channel: Channel,
    current_version: &str,
) -> Result<UpdateStatus, UpdateError> {
    let body = http_get_text(&releases_url())?;
    let releases: Vec<Release> =
        serde_json::from_str(&body).map_err(|e| UpdateError::Parse(e.to_string()))?;
    release::evaluate(&releases, channel, current_version)
}

/// Download every binary in a resolved [`UpdateInfo`], plus its signature.
///
/// All-or-nothing: a failure on any member aborts the whole download, so a
/// partially fetched stack is never handed to [`install_stack`].
pub fn download_release(info: &UpdateInfo) -> Result<DownloadedRelease, UpdateError> {
    tracing::info!(
        version = %info.version,
        binaries = info.assets.len(),
        "downloading whole-stack update"
    );
    let mut binaries = Vec::with_capacity(info.assets.len());
    for asset in &info.assets {
        tracing::debug!(name = %asset.name, "downloading stack member");
        binaries.push(DownloadedBinary {
            name: asset.name.clone(),
            image: asset.image.clone(),
            bytes: http_get_bytes(&asset.binary.url)?,
            minisig: http_get_text(&asset.signature.url)?,
        });
    }
    Ok(DownloadedRelease { binaries })
}

/// Verify and install the whole downloaded stack, then switch to it
/// atomically.
///
/// The sequence is deliberately ordered so that a failure at any point
/// leaves the running installation untouched:
///
/// 1. **Verify everything first.** Every binary is re-checked against the
///    embedded key (defence in depth — safe even if the caller skipped an
///    explicit [`verify_signature`]). One bad signature aborts before a
///    single byte is written.
/// 2. **Stage into a fresh version directory.** `<home>/versions/<version>/`
///    is populated in full. The live stack is not touched.
/// 3. **Repoint `current` atomically.** One rename
///    ([`wylde_stack::current::set_current`]) makes the whole new stack live
///    at once. A reader sees either the entire old stack or the entire new
///    one — never the GUI from one and the daemon from another.
/// 4. **Prune old version directories (#139).** With the pointer now on the
///    new stack, `versions/<ver>/` directories older than the retention
///    window ([`VERSIONS_RETAINED`]) are removed, so disk use stays bounded
///    instead of growing by a full copy of the whole stack every update. This
///    runs *last* and is best-effort: it never touches the current stack or
///    the one-previous rollback target, and it never fails an update that has
///    already succeeded (see [`prune_old_versions`]).
///
/// On success the new stack takes effect on the **next launch** — prompt the
/// user to restart. The running processes are deliberately *not*
/// `self_replace`d: that trick can only ever swap the one currently-executing
/// binary, which is exactly the limitation that confined the updater to the
/// GUI. Resolution through the `current` pointer covers every binary instead.
pub fn install_stack(version: &str, release: &DownloadedRelease) -> Result<(), UpdateError> {
    if release.binaries.is_empty() {
        return Err(UpdateError::Io("refusing to install an empty stack".into()));
    }

    // 1. Verify the whole set BEFORE writing anything.
    for bin in &release.binaries {
        verify_signature(&bin.bytes, &bin.minisig)?;
    }

    // 2-4. Stage, switch, prune. Split into its own function so the
    // post-verification sequence — the part #139 touches — is exercisable in
    // tests without a production signature (whose private half no test can
    // reproduce; the verification gate above is covered on its own in
    // `verify.rs`).
    stage_switch_and_prune(version, release)
}

/// Steps 2–4 of [`install_stack`], on an already-verified stack: stage into a
/// fresh version directory, flip `current` to it, then prune older versions.
/// Kept separate from the signature check so the staging → switch → prune path
/// is unit-testable without the embedded signing key.
fn stage_switch_and_prune(version: &str, release: &DownloadedRelease) -> Result<(), UpdateError> {
    // 2. Stage.
    let dir = version_dir(version)?;

    // Never write into the stack that is currently live. Re-installing the
    // same version, a retagged release, or a rollback would otherwise
    // overwrite running binaries in place — which on Windows fails partway
    // through with a sharing violation and leaves the live stack half
    // replaced while `current` still points at it. That is exactly the
    // partial-application state this function promises is unreachable.
    if let Some(live) = wylde_stack::current::current_dir() {
        if same_dir(&live, &dir) {
            return Err(UpdateError::Io(format!(
                "version {version} is already the current stack; refusing to \
                 overwrite it in place"
            )));
        }
    }

    stage_stack_in(&dir, release)?;

    // 3. Switch.
    wylde_stack::current::set_current(&dir).map_err(|e| UpdateError::Io(e.to_string()))?;

    // 4. Bound disk growth (#139). The pointer already names `dir`, so the new
    // stack is live and its predecessor is still on disk as the rollback
    // fallback — there is no instant in which the fallback is deleted before
    // the new version is committed. `dir` is `<home>/versions/<version>`, so
    // its parent is the `versions/` root to prune within.
    if let Some(versions_root) = dir.parent() {
        prune_old_versions(versions_root, &dir, VERSIONS_RETAINED);
    }

    tracing::info!(
        version,
        binaries = release.binaries.len(),
        dir = %dir.display(),
        "whole-stack update installed; applies on next launch"
    );
    Ok(())
}

/// How many version directories to keep under `versions/` after a successful
/// install: the newly-installed **current** stack, plus `VERSIONS_RETAINED - 1`
/// older stacks kept as rollback fallbacks.
///
/// **Why two.** Each retained version is a *full copy of the whole stack*, so
/// the floor is set as low as the rollback guarantee allows and no lower. The
/// atomic `current` pointer makes rolling back to the immediately-previous
/// version a single re-point; keeping exactly one previous is what that costs.
/// There is no rollback *consumer* in the tree yet — only the pointer mechanism
/// that would enable one — so this is a deliberate safety floor, not a tuned
/// depth: raise it if and when a rollback path proves it needs deeper history.
const VERSIONS_RETAINED: usize = 2;

/// Prune `versions/` down to [`VERSIONS_RETAINED`] entries, always keeping
/// `current` (the just-installed stack) and the newest older versions beneath
/// it. Called from [`stage_switch_and_prune`] *after* the pointer has flipped,
/// so what survives is decided with the new stack already live and its
/// predecessor still present — there is no instant where the fallback is
/// deleted before the new version is committed.
///
/// The properties #139 requires, and why each holds:
///
/// * **Bounded by construction.** Growth is capped on every install, not by
///   anyone remembering to clean up. N installs leave at most
///   [`VERSIONS_RETAINED`] directories, never N.
/// * **Never the current or the rollback target.** `current` is excluded by
///   path — not by parsing its name, which may be anything — and the single
///   newest *other* version, the one a rollback would re-point to, is kept.
/// * **Self-extending.** It enumerates `versions/*` from disk and removes whole
///   directories, so a new service that drops extra binaries under a version
///   dir is covered with no edit here, and a stray directory an earlier run
///   failed to delete (or that arrived some other way) is reconsidered on the
///   next install and caught up rather than leaking forever.
/// * **Failure-safe.** A directory that can't be removed — a locked or in-use
///   binary — is logged and skipped; the update already succeeded, so this
///   returns nothing and never fails it. Because the enumeration re-runs every
///   install, the stuck directory is simply retried next time.
///
/// Ordering is by semver descending, so "newest older version" is the highest
/// version below `current` on an upgrade and the version just stepped down
/// *from* on a downgrade — the correct rollback target either way. Names that
/// don't parse as semver sort last and are pruned before any real version is.
fn prune_old_versions(versions_root: &Path, current: &Path, keep: usize) {
    let entries = match std::fs::read_dir(versions_root) {
        Ok(entries) => entries,
        // No `versions/` directory (or it vanished) — nothing to prune.
        Err(_) => return,
    };

    // Every version directory except the live one. `current` is matched by
    // path, so it is retained whatever its directory name happens to be.
    let mut stale: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| !same_dir(p, current))
        .collect();

    // Newest first: parseable versions by semver descending, unparseable last.
    stale.sort_by(|a, b| match (dir_version(a), dir_version(b)) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.file_name().cmp(&a.file_name()),
    });

    // `current` already occupies one retained slot, so keep `keep - 1` others.
    for old in stale.iter().skip(keep.saturating_sub(1)) {
        match std::fs::remove_dir_all(old) {
            Ok(()) => tracing::info!(dir = %old.display(), "pruned old version dir (#139)"),
            Err(e) => tracing::warn!(
                dir = %old.display(),
                error = %e,
                "could not prune old version dir; will retry on next install"
            ),
        }
    }
}

/// Parse a version directory's name as semver, tolerating a leading `v`.
/// Returns `None` for anything that isn't a version; such directories sort
/// last in the prune order and are therefore removed before any real one.
fn dir_version(dir: &Path) -> Option<Version> {
    let name = dir.file_name()?.to_str()?;
    Version::parse(name.trim_start_matches(['v', 'V'])).ok()
}

/// `<home>/versions/<version>/`, created. `<home>` is `WYLDE_HOME` or
/// `%LOCALAPPDATA%\Wylde` — the same root the `current` pointer lives under,
/// so the installed stack and the pointer to it never diverge across users.
fn version_dir(version: &str) -> Result<PathBuf, UpdateError> {
    // Refuse anything that could escape the versions directory. Version
    // strings come from a GitHub tag, so they are attacker-influenced even if
    // the binaries behind them are signed.
    // The separator check also covers absolute paths, UNC prefixes, and NTFS
    // alternate data streams (`1.0.0:evil`), since all three need one of these
    // characters. Reserved DOS device names are rejected explicitly: they are
    // not a traversal risk but they make `create_dir_all` fail with an opaque
    // OS error, and a clear refusal is easier to act on.
    let stem = version.split('.').next().unwrap_or(version);
    let is_reserved_device = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
    ) || (stem.len() == 4
        && (stem.to_ascii_uppercase().starts_with("COM")
            || stem.to_ascii_uppercase().starts_with("LPT"))
        && stem.ends_with(|c: char| c.is_ascii_digit()));

    if version.is_empty()
        || version.contains(['/', '\\', ':'])
        || version.contains("..")
        || version.starts_with('.')
        || is_reserved_device
    {
        return Err(UpdateError::Io(format!(
            "refusing to install into a suspicious version directory: {version:?}"
        )));
    }
    let pointer = wylde_stack::current::pointer_path()
        .ok_or_else(|| UpdateError::Io("no home directory for the install root".into()))?;
    let home = pointer
        .parent()
        .ok_or_else(|| UpdateError::Io("pointer path has no parent".into()))?;
    let dir = home.join("versions").join(version);
    std::fs::create_dir_all(&dir)
        .map_err(|e| UpdateError::Io(format!("creating {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Are two paths the same directory? Compares canonicalised forms so a
/// pointer written with different casing or a trailing separator still counts
/// as the live stack. Falls back to a literal compare if either path can't be
/// canonicalised (which, for the live directory, means it is gone anyway).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Write every downloaded binary into `dir`. Split from [`install_stack`] so
/// the staging seam is unit-testable without a signing key or a live pointer.
fn stage_stack_in(dir: &Path, release: &DownloadedRelease) -> Result<(), UpdateError> {
    for bin in &release.binaries {
        if bin.bytes.is_empty() {
            return Err(UpdateError::Io(format!(
                "refusing to stage an empty payload for {}",
                bin.name
            )));
        }
        let path = dir.join(&bin.image);
        std::fs::write(&path, &bin.bytes)
            .map_err(|e| UpdateError::Io(format!("staging {}: {e}", bin.image)))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| UpdateError::Io(format!("staging stat: {e}")))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| UpdateError::Io(format!("staging chmod: {e}")))?;
        }
    }
    Ok(())
}

fn http_get_text(url: &str) -> Result<String, UpdateError> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    resp.into_string()
        .map_err(|e| UpdateError::Http(format!("reading response body: {e}")))
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| UpdateError::Http(format!("reading response body: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn user_agent_carries_crate_version() {
        assert!(USER_AGENT.starts_with("wylde-updater/"));
        assert!(USER_AGENT.len() > "wylde-updater/".len());
    }

    #[test]
    fn releases_url_default_points_at_source_repo() {
        // No override set in this test process.
        std::env::remove_var("WYLDE_UPDATE_RELEASES_URL");
        assert!(releases_url().contains("PeopleWonder/wylde"));
        assert!(releases_url().contains("/releases"));
    }

    fn dl(name: &str, image: &str, bytes: &[u8]) -> DownloadedBinary {
        DownloadedBinary {
            name: name.into(),
            image: image.into(),
            bytes: bytes.to_vec(),
            minisig: String::new(),
        }
    }

    #[test]
    fn stage_stack_writes_every_binary_under_its_own_image_name() {
        let dir = tempfile::tempdir().unwrap();
        let release = DownloadedRelease {
            binaries: vec![
                dl("wylde-gui", "wylde-gui.exe", b"gui bytes"),
                dl("wylde-lifecycle", "wylde-lifecycle.exe", b"daemon bytes"),
                dl("wylde-gateway", "wylde-gateway.exe", b"gateway bytes"),
            ],
        };
        stage_stack_in(dir.path(), &release).unwrap();

        // The whole stack lands, not just the shell — the property the old
        // single-payload `stage_update` could not express.
        assert_eq!(
            std::fs::read(dir.path().join("wylde-gui.exe")).unwrap(),
            b"gui bytes"
        );
        assert_eq!(
            std::fs::read(dir.path().join("wylde-lifecycle.exe")).unwrap(),
            b"daemon bytes"
        );
        assert_eq!(
            std::fs::read(dir.path().join("wylde-gateway.exe")).unwrap(),
            b"gateway bytes"
        );
    }

    #[test]
    fn stage_stack_refuses_an_empty_payload() {
        let dir = tempfile::tempdir().unwrap();
        let release = DownloadedRelease {
            binaries: vec![dl("wylde-gateway", "wylde-gateway.exe", b"")],
        };
        assert!(matches!(
            stage_stack_in(dir.path(), &release),
            Err(UpdateError::Io(_))
        ));
    }

    #[test]
    fn install_refuses_an_empty_stack() {
        assert!(matches!(
            install_stack("1.0.0", &DownloadedRelease::default()),
            Err(UpdateError::Io(_))
        ));
    }

    /// Release tags are attacker-influenced strings that become a directory
    /// name, so traversal attempts must be refused before `create_dir_all`.
    #[test]
    fn version_dir_refuses_path_traversal() {
        for bad in [
            "..",
            "../../etc",
            r"..\windows",
            "a/b",
            // NTFS alternate data stream, and an absolute path.
            "1.0.0:evil",
            r"C:\Windows",
            // Leading dot (hidden / relative).
            ".hidden",
            // Embedded traversal that no single-component check would catch.
            "1.0.0/../../evil",
            "",
            // Reserved DOS device names — not traversal, but they make
            // create_dir_all fail opaquely.
            "CON",
            "nul",
            "COM1",
            "LPT9",
        ] {
            assert!(
                matches!(version_dir(bad), Err(UpdateError::Io(_))),
                "version_dir accepted a suspicious version string: {bad:?}"
            );
        }
    }

    /// The guard must not reject legitimate release tags.
    #[test]
    #[serial]
    fn version_dir_accepts_real_release_versions() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(wylde_stack::current::HOME_DIR_ENV, home.path());
        for good in ["0.2.0", "0.2.0-beta.1", "1.0.0-rc.2+build.5", "10.20.30"] {
            assert!(
                version_dir(good).is_ok(),
                "version_dir rejected a legitimate release version: {good:?}"
            );
        }
        std::env::remove_var(wylde_stack::current::HOME_DIR_ENV);
    }

    #[test]
    fn update_errors_render_human_messages() {
        assert_eq!(
            UpdateError::NoSignature.to_string(),
            "release binary is unsigned (no .minisig asset)"
        );
        assert!(UpdateError::NoSigningKey
            .to_string()
            .contains("no signing key"));
    }

    // ---- #139: `versions/` retention -------------------------------------
    //
    // These drive the post-verification install seam ([`stage_switch_and_prune`])
    // directly. The signature gate above it is exercised in `verify.rs`; it
    // can't be here, because no test can reproduce the private half of the
    // embedded production key. Each test isolates the pointer/home env, so they
    // are `#[serial]` against each other and the other env-touching test.

    /// Isolate the `WYLDE_HOME` pointer root (and clear any `WYLDE_CURRENT`
    /// override) for a test, restoring on drop even if the test panics so a
    /// later serial test never inherits a dangling home. Mirrors the guard in
    /// `wylde-stack`'s `current` tests.
    struct HomeEnv;
    impl HomeEnv {
        fn set(home: &Path) -> Self {
            std::env::set_var(wylde_stack::current::HOME_DIR_ENV, home);
            std::env::remove_var(wylde_stack::current::CURRENT_DIR_ENV);
            HomeEnv
        }
    }
    impl Drop for HomeEnv {
        fn drop(&mut self) {
            std::env::remove_var(wylde_stack::current::HOME_DIR_ENV);
            std::env::remove_var(wylde_stack::current::CURRENT_DIR_ENV);
        }
    }

    /// A minimal well-formed downloaded stack. Bytes are non-empty (staging
    /// rejects an empty payload); the version is baked into them so a retained
    /// directory can be proven intact by content, not just existence.
    fn stack(version: &str) -> DownloadedRelease {
        DownloadedRelease {
            binaries: vec![dl(
                "wylde-lifecycle",
                "wylde-lifecycle.exe",
                format!("daemon bytes for {version}").as_bytes(),
            )],
        }
    }

    /// The version directories currently under `versions_root`, sorted by name.
    fn version_dirs(versions_root: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(versions_root)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs
    }

    fn dir_names(versions_root: &Path) -> Vec<String> {
        version_dirs(versions_root)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// **#139 — the boundedness gate.** Successive installs must leave
    /// `versions/` holding at most the retention count, not one full stack per
    /// update. Against the pre-#139 code — which staged every version and never
    /// pruned — this asserts `<= 2` where the directory in fact holds six; it
    /// goes red, which is the entire point. Delete the step-4 prune call and it
    /// goes red again.
    #[test]
    #[serial]
    fn successive_installs_keep_versions_bounded() {
        let home = tempfile::tempdir().unwrap();
        let _env = HomeEnv::set(home.path());

        for n in 1..=6 {
            let v = format!("0.0.{n}");
            stage_switch_and_prune(&v, &stack(&v)).unwrap();
        }

        let versions = home.path().join("versions");
        let count = version_dirs(&versions).len();
        assert!(
            count <= VERSIONS_RETAINED,
            "versions/ grew unbounded: {count} dirs after six installs, \
             expected at most {VERSIONS_RETAINED} — {:?}",
            dir_names(&versions),
        );
    }

    /// **#139 — rollback safety.** After pruning, the one retained previous
    /// version must be the immediately-preceding one AND be intact: a real,
    /// re-pointable rollback target, not a hollowed-out directory.
    #[test]
    #[serial]
    fn the_retained_previous_is_an_intact_rollback_target() {
        let home = tempfile::tempdir().unwrap();
        let _env = HomeEnv::set(home.path());

        for v in ["0.1.0", "0.2.0", "0.3.0"] {
            stage_switch_and_prune(v, &stack(v)).unwrap();
        }

        let versions = home.path().join("versions");

        // Bounded to current (0.3.0) + exactly one previous (0.2.0); the oldest
        // (0.1.0) is gone.
        assert_eq!(
            dir_names(&versions),
            vec!["0.2.0".to_string(), "0.3.0".to_string()],
            "kept the wrong set",
        );
        assert!(
            !versions.join("0.1.0").exists(),
            "0.1.0 should have been pruned"
        );

        // The retained previous is a working rollback target: its staged daemon
        // is byte-intact, and re-pointing `current` at it resolves.
        let prev = versions.join("0.2.0");
        assert_eq!(
            std::fs::read(prev.join("wylde-lifecycle.exe")).unwrap(),
            b"daemon bytes for 0.2.0",
            "the rollback target was pruned or corrupted"
        );
        wylde_stack::current::set_current(&prev).unwrap();
        assert_eq!(
            wylde_stack::current::current_dir().as_deref(),
            Some(prev.as_path()),
            "rolling back to the retained previous must resolve"
        );
    }

    /// **#139 — self-extending + catch-up.** Pruning enumerates `versions/*`
    /// from disk and removes whole directories, so (a) a directory holding a
    /// binary this code never heard of is still pruned in full — a new
    /// service's extra binaries are covered with no edit here — and (b) a stray
    /// directory left behind by an earlier run is caught up on a later install
    /// rather than leaking.
    #[test]
    #[serial]
    fn prune_is_disk_driven_so_foreign_dirs_are_caught_up() {
        let home = tempfile::tempdir().unwrap();
        let _env = HomeEnv::set(home.path());

        let versions = home.path().join("versions");
        // A leftover no install this session created, carrying a binary no
        // roster knows about.
        let leaked = versions.join("0.0.9");
        std::fs::create_dir_all(&leaked).unwrap();
        std::fs::write(leaked.join("wylde-brand-new-service.exe"), b"unknown").unwrap();

        stage_switch_and_prune("0.1.0", &stack("0.1.0")).unwrap();
        stage_switch_and_prune("0.2.0", &stack("0.2.0")).unwrap();

        // 0.0.9 is older than the retained window; it is removed whole,
        // including the binary this code was never taught about.
        assert!(
            !leaked.exists(),
            "a disk-enumerated foreign version dir must be pruned, extra \
             binaries and all"
        );
        assert_eq!(version_dirs(&versions).len(), VERSIONS_RETAINED);
    }

    /// **#139 — failure-safe.** Pruning must never turn a successful update
    /// into a failure. A `versions/` entry that isn't a prunable version
    /// directory (here a stray file) is skipped, every install still returns
    /// `Ok`, the pointer lands on the newest stack, and the stray is never
    /// mistaken for a version.
    #[test]
    #[serial]
    fn a_prune_hiccup_never_fails_the_install() {
        let home = tempfile::tempdir().unwrap();
        let _env = HomeEnv::set(home.path());

        let versions = home.path().join("versions");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::write(versions.join("not-a-version.txt"), b"stray").unwrap();

        for n in 1..=3 {
            let v = format!("0.0.{n}");
            assert!(
                stage_switch_and_prune(&v, &stack(&v)).is_ok(),
                "install {v} must succeed despite junk in versions/"
            );
        }

        assert_eq!(
            wylde_stack::current::current_dir(),
            Some(versions.join("0.0.3")),
            "the pointer must land on the newest install"
        );
        assert!(
            versions.join("not-a-version.txt").is_file(),
            "a non-version entry must be left untouched, never pruned"
        );
    }
}
