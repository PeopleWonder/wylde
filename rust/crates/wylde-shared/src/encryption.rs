//! Encryption at rest for `data_dir` state files (OI-14).
//!
//! Sensitive local state — the user profile, the anchor vocabulary, saved
//! conversations, workspace registry/notes/persona — is encrypted on disk by
//! default, using the platform key store so the ciphertext is bound to the
//! current OS user. On Windows that is **DPAPI** (`CryptProtectData` /
//! `CryptUnprotectData`, per-user scope). No key material is ever stored by
//! Wylde; the OS holds it.
//!
//! ## File format
//!
//! An encrypted file starts with the ASCII magic [`ENC_HEADER`]
//! (`WYLDE-ENC-V1\n`) followed by the raw DPAPI blob. A file that does **not**
//! start with the magic is treated as plaintext — that is how a JSON/JSONL/MD
//! store written before encryption (or while the toggle was off) is still
//! read. Real JSON/Markdown never begins with `WYLDE-ENC-V1`, so detection is
//! unambiguous.
//!
//! ## Lazy migration
//!
//! [`read_to_string_at_rest`] decrypts a header-bearing file and, when it
//! reads a *plaintext* file while encryption is enabled, **re-writes it
//! encrypted** (best-effort). Existing installs migrate transparently on
//! first read; nothing has to convert the whole `data_dir` up front.
//!
//! ## Toggle
//!
//! [`is_encryption_enabled`] is **on by default**. It honours, in order:
//! the `WYLDE_ENCRYPTION_AT_REST` env override (tests / power users), then a
//! per-user pref file (`<data_dir>/encryption_at_rest.json`, written by the
//! Settings toggle via [`set_encryption_enabled`]), then the default (on).
//! When off, [`write_at_rest`] writes plaintext — so flipping the toggle off
//! rewrites each store as plaintext on its next save.
//!
//! ## Fallback
//!
//! If DPAPI is unavailable (a non-Windows build, or the API erroring),
//! [`encrypt_at_rest`] returns the data **unencrypted** and logs a warning —
//! the file is still hardened to owner-only by [`crate::secure_file`], which
//! is the pre-existing at-rest protection. Encryption is defence in depth on
//! top of that, never the only guard.

use std::io;
use std::path::{Path, PathBuf};

/// Magic prefix marking a DPAPI-encrypted file. Chosen so no valid
/// JSON/JSONL/Markdown store can collide with it.
pub const ENC_HEADER: &[u8] = b"WYLDE-ENC-V1\n";

/// `<data_dir>` resolved the same way the harness/workspaces services do
/// (`WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data`). Kept here so
/// `wylde-shared` can find the toggle pref file without depending on a
/// service crate.
fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join(".wylde").join("data")
}

/// `<data_dir>/encryption_at_rest.json` — the Settings toggle's persisted
/// state, read by every service so the choice applies process-wide.
fn pref_path() -> PathBuf {
    data_dir().join("encryption_at_rest.json")
}

/// Whether encryption at rest is enabled. **On by default.**
///
/// Resolution order: `WYLDE_ENCRYPTION_AT_REST` env (`0`/`false`/`off` ⇒ off,
/// anything else ⇒ on) → the pref file → default on.
pub fn is_encryption_enabled() -> bool {
    if let Ok(v) = std::env::var("WYLDE_ENCRYPTION_AT_REST") {
        return !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        );
    }
    match std::fs::read_to_string(pref_path()) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("enabled").and_then(serde_json::Value::as_bool))
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Persist the Settings toggle. Written owner-only; read by
/// [`is_encryption_enabled`] across services. The `WYLDE_ENCRYPTION_AT_REST`
/// env override, if set, still wins (it exists for tests / power users).
pub fn set_encryption_enabled(enabled: bool) -> io::Result<()> {
    let path = pref_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&serde_json::json!({ "enabled": enabled }))
        .unwrap_or_else(|_| "{\"enabled\":true}".to_owned());
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    let _ = crate::secure_file::harden_perms(&path);
    Ok(())
}

/// True when `data` carries the encrypted-file magic.
pub fn is_encrypted(data: &[u8]) -> bool {
    data.starts_with(ENC_HEADER)
}

/// Encrypt `data` for at-rest storage. When encryption is enabled and the
/// platform backend is available, returns [`ENC_HEADER`] followed by the
/// DPAPI blob. Otherwise (disabled, or no backend) returns `data` verbatim —
/// the caller's owner-only hardening remains the protection. Infallible by
/// design: a backend error degrades to plaintext rather than failing a save.
pub fn encrypt_at_rest(data: &[u8]) -> Vec<u8> {
    if !is_encryption_enabled() {
        return data.to_vec();
    }
    match backend_protect(data) {
        Ok(blob) => {
            let mut out = Vec::with_capacity(ENC_HEADER.len() + blob.len());
            out.extend_from_slice(ENC_HEADER);
            out.extend_from_slice(&blob);
            out
        }
        Err(e) => {
            tracing::warn!(error = %e, "encryption: backend unavailable, storing plaintext (owner-only)");
            data.to_vec()
        }
    }
}

/// Decrypt at-rest `data`. A header-bearing blob is DPAPI-decrypted; a
/// headerless (plaintext) blob is returned as-is (the migration source).
/// `Err` only when a header IS present but the blob can't be decrypted —
/// a corrupt or foreign-user file — so the caller can surface a clear error
/// rather than handing back garbage.
pub fn decrypt_at_rest(data: &[u8]) -> io::Result<Vec<u8>> {
    if !is_encrypted(data) {
        return Ok(data.to_vec());
    }
    let blob = &data[ENC_HEADER.len()..];
    backend_unprotect(blob).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("encryption: cannot decrypt at-rest file (corrupt or not this user's): {e}"),
        )
    })
}

/// Read a file and decrypt it. A plaintext file is returned verbatim and,
/// when encryption is enabled, **lazily migrated** to ciphertext on disk
/// (best-effort — a migration write failure never fails the read).
pub fn read_at_rest(path: &Path) -> io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    if is_encrypted(&raw) {
        return decrypt_at_rest(&raw);
    }
    // Plaintext on disk. Migrate it forward if we're meant to be encrypting.
    if is_encryption_enabled() && backend_available() {
        let _ = write_at_rest(path, &raw); // best-effort
    }
    Ok(raw)
}

/// [`read_at_rest`] decoded as UTF-8.
pub fn read_to_string_at_rest(path: &Path) -> io::Result<String> {
    let bytes = read_at_rest(path)?;
    String::from_utf8(bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("at-rest file not UTF-8: {e}"),
        )
    })
}

/// Encrypt (per [`encrypt_at_rest`]) and atomically write `data` to `path`,
/// then harden it owner-only. The single write path every `data_dir` store
/// routes through, so encryption + atomicity + permissions are applied in
/// exactly one place.
pub fn write_at_rest(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = encrypt_at_rest(data);
    // Per-process unique temp name so concurrent writers to sibling files
    // (or a retried write) never share a temp path.
    let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("store");
    let tmp = path.with_file_name(format!("{fname}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &payload)?;
    std::fs::rename(&tmp, path)?;
    let _ = crate::secure_file::harden_perms(path); // fail-soft
    Ok(())
}

// ── platform backend ─────────────────────────────────────────────────────

/// Whether the platform encryption backend is usable. Windows DPAPI is
/// always present; other platforms have no backend yet (fall back to
/// owner-only DACL/chmod — see module docs).
fn backend_available() -> bool {
    cfg!(windows)
}

#[cfg(windows)]
fn backend_protect(data: &[u8]) -> io::Result<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .map_err(|e| io::Error::other(format!("CryptProtectData: {e}")))?;
        let out = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut core::ffi::c_void));
        Ok(out)
    }
}

#[cfg(windows)]
fn backend_unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .map_err(|e| io::Error::other(format!("CryptUnprotectData: {e}")))?;
        let out = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut core::ffi::c_void));
        Ok(out)
    }
}

#[cfg(not(windows))]
fn backend_protect(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::other(
        "no at-rest encryption backend on this platform",
    ))
}

#[cfg(not(windows))]
fn backend_unprotect(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::other(
        "no at-rest encryption backend on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    /// `encryption_at_rest.json` + `WYLDE_*` env are process-global; serialise.
    static ENC_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct Env {
        _g: MutexGuard<'static, ()>,
        _td: TempDir,
        prev_data: Option<std::ffi::OsString>,
        prev_flag: Option<std::ffi::OsString>,
    }
    impl Env {
        fn new() -> Self {
            let g = ENC_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().unwrap();
            let prev_data = std::env::var_os("WYLDE_DATA_DIR");
            let prev_flag = std::env::var_os("WYLDE_ENCRYPTION_AT_REST");
            std::env::set_var("WYLDE_DATA_DIR", td.path());
            std::env::remove_var("WYLDE_ENCRYPTION_AT_REST");
            Self {
                _g: g,
                _td: td,
                prev_data,
                prev_flag,
            }
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            match self.prev_data.take() {
                Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
                None => std::env::remove_var("WYLDE_DATA_DIR"),
            }
            match self.prev_flag.take() {
                Some(v) => std::env::set_var("WYLDE_ENCRYPTION_AT_REST", v),
                None => std::env::remove_var("WYLDE_ENCRYPTION_AT_REST"),
            }
        }
    }

    #[test]
    fn enabled_by_default_and_env_override() {
        let _e = Env::new();
        assert!(is_encryption_enabled(), "default on");
        std::env::set_var("WYLDE_ENCRYPTION_AT_REST", "0");
        assert!(!is_encryption_enabled(), "env off");
        std::env::set_var("WYLDE_ENCRYPTION_AT_REST", "1");
        assert!(is_encryption_enabled(), "env on");
    }

    #[test]
    fn pref_file_toggles_when_env_unset() {
        let _e = Env::new();
        set_encryption_enabled(false).unwrap();
        assert!(!is_encryption_enabled(), "pref off honoured");
        set_encryption_enabled(true).unwrap();
        assert!(is_encryption_enabled(), "pref on honoured");
        // Env override still wins over the pref file.
        std::env::set_var("WYLDE_ENCRYPTION_AT_REST", "off");
        assert!(!is_encryption_enabled());
    }

    #[cfg(windows)]
    #[test]
    fn round_trip_through_dpapi() {
        let _e = Env::new(); // encryption on by default
        let payload = br#"{"secret":"value","n":42}"#;
        let enc = encrypt_at_rest(payload);
        assert!(is_encrypted(&enc), "header present");
        assert_ne!(&enc[ENC_HEADER.len()..], &payload[..], "ciphertext differs");
        let dec = decrypt_at_rest(&enc).unwrap();
        assert_eq!(dec, payload);
    }

    #[test]
    fn decrypt_passes_through_plaintext() {
        let _e = Env::new();
        let plain = br#"{"plain":true}"#;
        // Headerless input is returned verbatim (the lazy-migration source).
        assert_eq!(decrypt_at_rest(plain).unwrap(), plain);
    }

    #[test]
    fn corrupt_encrypted_file_is_clear_error() {
        let _e = Env::new();
        let mut corrupt = ENC_HEADER.to_vec();
        corrupt.extend_from_slice(b"not a real dpapi blob");
        let err = decrypt_at_rest(&corrupt).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // On a non-Windows build there is no backend; the error is still clear.
        assert!(err.to_string().contains("cannot decrypt"));
    }

    #[test]
    fn write_then_read_round_trips_on_disk() {
        let _e = Env::new();
        let path = data_dir().join("sub").join("store.json");
        let body = br#"{"k":"v"}"#;
        write_at_rest(&path, body).unwrap();
        // On disk it's encrypted (Windows) or plaintext (no backend).
        let raw = std::fs::read(&path).unwrap();
        if cfg!(windows) {
            assert!(is_encrypted(&raw), "stored encrypted on Windows");
        }
        assert_eq!(read_to_string_at_rest(&path).unwrap(), r#"{"k":"v"}"#);
    }

    #[cfg(windows)]
    #[test]
    fn toggle_off_writes_plaintext() {
        let _e = Env::new();
        std::env::set_var("WYLDE_ENCRYPTION_AT_REST", "0");
        let path = data_dir().join("plain.json");
        write_at_rest(&path, br#"{"k":"v"}"#).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(!is_encrypted(&raw), "plaintext when disabled");
        assert_eq!(read_to_string_at_rest(&path).unwrap(), r#"{"k":"v"}"#);
    }

    #[cfg(windows)]
    #[test]
    fn lazy_migration_encrypts_plaintext_on_read() {
        let _e = Env::new(); // on by default
        let path = data_dir().join("legacy.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Simulate a pre-encryption install: plaintext on disk.
        std::fs::write(&path, br#"{"legacy":true}"#).unwrap();
        assert!(!is_encrypted(&std::fs::read(&path).unwrap()));
        // Reading migrates it forward.
        let got = read_to_string_at_rest(&path).unwrap();
        assert_eq!(got, r#"{"legacy":true}"#);
        assert!(
            is_encrypted(&std::fs::read(&path).unwrap()),
            "migrated to ciphertext"
        );
        // Still readable after migration.
        assert_eq!(read_to_string_at_rest(&path).unwrap(), r#"{"legacy":true}"#);
    }
}
