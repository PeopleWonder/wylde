//! Root-jail path resolution — the single security boundary for the
//! `workspaces.fs.*` verbs (S1 / plan P0.2).
//!
//! Every caller-supplied path is resolved against a workspace's `folder` and
//! rejected if it escapes that root. The GUI process never touches arbitrary
//! disk paths itself; it routes every read/write/list through these verbs, so
//! this module is the *whole* file-access guardrail. Mirrors the indexer's
//! canonical-path discipline (`rag::indexer::walk::canonical_path`) so a
//! jailed path matches the form the index stores.
//!
//! ## The threat model and how each case is closed
//! - **Absolute paths / drive letters** (`/etc/passwd`, `C:\Windows`): rejected
//!   up front — the rel-path must be relative, with no root/prefix component.
//! - **`..` traversal** (`../../secrets`): rejected up front — no `ParentDir`
//!   component is allowed in the input at all (defence-in-depth; the canonical
//!   check below would also catch a net-escape, but we never even build it).
//! - **Symlink-out** (a link inside the workspace pointing elsewhere):
//!   `canonicalize()` resolves the link, and [`ensure_within`] then rejects the
//!   resolved target because it no longer sits under the canonical root.
//! - **UNC / verbatim prefixes**: any explicit path `Prefix` component in the
//!   *input* is rejected; the canonical root carries its own `\\?\` prefix and
//!   is only ever compared against another canonicalized path.

use std::path::{Component, Path, PathBuf};

use crate::error::{Result, WorkspacesError};

/// Canonicalize a workspace `folder` into the root every resolved path must
/// sit under. Fails if the folder is missing/unreadable — a workspace whose
/// folder has gone away can't serve files.
fn canonical_root(folder: &str) -> Result<PathBuf> {
    Path::new(folder).canonicalize().map_err(|e| {
        WorkspacesError::PathEscape(format!("workspace folder unavailable ({folder:?}): {e}"))
    })
}

/// Reject a caller path that is anything other than a *relative* path made of
/// plain components. No absolute paths, no drive/UNC prefixes, no `..`, no
/// leading separator. Runs before any filesystem touch.
fn reject_hostile(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(WorkspacesError::PathEscape(format!(
            "absolute path not allowed: {rel:?}"
        )));
    }
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(WorkspacesError::PathEscape(format!(
                    "rooted/prefixed path not allowed: {rel:?}"
                )));
            }
            Component::ParentDir => {
                return Err(WorkspacesError::PathEscape(format!(
                    "`..` traversal not allowed: {rel:?}"
                )));
            }
            // CurDir (`.`) and Normal components are fine.
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Is `candidate` the same as, or nested under, `root`? Both must already be
/// canonical (component-wise `starts_with`, so no string-prefix false hits like
/// `/proj2` under `/proj`).
fn ensure_within(root: &Path, candidate: &Path) -> Result<()> {
    if candidate.starts_with(root) {
        Ok(())
    } else {
        Err(WorkspacesError::PathEscape(format!(
            "path escapes workspace root: {candidate:?}"
        )))
    }
}

/// Resolve `rel_path` against a workspace `folder`, enforcing the root jail.
///
/// `require_exists`:
/// - `true` (read / list_dir): the target must exist; it is fully
///   canonicalized (resolving any symlinks) and then jailed.
/// - `false` (write): the *leaf* may not exist yet, so the parent directory is
///   canonicalized and jailed and the leaf re-joined — a write can create a new
///   file but still cannot escape the root (even through a symlinked parent).
///
/// An empty or `"."` `rel_path` resolves to the workspace root itself (used by
/// `list_dir` to enumerate the top level).
pub fn resolve(folder: &str, rel_path: &str, require_exists: bool) -> Result<PathBuf> {
    let root = canonical_root(folder)?;
    reject_hostile(rel_path)?;

    let trimmed = rel_path.trim();
    let is_root_ref = trimmed.is_empty() || trimmed == ".";
    let joined = if is_root_ref {
        root.clone()
    } else {
        root.join(trimmed)
    };

    if require_exists {
        let canon = joined.canonicalize().map_err(WorkspacesError::Io)?;
        ensure_within(&root, &canon)?;
        Ok(canon)
    } else {
        if is_root_ref {
            return Err(WorkspacesError::BadRequest(
                "write target must name a file, not the workspace root".into(),
            ));
        }
        let parent = joined.parent().ok_or_else(|| {
            WorkspacesError::BadRequest(format!("path has no parent directory: {rel_path:?}"))
        })?;
        let leaf = joined.file_name().ok_or_else(|| {
            WorkspacesError::BadRequest(format!("path has no file name: {rel_path:?}"))
        })?;
        let canon_parent = parent.canonicalize().map_err(WorkspacesError::Io)?;
        ensure_within(&root, &canon_parent)?;
        Ok(canon_parent.join(leaf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_a_normal_nested_file() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("main.rs"), "fn main(){}").unwrap();

        let got = resolve(&root.to_string_lossy(), "src/main.rs", true).unwrap();
        assert!(got.ends_with("main.rs"));
        // It sits under the canonical root.
        assert!(got.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn rejects_absolute_paths() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        // POSIX absolute and Windows drive-absolute.
        for bad in ["/etc/passwd", r"C:\Windows\system32\drivers\etc\hosts"] {
            let err = resolve(&root, bad, true).unwrap_err();
            assert_eq!(err.code(), "path_escape", "input {bad:?}");
        }
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        for bad in ["../secrets", "src/../../escape", "a/b/../../../c"] {
            let err = resolve(&root, bad, true).unwrap_err();
            assert_eq!(err.code(), "path_escape", "input {bad:?}");
        }
    }

    #[test]
    fn rejects_leading_separator() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        // A leading separator is a RootDir component → rejected.
        let err = resolve(&root, "/abs-ish", true).unwrap_err();
        assert_eq!(err.code(), "path_escape");
    }

    #[test]
    fn missing_existing_target_is_io_not_escape() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        // A clean relative path that simply doesn't exist → io (not a jail
        // breach). The editor surfaces this as not-found, not a security error.
        let err = resolve(&root, "does/not/exist.rs", true).unwrap_err();
        assert_eq!(err.code(), "io");
    }

    #[test]
    fn write_allows_missing_leaf_under_root() {
        let td = tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir(root.join("src")).unwrap();
        // Leaf doesn't exist yet — write resolution still succeeds because the
        // parent exists and is in-jail.
        let got = resolve(&root.to_string_lossy(), "src/new_file.rs", false).unwrap();
        assert!(got.ends_with("new_file.rs"));
        assert!(got.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn write_rejects_root_reference() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        for r in ["", "."] {
            let err = resolve(&root, r, false).unwrap_err();
            assert_eq!(err.code(), "bad_request", "input {r:?}");
        }
    }

    #[test]
    fn empty_path_resolves_root_for_listing() {
        let td = tempdir().unwrap();
        let root = td.path();
        let got = resolve(&root.to_string_lossy(), "", true).unwrap();
        assert_eq!(got, root.canonicalize().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unc_prefix_input() {
        let td = tempdir().unwrap();
        let root = td.path().to_string_lossy().into_owned();
        let err = resolve(&root, r"\\server\share\file", true).unwrap_err();
        assert_eq!(err.code(), "path_escape");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_out_is_rejected() {
        use std::os::unix::fs::symlink;
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        let td = tempdir().unwrap();
        let root = td.path();
        // A symlink inside the workspace pointing at an outside file.
        symlink(outside.path().join("secret.txt"), root.join("link.txt")).unwrap();
        let err = resolve(&root.to_string_lossy(), "link.txt", true).unwrap_err();
        assert_eq!(err.code(), "path_escape", "symlink-out must be jailed");
    }
}
