//! The Wylde-managed n8n engine contract shared by `wylde-lifecycle` (which
//! launches the engine) and `wylde-n8n` (which attaches to it).
//!
//! The engine is a Node runtime Wylde does not own. The lifecycle daemon is
//! the one crate allowed to spawn third-party processes, so it launches the
//! engine and *owns* its identity: the encryption key pinned into the
//! engine's environment and the owner account `wylde-n8n` provisions and
//! logs in as. `wylde-n8n` only reads that identity back. Keeping the file
//! format, the port and the opt-out switch here is what stops the two sides
//! drifting apart.
//!
//! Wylde is a single-user, local-first app: there is no Wylde login and
//! therefore no portable user session to forward (the OS user that owns
//! the machine *is* the Wylde identity). "Shared auth" is realised by
//! Wylde *owning* the n8n instance outright — it provisions the n8n
//! owner account itself, holds the credential on the user's behalf, and
//! presents the session to the embedded editor automatically.
//!
//! The credential is **Wylde-generated** (random), never the user's own
//! personal secret, and lives only inside the out-of-tree data dir
//! (`WyldeData/n8n/`, the same durable home as the workflow database).
//! It is written once and reused on every restart so the n8n encryption
//! key — which encrypts stored credentials at rest — stays stable and
//! the owner login keeps working across bounces.
//!
//! Nothing here is ever logged at value level — only presence/absence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default loopback port the engine binds. Overridable with
/// `WYLDE_N8N_PORT`. Kept at n8n's own default so the
/// `http://127.0.0.1:5678` upstream URL works untouched.
pub const DEFAULT_PORT: u16 = 5678;

/// File name under the data dir holding the Wylde-owned n8n identity.
const IDENTITY_FILE: &str = "wylde_n8n_identity.json";

/// The owner account's email. n8n needs *an* email and validates the
/// shape (a bare `@localhost` is rejected — it wants a dotted domain),
/// so this synthetic `.local` address is what n8n accepts. It is never
/// mailed; the owner exists only to authorise the loopback editor.
const OWNER_EMAIL: &str = "wylde-owner@wylde.local";

/// The engine's loopback port: `WYLDE_N8N_PORT`, else [`DEFAULT_PORT`].
pub fn port_from_env() -> u16 {
    std::env::var("WYLDE_N8N_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Whether Wylde manages the engine. `WYLDE_N8N_MANAGED=0` (or `false` /
/// `no`) opts out: lifecycle launches nothing and `wylde-n8n` is a pure
/// proxy in front of a user-managed n8n.
pub fn managed_from_env() -> bool {
    managed_from(std::env::var("WYLDE_N8N_MANAGED").ok().as_deref())
}

fn managed_from(raw: Option<&str>) -> bool {
    !matches!(raw, Some("0") | Some("false") | Some("no"))
}

/// The loopback base URL the engine answers on for `port`.
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The Wylde-owned n8n identity. Serialised to `wylde_n8n_identity.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct N8nIdentity {
    /// n8n `N8N_ENCRYPTION_KEY` — stable across restarts (load-bearing
    /// for decrypting saved credentials). 32 random bytes, hex.
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

    /// Load the identity from `data_dir`, minting and persisting a fresh
    /// one on first run. Idempotent: a second call returns the same
    /// persisted identity. **Only the engine's launcher calls this** — the
    /// key it mints is what the engine encrypts credentials with.
    ///
    /// A parse failure on an existing file is surfaced rather than
    /// silently overwritten — clobbering the encryption key would orphan
    /// every saved credential, so a corrupt file is a loud error the
    /// operator must resolve.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        if let Some(identity) = Self::load(data_dir)? {
            return Ok(identity);
        }
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create n8n data dir {}", data_dir.display()))?;
        let path = Self::path_in(data_dir);
        let identity = Self::mint();
        let blob = serde_json::to_string_pretty(&identity)?;
        std::fs::write(&path, blob.as_bytes())
            .with_context(|| format!("write n8n identity to {}", path.display()))?;
        if let Err(e) = crate::secure_file::harden_perms(&path) {
            tracing::warn!("n8n identity: could not restrict {}: {e}", path.display());
        }
        tracing::info!(
            "n8n identity: minted a fresh Wylde-owned identity at {} \
             (owner + encryption key; values never logged)",
            path.display()
        );
        Ok(identity)
    }

    /// Read the identity from `data_dir` without minting. `Ok(None)` when
    /// the engine's launcher has not written one yet. What `wylde-n8n`
    /// calls: it attaches to the engine, it never keys it.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path_in(data_dir);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read n8n identity at {}", path.display()))?;
        let identity = serde_json::from_str(&raw).with_context(|| {
            format!(
                "parse n8n identity at {} — refusing to overwrite a possibly-valid \
                 encryption key; fix or remove the file by hand",
                path.display()
            )
        })?;
        Ok(Some(identity))
    }

    /// Path the identity is persisted to under `data_dir`.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(IDENTITY_FILE)
    }
}

/// 32 random bytes (OS CSPRNG) as 64 lowercase hex chars.
fn random_hex_32() -> String {
    crate::rng::byte_array::<32>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A random password that satisfies n8n's owner-setup policy: a fixed
/// `Wq7` prefix guarantees an uppercase letter and a digit, followed by
/// 24 random bytes as hex (48 chars). Total 51 chars — within n8n's
/// 8..=64 bound. The randomness, not the prefix, carries the entropy.
fn random_password() -> String {
    let hex: String = crate::rng::byte_array::<24>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("Wq7{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let td = tempfile::tempdir().unwrap();
        assert_eq!(N8nIdentity::load(td.path()).unwrap(), None);
        let first = N8nIdentity::load_or_create(td.path()).unwrap();
        assert_eq!(first.owner_email, OWNER_EMAIL);
        assert_eq!(first.encryption_key.len(), 64);
        // n8n owner-setup policy: 8..=64 chars, an uppercase, a digit.
        assert!((8..=64).contains(&first.owner_password.len()));
        assert!(first.owner_password.chars().any(|c| c.is_ascii_uppercase()));
        assert!(first.owner_password.chars().any(|c| c.is_ascii_digit()));
        // Second call returns the SAME persisted identity — the
        // encryption key must never rotate under a live database.
        let second = N8nIdentity::load_or_create(td.path()).unwrap();
        assert_eq!(first, second);
        // ...and the attach side reads back exactly what the launcher minted.
        assert_eq!(N8nIdentity::load(td.path()).unwrap(), Some(first));
    }

    #[test]
    fn corrupt_identity_file_is_a_loud_error_not_a_silent_remint() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(N8nIdentity::path_in(td.path()), b"{ not json").unwrap();
        let err = N8nIdentity::load_or_create(td.path()).unwrap_err();
        assert!(
            err.to_string().contains("parse n8n identity"),
            "expected a parse error, got: {err}"
        );
    }

    #[test]
    fn managed_defaults_on_and_respects_optout() {
        assert!(managed_from(None));
        assert!(managed_from(Some("1")));
        assert!(!managed_from(Some("0")));
        assert!(!managed_from(Some("false")));
        assert!(!managed_from(Some("no")));
    }

    #[test]
    fn base_url_is_loopback_on_the_port() {
        assert_eq!(base_url(5999), "http://127.0.0.1:5999");
    }
}
