//! One-way legacy-location adoption for the #250 data-root unification.
//!
//! Moving a resolver without moving the bytes is the whole hazard #138
//! deferred on: an existing install's starred default, per-model inference
//! overrides, routing profiles and paired devices all sit behind the *old*
//! path, and a store that quietly resolves somewhere new reads an empty
//! directory and reports it as "nothing configured". There is no error to
//! notice — it presents as "Wylde forgot my settings".
//!
//! So every store #250 moves calls one of these two helpers on the way to its
//! path, and the contract they share is:
//!
//! * **One-way.** Legacy → canonical, never the reverse. The legacy bytes are
//!   *copied*, never deleted or renamed: a downgrade to a pre-#250 build still
//!   finds its data, and a half-finished copy cannot destroy the only replica.
//! * **Never clobbers.** The copy happens only when the canonical location does
//!   not exist (for a tree: does not exist, or exists with nothing in it). A
//!   value written since the move — which is by construction newer than the
//!   legacy one — is never overwritten by a stale legacy value.
//! * **Idempotent.** Because the first successful copy creates the canonical
//!   location, every subsequent call sees it and no-ops. Running twice, or on
//!   every path resolution, does not duplicate, merge, or re-import.
//! * **Cheap enough to be unconditional.** The steady-state cost is one
//!   `exists()` stat, which is why callers can invoke it on each resolution
//!   instead of latching it behind a `OnceLock` — a latch would be wrong under
//!   the env rebinding every test suite here does.
//! * **Silent on failure.** These match the surrounding stores' fail-soft file
//!   handling: a copy that fails leaves the legacy bytes untouched and the
//!   canonical location absent, so the next call simply retries. The return
//!   value reports whether bytes moved, for tests and callers that care.

use std::path::Path;

/// True when `dir` exists and holds at least one entry. An empty directory
/// counts as "not yet populated" — services routinely `create_dir_all` their
/// store root before writing anything, and treating that as "already migrated"
/// would strand the legacy data behind an empty folder.
fn is_populated_dir(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Copy a single legacy file to its canonical location, once.
///
/// No-ops when: the two paths are the same (an env override can collapse
/// them), `canonical` already exists, or `legacy` is not a file. Returns
/// `true` only when bytes were actually copied.
pub fn adopt_legacy_file(legacy: &Path, canonical: &Path) -> bool {
    if legacy == canonical || canonical.exists() || !legacy.is_file() {
        return false;
    }
    if let Some(parent) = canonical.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    // Copy, never rename: the legacy replica has to survive so a downgrade
    // still reads it, and so a failure here cannot be a data loss.
    std::fs::copy(legacy, canonical).is_ok()
}

/// Copy a legacy directory tree to its canonical location, once.
///
/// No-ops when the two paths are the same, when `canonical` is already
/// populated, or when `legacy` is not a directory. Returns `true` when at
/// least one file was copied.
///
/// Deliberately not a merge: if the canonical tree has anything in it, this
/// build has already written there and the legacy tree is stale. Merging two
/// partially-diverged stores is how a "migration" resurrects deleted entries.
pub fn adopt_legacy_tree(legacy: &Path, canonical: &Path) -> bool {
    if legacy == canonical || !legacy.is_dir() || is_populated_dir(canonical) {
        return false;
    }
    copy_tree(legacy, canonical)
}

/// Recursive copy of `from` into `to`, creating `to`. Best-effort: an entry
/// that fails is skipped rather than aborting the rest, so one unreadable file
/// cannot strand every other store in the tree.
fn copy_tree(from: &Path, to: &Path) -> bool {
    if std::fs::create_dir_all(to).is_err() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return false;
    };
    let mut copied = false;
    for entry in entries.flatten() {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copied |= copy_tree(&src, &dst);
        } else if std::fs::copy(&src, &dst).is_ok() {
            copied = true;
        }
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    #[test]
    fn file_is_adopted_when_only_the_legacy_copy_exists() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("data").join("default_model.json");
        let canonical = td
            .path()
            .join(".wylde")
            .join("data")
            .join("default_model.json");
        write(&legacy, r#"{"model":"qwen3:0.6b"}"#);

        assert!(adopt_legacy_file(&legacy, &canonical), "bytes should move");
        assert_eq!(
            fs::read_to_string(&canonical).unwrap(),
            r#"{"model":"qwen3:0.6b"}"#
        );
        // One-way: the legacy replica survives, so a downgrade still reads it.
        assert!(legacy.is_file(), "legacy copy must be preserved");
    }

    #[test]
    fn file_adoption_never_clobbers_a_newer_canonical_value() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("legacy.json");
        let canonical = td.path().join("canonical.json");
        write(&legacy, r#"{"model":"stale"}"#);
        write(&canonical, r#"{"model":"fresh"}"#);

        assert!(!adopt_legacy_file(&legacy, &canonical));
        assert_eq!(
            fs::read_to_string(&canonical).unwrap(),
            r#"{"model":"fresh"}"#,
            "a value written after the move outranks the legacy one"
        );
    }

    #[test]
    fn file_adoption_is_idempotent() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("legacy.json");
        let canonical = td.path().join("sub").join("canonical.json");
        write(&legacy, "one");

        assert!(adopt_legacy_file(&legacy, &canonical));
        // The user edits the canonical value; a second run must not undo it.
        fs::write(&canonical, "two").unwrap();
        assert!(!adopt_legacy_file(&legacy, &canonical));
        assert!(!adopt_legacy_file(&legacy, &canonical));
        assert_eq!(fs::read_to_string(&canonical).unwrap(), "two");
    }

    #[test]
    fn file_adoption_no_ops_when_paths_collapse_to_one() {
        let td = tempdir().unwrap();
        let same = td.path().join("same.json");
        write(&same, "body");
        assert!(!adopt_legacy_file(&same, &same));
        assert_eq!(fs::read_to_string(&same).unwrap(), "body");
    }

    #[test]
    fn file_adoption_no_ops_when_there_is_nothing_to_adopt() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("absent.json");
        let canonical = td.path().join("canonical.json");
        assert!(!adopt_legacy_file(&legacy, &canonical));
        assert!(!canonical.exists(), "must not create an empty placeholder");
    }

    #[test]
    fn tree_is_adopted_recursively_and_preserves_the_legacy_copy() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("data").join("model_registry");
        let canonical = td.path().join(".wylde").join("data").join("model_registry");
        write(&legacy.join("profiles.json"), r#"{"chat":"a"}"#);
        write(&legacy.join("nested").join("swaps.json"), "[]");

        assert!(adopt_legacy_tree(&legacy, &canonical));
        assert_eq!(
            fs::read_to_string(canonical.join("profiles.json")).unwrap(),
            r#"{"chat":"a"}"#
        );
        assert_eq!(
            fs::read_to_string(canonical.join("nested").join("swaps.json")).unwrap(),
            "[]"
        );
        assert!(legacy.join("profiles.json").is_file());
    }

    #[test]
    fn tree_adoption_skips_a_populated_canonical_tree() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("legacy");
        let canonical = td.path().join("canonical");
        write(&legacy.join("profiles.json"), "stale");
        write(&canonical.join("profiles.json"), "fresh");

        assert!(!adopt_legacy_tree(&legacy, &canonical));
        assert_eq!(
            fs::read_to_string(canonical.join("profiles.json")).unwrap(),
            "fresh"
        );
    }

    /// A store that `create_dir_all`s its root before its first write is the
    /// common case, and treating that empty dir as "already migrated" would
    /// strand every legacy byte behind it.
    #[test]
    fn tree_adoption_still_runs_into_an_empty_canonical_dir() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("legacy");
        let canonical = td.path().join("canonical");
        write(&legacy.join("profiles.json"), "data");
        fs::create_dir_all(&canonical).unwrap();

        assert!(adopt_legacy_tree(&legacy, &canonical));
        assert_eq!(
            fs::read_to_string(canonical.join("profiles.json")).unwrap(),
            "data"
        );
    }

    #[test]
    fn tree_adoption_is_idempotent() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("legacy");
        let canonical = td.path().join("canonical");
        write(&legacy.join("profiles.json"), "one");

        assert!(adopt_legacy_tree(&legacy, &canonical));
        fs::write(canonical.join("profiles.json"), "two").unwrap();
        assert!(!adopt_legacy_tree(&legacy, &canonical));
        assert!(!adopt_legacy_tree(&legacy, &canonical));
        assert_eq!(
            fs::read_to_string(canonical.join("profiles.json")).unwrap(),
            "two",
            "a re-run must not resurrect the legacy value"
        );
    }

    #[test]
    fn tree_adoption_no_ops_when_there_is_no_legacy_tree() {
        let td = tempdir().unwrap();
        let legacy = td.path().join("absent");
        let canonical = td.path().join("canonical");
        assert!(!adopt_legacy_tree(&legacy, &canonical));
        assert!(!canonical.exists());
    }
}
