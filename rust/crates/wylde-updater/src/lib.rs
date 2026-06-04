//! Wylde in-app self-updater (Phase 12.5).
//!
//! Pulls newer `wylde-gui` builds from the project's **public** GitHub
//! Releases (`PeopleWonder/wylde`), verifies them against one embedded
//! minisign/Ed25519 public key, and swaps the running binary on Windows.
//!
//! Design rationale lives in `docs/self-updater-design.md`. Properties this
//! crate guarantees:
//!
//! * **Privacy.** The only outbound call is an unauthenticated `GET` to the
//!   public GitHub REST API. No identity, token, or fingerprint is sent.
//! * **Fail-closed verification.** No code path installs a binary that
//!   hasn't passed [`verify_signature`] against the embedded key. An
//!   un-keyed (placeholder) build refuses to install at all.
//! * **Blocking, runtime-free.** The whole API is synchronous. The GUI,
//!   which has no tokio reactor on its executor, drives it via the Pipe
//!   crate's `bridged_spawn_blocking`.
//!
//! ## Typical flow
//!
//! ```no_run
//! use wylde_updater::{check_for_update, download_release, install_update, Channel, UpdateStatus};
//!
//! let status = check_for_update(Channel::Stable, env!("CARGO_PKG_VERSION"))?;
//! if let UpdateStatus::Available(info) = status {
//!     let dl = download_release(&info)?;
//!     // install_update re-verifies before touching the running binary.
//!     install_update(&dl.bytes, &dl.minisig)?;
//!     // ...prompt the user to restart to apply.
//! }
//! # Ok::<(), wylde_updater::UpdateError>(())
//! ```

mod pubkey;
mod release;
mod verify;

use std::io::Read;
use std::path::{Path, PathBuf};

pub use pubkey::{has_signing_key, PUBLIC_KEY};
pub use release::{Channel, Release, ReleaseAsset, UpdateInfo, UpdateStatus};
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
    /// The release has no recognisable `wylde-gui` binary asset.
    #[error("release has no installable binary asset")]
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

/// The downloaded binary bytes plus the text of its `.minisig` signature.
#[derive(Debug, Clone)]
pub struct DownloadedRelease {
    pub bytes: Vec<u8>,
    pub minisig: String,
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

/// Download the binary + its signature for a resolved [`UpdateInfo`].
pub fn download_release(info: &UpdateInfo) -> Result<DownloadedRelease, UpdateError> {
    tracing::info!(version = %info.version, "downloading update");
    let bytes = http_get_bytes(&info.binary.url)?;
    let minisig = http_get_text(&info.signature.url)?;
    Ok(DownloadedRelease { bytes, minisig })
}

/// Verify, stage, and swap the running binary with the downloaded update.
///
/// Re-verifies the signature (defence in depth) before writing a single
/// byte near the running executable, so this is safe to call even if the
/// caller skipped an explicit [`verify_signature`]. On success the new
/// binary takes effect on the **next launch** — prompt the user to
/// restart.
pub fn install_update(bytes: &[u8], minisig: &str) -> Result<(), UpdateError> {
    verify_signature(bytes, minisig)?;
    let staged = stage_update(bytes)?;
    let result = self_replace::self_replace(&staged)
        .map_err(|e| UpdateError::Io(format!("self-replace failed: {e}")));
    // The staged temp file has served its purpose either way; best-effort
    // cleanup so we don't leave a stray binary beside the exe. A failure
    // here is harmless (a leftover .update file is inert).
    let _ = std::fs::remove_file(&staged); // wylde-check: discard-result-ok
    result?;
    tracing::info!("update installed; will apply on next launch");
    Ok(())
}

/// Name of the staged update file, dropped next to the running exe.
#[cfg(windows)]
const STAGED_NAME: &str = "wylde-gui.update.exe";
#[cfg(not(windows))]
const STAGED_NAME: &str = "wylde-gui.update";

/// Write `bytes` to a staging file next to the running executable and
/// return its path. Split from [`install_update`] (which then calls
/// `self-replace`) so the write/sanity-check seam is unit-testable
/// without replacing the test runner.
fn stage_update(bytes: &[u8]) -> Result<PathBuf, UpdateError> {
    let exe = std::env::current_exe().map_err(|e| UpdateError::Io(format!("current_exe: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| UpdateError::Io("running executable has no parent dir".into()))?;
    stage_update_in(dir, bytes)
}

fn stage_update_in(dir: &Path, bytes: &[u8]) -> Result<PathBuf, UpdateError> {
    if bytes.is_empty() {
        return Err(UpdateError::Io("refusing to stage an empty update payload".into()));
    }
    let path = dir.join(STAGED_NAME);
    std::fs::write(&path, bytes).map_err(|e| UpdateError::Io(format!("staging write: {e}")))?;
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
    Ok(path)
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

    #[test]
    fn stage_update_writes_payload_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage_update_in(dir.path(), b"new binary bytes").unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"new binary bytes");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), STAGED_NAME);
    }

    #[test]
    fn stage_update_refuses_empty_payload() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            stage_update_in(dir.path(), b""),
            Err(UpdateError::Io(_))
        ));
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
}
