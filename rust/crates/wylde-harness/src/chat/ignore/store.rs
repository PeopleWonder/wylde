//! Global ignore-list persistence (Slice M) — the harness tier.
//!
//! One flat JSON array at `<data_dir>/global_ignore.json` (the harness
//! `data_dir`, beside `global_anchors.json` — the store it mirrors).
//! Encrypt-at-rest + atomic replace via the shared engine (OI-14);
//! fail-soft reads. Entries are `{token, added_at}` — the same shape the
//! workspace/conversation tiers persist in `wylde-workspaces` (duplicated
//! two-field struct rather than a `wylde-shared` lift; the wire shape is
//! pinned by both sides' tests).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::memory::common::data_dir;

/// `<data_dir>/global_ignore.json`.
pub fn global_ignore_path() -> PathBuf {
    data_dir().join("global_ignore.json")
}

/// One globally ignored token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IgnoreEntry {
    pub token: String,
    /// Unix seconds when the ignore was added.
    #[serde(default)]
    pub added_at: u64,
}

/// Load the global ignore list. Fail-soft: empty on a missing/torn file.
pub fn load() -> Vec<IgnoreEntry> {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&global_ignore_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(entries: &[IgnoreEntry]) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(entries).unwrap();
    wylde_shared::encryption::write_at_rest(&global_ignore_path(), body.as_bytes())
}

/// Ignore `token` globally. Idempotent — re-adding is a no-write success
/// (`Ok(false)`).
pub fn add(token: &str) -> std::io::Result<bool> {
    let token = token.trim();
    let mut all = load();
    if all.iter().any(|e| e.token == token) {
        return Ok(false);
    }
    all.push(IgnoreEntry {
        token: token.to_owned(),
        added_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    save(&all)?;
    Ok(true)
}

/// Stop ignoring `token` globally. `Ok(false)` when it wasn't ignored.
pub fn remove(token: &str) -> std::io::Result<bool> {
    let token = token.trim();
    let mut all = load();
    let before = all.len();
    all.retain(|e| e.token != token);
    let removed = all.len() != before;
    if removed {
        save(&all)?;
    }
    Ok(removed)
}

/// Is `token` globally ignored? (The turn driver's gather check.)
pub fn is_ignored(token: &str) -> bool {
    let token = token.trim();
    load().iter().any(|e| e.token == token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// Per-test `WYLDE_DATA_DIR` sandbox, sharing the process-wide env mutex
    /// every harness store test uses (the global-anchors idiom).
    struct Env {
        _g: MutexGuard<'static, ()>,
        _td: TempDir,
        prior: Option<std::ffi::OsString>,
    }
    impl Env {
        fn new() -> Self {
            let g = crate::memory::common::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().unwrap();
            let prior = std::env::var_os("WYLDE_DATA_DIR");
            std::env::set_var("WYLDE_DATA_DIR", td.path());
            Self {
                _g: g,
                _td: td,
                prior,
            }
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
                None => std::env::remove_var("WYLDE_DATA_DIR"),
            }
        }
    }

    #[test]
    fn add_remove_round_trip_is_idempotent() {
        let _env = Env::new();
        assert!(add("noisy_macro").unwrap());
        assert!(!add(" noisy_macro ").unwrap(), "trimmed duplicate no-op");
        assert!(is_ignored("noisy_macro"));
        assert_eq!(load().len(), 1);
        assert!(load()[0].added_at > 0);

        assert!(remove("noisy_macro").unwrap());
        assert!(!remove("noisy_macro").unwrap());
        assert!(!is_ignored("noisy_macro"));
        assert!(load().is_empty());
    }

    #[test]
    fn missing_file_is_empty() {
        let _env = Env::new();
        assert!(load().is_empty());
        assert!(!is_ignored("anything"));
    }
}
