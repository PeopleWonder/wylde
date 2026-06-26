//! Wylde-owned n8n identity, persisted in the out-of-tree data dir.
//!
//! Wylde is a single-user, local-first app: there is no Wylde login and
//! therefore no portable user session to forward (the OS user that owns
//! the machine *is* the Wylde identity). "Shared auth" is realised by
//! Wylde *owning* the n8n instance outright — it provisions the n8n
//! owner account itself, holds the credential on the user's behalf, and
//! presents the session to the embedded editor automatically. The user
//! never sees an n8n login screen because Wylde already authenticated on
//! their behalf.
//!
//! The credential here is **Wylde-generated** (random), never the user's
//! own personal secret, and lives only inside the out-of-tree data dir
//! (`WyldeData/n8n/`, the same durable home as the workflow database).
//! It is written once and reused on every restart so the n8n encryption
//! key — which encrypts stored credentials at rest — stays stable and
//! the owner login keeps working across bounces.
//!
//! Three fields:
//!   * `encryption_key` — n8n's `N8N_ENCRYPTION_KEY`. MUST be stable
//!     across restarts or n8n can't decrypt previously-saved
//!     credentials. 32 random bytes, hex.
//!   * `owner_email` / `owner_password` — the n8n owner account Wylde
//!     provisions and logs in as. The password is 32 random bytes, hex.
//!
//! Nothing here is ever logged at value level — only presence/absence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// File name under the data dir holding the Wylde-owned n8n identity.
const SECRET_FILE: &str = "wylde_n8n_identity.json";

/// The owner account's email. n8n needs *an* email and validates the
/// shape (a bare `@localhost` is rejected — it wants a dotted domain),
/// so this synthetic `.local` address is what n8n accepts. It is never
/// mailed; the owner exists only to authorise the loopback editor.
const OWNER_EMAIL: &str = "wylde-owner@wylde.local";

/// The Wylde-owned n8n identity. Serialised to `wylde_n8n_identity.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct N8nIdentity {
    /// n8n `N8N_ENCRYPTION_KEY` — stable across restarts (load-bearing
    /// for decrypting saved credentials).
    pub encryption_key: String,
    /// Owner account email (synthetic, loopback-only).
    pub owner_email: String,
    /// Owner account password — Wylde-generated, never the user's own.
    pub owner_password: String,
}

impl N8nIdentity {
    /// Mint a fresh identity from the OS CSPRNG. The encryption key is
    /// 32 random bytes (64 hex chars); the owner password is a random
    /// secret that satisfies n8n's policy (≥8 chars, an uppercase, and a
    /// digit — n8n rejects all-lowercase-hex at owner setup).
    fn mint() -> Self {
        Self {
            encryption_key: random_hex_32(),
            owner_email: OWNER_EMAIL.to_owned(),
            owner_password: random_password(),
        }
    }

    /// Load the identity from `data_dir/wylde_n8n_identity.json`, minting
    /// and persisting a fresh one on first run. Idempotent: a second
    /// call returns the same persisted identity.
    ///
    /// A parse failure on an existing file is surfaced rather than
    /// silently overwritten — clobbering the encryption key would orphan
    /// every saved credential, so a corrupt file is a loud error the
    /// operator must resolve.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(SECRET_FILE);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read n8n identity at {}", path.display()))?;
            let identity: N8nIdentity = serde_json::from_str(&raw).with_context(|| {
                format!(
                    "parse n8n identity at {} — refusing to overwrite a possibly-valid \
                     encryption key; fix or remove the file by hand",
                    path.display()
                )
            })?;
            return Ok(identity);
        }
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create n8n data dir {}", data_dir.display()))?;
        let identity = Self::mint();
        let blob = serde_json::to_string_pretty(&identity)?;
        write_private(&path, blob.as_bytes())
            .with_context(|| format!("write n8n identity to {}", path.display()))?;
        tracing::info!(
            "wylde-n8n: minted a fresh Wylde-owned n8n identity at {} \
             (owner + encryption key; values never logged)",
            path.display()
        );
        Ok(identity)
    }

    /// Path the identity is persisted to under `data_dir`.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(SECRET_FILE)
    }
}

/// 32 random bytes (OS CSPRNG) as 64 lowercase hex chars.
fn random_hex_32() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random password that satisfies n8n's owner-setup policy: a fixed
/// `Wq7` prefix guarantees an uppercase letter and a digit, followed by
/// 24 random bytes as hex (48 chars). Total 51 chars — within n8n's
/// 8..=64 bound. The randomness, not the prefix, carries the entropy.
fn random_password() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("Wq7{hex}")
}

/// Write `bytes` to `path`, best-effort restricting the file to the
/// owner. On Windows the inherited ACL of the per-user `WyldeData` dir
/// already scopes it to the OS user; on Unix we set mode 0600. The
/// secret never leaves loopback either way.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "wylde-n8n-secret-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    #[test]
    fn random_hex_is_64_chars_and_varies() {
        let a = random_hex_32();
        let b = random_hex_32();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two mints must differ (CSPRNG)");
    }

    #[test]
    fn load_or_create_mints_then_is_stable() {
        let td = tempdir("stable");
        let first = N8nIdentity::load_or_create(&td.0).unwrap();
        assert_eq!(first.owner_email, OWNER_EMAIL);
        assert_eq!(first.encryption_key.len(), 64);
        // n8n owner-setup policy: 8..=64 chars, an uppercase, a digit.
        assert!((8..=64).contains(&first.owner_password.len()));
        assert!(first.owner_password.chars().any(|c| c.is_ascii_uppercase()));
        assert!(first.owner_password.chars().any(|c| c.is_ascii_digit()));
        // Second call returns the SAME persisted identity — the
        // encryption key must never rotate under a live database.
        let second = N8nIdentity::load_or_create(&td.0).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn corrupt_identity_file_is_a_loud_error_not_a_silent_remint() {
        let td = tempdir("corrupt");
        std::fs::write(N8nIdentity::path_in(&td.0), b"{ not json").unwrap();
        let err = N8nIdentity::load_or_create(&td.0).unwrap_err();
        assert!(
            err.to_string().contains("parse n8n identity"),
            "expected a parse error, got: {err}"
        );
    }
}
