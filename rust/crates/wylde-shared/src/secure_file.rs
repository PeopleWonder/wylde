//! Owner-only filesystem permission hardening.
//!
//! Rust counterpart of `Core/shared/secure_file.py`. Sensitive state
//! files — device bearer tokens, password hashes — must not be left
//! world-readable. [`harden_perms`] restricts a file to the current
//! user: `chmod 0o600` on POSIX, and on Windows a *protected* DACL
//! carrying a single Full-Control ACE for the file's owner (installing
//! a protected DACL also strips inherited ACEs).
//!
//! The Windows path calls the Win32 security API directly via the
//! `windows` crate — it does **not** shell out to `icacls`. Spawning
//! external processes is reserved for the `wylde-lifecycle` crate
//! (wylde_check rule 29).
//!
//! Fail-soft by design. A hardening failure is logged and swallowed —
//! never propagated — so it cannot break the atomic-rename write path
//! it is meant to run *after*.

use std::path::Path;

/// Restrict a file to owner-only access.
///
/// On POSIX: `chmod 0o600`.
/// On Windows: replace the DACL with a single Full-Control ACE for the
/// file's owner and mark it protected, which also strips inherited ACEs.
///
/// No-op gracefully if the file doesn't exist or the platform call
/// fails — a warning is logged rather than raised. Always returns
/// `Ok(())`; the `Result` is kept for signature parity with callers
/// that thread it through.
pub fn harden_perms(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        tracing::warn!(path = %path.display(), "secure_file: cannot harden missing path");
        return Ok(());
    }
    #[cfg(unix)]
    harden_unix(path);
    #[cfg(windows)]
    harden_windows(path);
    Ok(())
}

#[cfg(unix)]
fn harden_unix(path: &Path) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, Permissions::from_mode(0o600)) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "secure_file: chmod 0o600 failed",
        );
    }
}

#[cfg(windows)]
fn harden_windows(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, InitializeAcl, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    // Wide, NUL-terminated path for the W-suffixed APIs.
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let name = PCWSTR(wide.as_ptr());

    let outcome: windows::core::Result<()> = unsafe {
        // 1. Read the file's owner SID. For a file this process just
        //    wrote, the owner is the current user.
        let mut owner_sid = PSID::default();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let read = GetNamedSecurityInfoW(
            name,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner_sid),
            None,
            None,
            None,
            &mut psd,
        );
        if let Err(e) = read.ok() {
            Err(e)
        } else {
            // 2. Build a one-ACE DACL: the owner gets Full Control,
            //    nobody else. The buffer is u32-backed so the ACL lands
            //    DWORD-aligned, as the Win32 API requires.
            let sid_len = GetLengthSid(owner_sid) as usize;
            let acl_len =
                size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_len;
            let mut acl_buf: Vec<u32> = vec![0; acl_len.div_ceil(size_of::<u32>())];
            let acl = acl_buf.as_mut_ptr().cast::<ACL>();
            let built = InitializeAcl(acl, acl_len as u32, ACL_REVISION).and_then(|()| {
                AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS.0, owner_sid)
            });
            let result = match built {
                Err(e) => Err(e),
                // 3. Install it as a *protected* DACL — that strips the
                //    inherited ACEs the file picked up from its parent.
                Ok(()) => SetNamedSecurityInfoW(
                    name,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    // owner/group unchanged: the security-info flags request
                    // only the DACL, so these are ignored. windows 0.62 models
                    // the old null-PSID sentinel as `None`.
                    None,
                    None,
                    Some(acl.cast_const()),
                    None,
                )
                .ok(),
            };
            // Free the descriptor GetNamedSecurityInfoW allocated for us.
            let _ = LocalFree(Some(HLOCAL(psd.0)));
            result
        }
    };
    if let Err(e) = outcome {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "secure_file: Windows ACL harden failed",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_path_is_noop() {
        let tmp = TempDir::new().expect("tempdir");
        // No file at the path — must return Ok, not panic.
        harden_perms(&tmp.path().join("does-not-exist.json")).expect("noop");
    }

    #[test]
    fn content_intact_after_harden() {
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("state.json");
        let payload = r#"{"secret":"value"}"#;
        std::fs::write(&target, payload).expect("write");
        harden_perms(&target).expect("harden");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), payload);
    }

    #[cfg(unix)]
    #[test]
    fn posix_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("state.json");
        std::fs::write(&target, "x").expect("write");
        harden_perms(&target).expect("harden");
        let mode = std::fs::metadata(&target)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// After hardening, the file's DACL holds exactly one ACE. A temp
    /// file normally inherits several ACEs from its parent directory;
    /// a count of one proves both the owner-only grant and the
    /// inheritance strip landed.
    #[cfg(windows)]
    #[test]
    fn windows_dacl_has_single_ace() {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows::Win32::Security::{
            AclSizeInformation, GetAclInformation, ACL, ACL_SIZE_INFORMATION,
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        };

        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("state.json");
        std::fs::write(&target, "x").expect("write");
        harden_perms(&target).expect("harden");

        let wide: Vec<u16> = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let ace_count = unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut psd,
            )
            .ok()
            .expect("read dacl");
            let mut info = ACL_SIZE_INFORMATION::default();
            GetAclInformation(
                dacl,
                core::ptr::from_mut(&mut info).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
            .expect("acl info");
            let _ = LocalFree(Some(HLOCAL(psd.0)));
            info.AceCount
        };
        assert_eq!(ace_count, 1, "DACL must carry exactly one ACE");
    }
}
