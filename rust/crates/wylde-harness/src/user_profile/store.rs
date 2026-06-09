//! Persistence for the user profile + its pending/rejected proposals.
//!
//! One JSON document at `<data_dir>/user_profile.json` holds the whole
//! subsystem state: the [`UserProfile`] itself, the pending
//! [`ProfileProposal`] queue, and the rejected-suppression log
//! ([`RejectedRecord`], OI-11). Mirrors the harness's other in-process
//! stores (`memory/short_term/store.rs`): atomic temp-write + rename,
//! a process-wide mutex against torn reads, and owner-only file perms.
//!
//! ## Encryption at rest (OI-14) — follow-up, not this slice
//!
//! Plan v2 §11.4 / OI-14 calls for file-level encryption-at-rest, on by
//! default, platform-native (Windows DPAPI). `wylde-shared` ships
//! [`harden_perms`](wylde_shared::secure_file::harden_perms) — an
//! owner-only DACL — but **no** encryption helper yet. Per the slice
//! brief, rather than reinvent crypto inside this Phase-2 slice we store
//! plain JSON hardened to owner-only and flag the DPAPI wrapping as a
//! follow-up to the storage-hygiene work (it belongs in `wylde-shared`
//! as a shared `secure_file`-adjacent helper, reused by `user_profile`,
//! `global_anchors`, and the workspace stores alike — one
//! implementation, not five). [`encrypt_at_rest_enabled`] reads the
//! gating env var today so the call site is ready the moment the helper
//! lands.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::memory::common::data_dir;
use crate::user_profile::profile::{ProfileProposal, RejectedRecord, UserProfile};

use serde::{Deserialize, Serialize};

/// Serialises in-process reads/writes of the single profile document.
/// Cross-process torn-write safety still rests on the atomic temp +
/// rename below, matching the short-term store.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// The whole on-disk document: profile + proposal queues.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileStore {
    #[serde(default)]
    pub profile: UserProfile,
    /// LLM proposals awaiting accept/edit/reject.
    #[serde(default)]
    pub pending: Vec<ProfileProposal>,
    /// Rejected proposals, kept for the OI-11 suppression window.
    #[serde(default)]
    pub rejected: Vec<RejectedRecord>,
}

/// `<data_dir>/user_profile.json` (Build Order Appendix C). Resolves
/// `data_dir()` on every call so tests can swap `WYLDE_DATA_DIR`.
pub fn path() -> PathBuf {
    data_dir().join("user_profile.json")
}

/// Whether encryption-at-rest is enabled (OI-14). Default **on**. Now backed
/// by the shared [`wylde_shared::encryption`] engine (DPAPI on Windows) — the
/// profile's reads/writes route through it, so a personal-facts file is
/// ciphertext on disk, not just owner-only plaintext.
pub fn encrypt_at_rest_enabled() -> bool {
    wylde_shared::encryption::is_encryption_enabled()
}

/// Read the store, returning [`ProfileStore::default`] when the file is
/// missing or unparseable (best-effort, matching the other harness
/// stores — a corrupt file reads as "empty profile", never an error the
/// caller has to handle on the read path).
pub fn read() -> ProfileStore {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    read_locked()
}

fn read_locked() -> ProfileStore {
    let path = path();
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&path) else {
        return ProfileStore::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Load the store, hand it to `f` for mutation, persist the result, and
/// return whatever `f` returns. The read→mutate→write is performed
/// under the store lock so two writers can't lose each other's edits.
pub fn with_store<F, R>(f: F) -> std::io::Result<R>
where
    F: FnOnce(&mut ProfileStore) -> R,
{
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = read_locked();
    let out = f(&mut store);
    write_locked(&store)?;
    Ok(out)
}

/// Persist `store`. Encrypt-at-rest (OI-14) + atomic temp-write + rename +
/// owner-only perms, all via the shared engine.
fn write_locked(store: &ProfileStore) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(store).expect("ProfileStore serialises to JSON");
    // The profile carries personal facts — encrypted + owner-only on disk.
    wylde_shared::encryption::write_at_rest(&path(), body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_profile::test_support::TestEnv;
    use serde_json::json;

    #[test]
    fn read_returns_default_when_missing() {
        let _env = TestEnv::new();
        let store = read();
        assert_eq!(store.profile, UserProfile::default());
        assert!(store.pending.is_empty());
        assert!(store.rejected.is_empty());
    }

    #[test]
    fn with_store_persists_and_reloads() {
        let _env = TestEnv::new();
        with_store(|s| {
            s.profile.name = Some("Aaron".into());
            s.profile.free_text_rules = "Be terse.".into();
        })
        .unwrap();
        let back = read();
        assert_eq!(back.profile.name.as_deref(), Some("Aaron"));
        assert_eq!(back.profile.free_text_rules, "Be terse.");
    }

    #[test]
    fn corrupt_file_reads_as_default() {
        let _env = TestEnv::new();
        std::fs::create_dir_all(path().parent().unwrap()).unwrap();
        std::fs::write(path(), "{ not json").unwrap();
        assert_eq!(read().profile, UserProfile::default());
    }

    #[test]
    fn with_store_returns_closure_value() {
        let _env = TestEnv::new();
        let n = with_store(|s| {
            s.profile.recurring_topics.push("rust".into());
            s.profile.recurring_topics.len()
        })
        .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn unknown_fields_survive_via_default_deser() {
        let _env = TestEnv::new();
        std::fs::create_dir_all(path().parent().unwrap()).unwrap();
        // A doc the way a real write produces it, plus a stray pending.
        std::fs::write(
            path(),
            serde_json::to_string_pretty(&json!({
                "profile": {"name": "A"},
                "pending": [],
                "rejected": [{"field": "name", "proposed": "B", "rejected_at": 10}]
            }))
            .unwrap(),
        )
        .unwrap();
        let store = read();
        assert_eq!(store.profile.name.as_deref(), Some("A"));
        assert_eq!(store.rejected.len(), 1);
    }

    #[test]
    fn encrypt_at_rest_defaults_on_and_honours_opt_out() {
        let prev = std::env::var("WYLDE_ENCRYPTION_AT_REST").ok();
        std::env::remove_var("WYLDE_ENCRYPTION_AT_REST");
        assert!(encrypt_at_rest_enabled());
        std::env::set_var("WYLDE_ENCRYPTION_AT_REST", "false");
        assert!(!encrypt_at_rest_enabled());
        match prev {
            Some(v) => std::env::set_var("WYLDE_ENCRYPTION_AT_REST", v),
            None => std::env::remove_var("WYLDE_ENCRYPTION_AT_REST"),
        }
    }
}
