//! Where the **current** stack lives, and how it is repointed atomically.
//!
//! # The bug this replaces
//!
//! `launch_wylde.ps1` used to resolve each binary independently, taking the
//! first hit across `rust/bin` → `rust/target/release` → `rust/target/debug`.
//! Two failures fall out of that:
//!
//! * **Shadowing.** A stale artifact at an earlier candidate wins over a
//!   fresh build forever. Rebuild all you like; the launcher still runs the
//!   old one. This is what actually bit — the running stack was ~3 days
//!   behind the tree with nothing indicating it.
//! * **Profile mixing.** Because the walk is *per binary*, one launch could
//!   take the daemon from `target/release` and a service from `rust/bin`.
//!   There is no version relationship between those, so the stack is
//!   internally inconsistent in a way nothing reports.
//!
//! # The replacement
//!
//! Resolution picks **one directory for the whole in-tree stack** and takes
//! every binary from it. Two modes:
//!
//! * [`Source::Current`] — a pointer at `%LOCALAPPDATA%\Wylde\current` names
//!   the stack directory the updater last installed. This is what an
//!   installed user gets, and it is why a shortcut can never go stale: the
//!   shortcut names the launcher, the launcher reads the pointer, and the
//!   updater [`set_current`]s the pointer atomically.
//! * [`Source::BuildTree`] — **no pointer present**, so fall back to the repo
//!   build tree. This is the dev-rig path and it is deliberately preserved:
//!   the daemon directory is chosen by exactly the candidate order the old
//!   script used, so a machine with no pointer resolves the daemon to the
//!   same file it always did. What changed is that the *rest* of the stack
//!   now comes from that same directory instead of restarting the walk.
//!
//! Bucket siblings (`Services/<name>/`) resolve beside their own manifest in
//! build-tree mode — they are not built into the profile directories. That is
//! not profile drift; it is where those binaries actually live.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::roster::{roster_in, StackBinary, Tier};
use crate::{service_name as sn, wylde_root, StackError};

/// Points directly at a stack directory, bypassing the pointer file.
/// Primarily a test seam; also the escape hatch for running an explicit
/// stack without installing it.
pub const CURRENT_DIR_ENV: &str = "WYLDE_CURRENT";

/// Relocates the pointer file itself (the directory that holds `current`).
/// Exists so tests never touch the real `%LOCALAPPDATA%`.
pub const HOME_DIR_ENV: &str = "WYLDE_HOME";

/// Where the in-tree backend binaries are looked for, in order. Identical to
/// the candidate list the old launcher and `services::rust_binary_path` walk
/// — but consulted **once**, to choose a directory, not once per binary.
const BACKEND_PROFILE_DIRS: &[&[&str]] = &[
    &["rust", "bin"],
    &["rust", "target", "release"],
    &["rust", "target", "debug"],
];

/// Where the gpui shell is looked for, in order. `wylde-gui` builds out of
/// the standalone `Core/GUI/` workspace, so it is never under `rust/target/`.
const GUI_PROFILE_DIRS: &[&[&str]] = &[
    &["Core", "GUI", "target", "release"],
    &["Core", "GUI", "target", "debug"],
];

/// How the stack was located.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// An installed stack, named by the `current` pointer.
    Current { dir: PathBuf },
    /// No pointer — the repo build tree. Carries the single directory chosen
    /// for the backend and the single one chosen for the GUI, so a launch log
    /// records exactly which profile ran.
    BuildTree {
        backend: Option<PathBuf>,
        gui: Option<PathBuf>,
    },
}

/// One roster entry resolved to a concrete file (or found missing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedBinary {
    pub name: String,
    pub tier: Tier,
    /// The file to run. `None` when the binary is not present — a service
    /// that simply has not been built is a normal, non-fatal state.
    pub path: Option<PathBuf>,
}

/// The whole stack, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedStack {
    pub source: Source,
    pub binaries: Vec<ResolvedBinary>,
}

impl ResolvedStack {
    pub fn get(&self, name: &str) -> Option<&ResolvedBinary> {
        self.binaries.iter().find(|b| b.name == name)
    }

    /// The path for `name`, if it resolved to a real file.
    pub fn path_of(&self, name: &str) -> Option<&Path> {
        self.get(name).and_then(|b| b.path.as_deref())
    }

    /// Roster entries with no binary on disk. Informational: the launcher
    /// logs these so "service X never came up" is diagnosable at a glance
    /// instead of being silent.
    pub fn missing(&self) -> Vec<&str> {
        self.binaries
            .iter()
            .filter(|b| b.path.is_none())
            .map(|b| b.name.as_str())
            .collect()
    }

    /// The daemon must be runnable for a launch to mean anything.
    pub fn daemon(&self) -> Result<&Path, StackError> {
        self.path_of(sn::LIFECYCLE).ok_or_else(|| {
            StackError::NotFound(format!(
                "no {}{} found (searched: {})",
                sn::LIFECYCLE,
                crate::EXE_SUFFIX,
                self.searched_hint()
            ))
        })
    }

    pub fn gui(&self) -> Result<&Path, StackError> {
        self.path_of(sn::GUI).ok_or_else(|| {
            StackError::NotFound(format!(
                "no {}{} found (searched: {})",
                sn::GUI,
                crate::EXE_SUFFIX,
                self.searched_hint()
            ))
        })
    }

    fn searched_hint(&self) -> String {
        match &self.source {
            Source::Current { dir } => dir.display().to_string(),
            Source::BuildTree { backend, gui } => format!(
                "backend={}, gui={}",
                backend
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
                gui.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
            ),
        }
    }
}

/// The pointer file: `<home>/current`, where `<home>` is `WYLDE_HOME` or
/// `%LOCALAPPDATA%\Wylde`. Its content is the path of the stack directory.
///
/// A *file* holding a path rather than a directory junction: junction
/// creation needs privileges on some Windows configurations, and a one-line
/// text file is atomically replaceable everywhere (see [`set_current`]).
pub fn pointer_path() -> Option<PathBuf> {
    Some(wylde_home()?.join("current"))
}

fn wylde_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(HOME_DIR_ENV) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share").into())
        })?;
    Some(PathBuf::from(base).join("Wylde"))
}

/// The directory the `current` pointer names, if the pointer exists and names
/// a real directory. A pointer naming a vanished directory is treated as
/// absent — the launcher falls back rather than refusing to start.
pub fn current_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(CURRENT_DIR_ENV) {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
    }
    let pointer = pointer_path()?;
    let raw = fs::read_to_string(&pointer).ok()?;
    let dir = PathBuf::from(raw.trim());
    dir.is_dir().then_some(dir)
}

/// Atomically repoint `current` at `dir`.
///
/// Write-to-temp + rename: a reader either sees the whole old path or the
/// whole new one, never a truncated line. This is what makes a shortcut
/// unable to go stale — and what makes the updater's whole-stack swap
/// all-or-nothing, since the new stack is fully staged and verified before
/// the pointer moves.
pub fn set_current(dir: &Path) -> Result<(), StackError> {
    if !dir.is_dir() {
        return Err(StackError::Io(format!(
            "refusing to point `current` at a non-directory: {}",
            dir.display()
        )));
    }
    let pointer =
        pointer_path().ok_or_else(|| StackError::Io("no home directory for the pointer".into()))?;
    let parent = pointer
        .parent()
        .ok_or_else(|| StackError::Io("pointer path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(|e| StackError::Io(format!("creating {parent:?}: {e}")))?;

    let tmp = pointer.with_extension("current.tmp");
    fs::write(&tmp, dir.display().to_string())
        .map_err(|e| StackError::Io(format!("writing pointer temp: {e}")))?;
    fs::rename(&tmp, &pointer).map_err(|e| {
        let _ = fs::remove_file(&tmp); // wylde-check: discard-result-ok
        StackError::Io(format!("renaming pointer into place: {e}"))
    })?;
    Ok(())
}

/// Resolve the whole stack rooted at [`wylde_root`].
pub fn resolve() -> ResolvedStack {
    resolve_in(&wylde_root())
}

/// [`resolve`] rooted at an explicit `root`.
pub fn resolve_in(root: &Path) -> ResolvedStack {
    let entries = roster_in(root);

    if let Some(dir) = current_dir() {
        // Installed layout: one flat directory, everything from it. There is
        // no candidate walk here at all, so shadowing is structurally
        // impossible.
        let binaries = entries
            .iter()
            .map(|b| ResolvedBinary {
                name: b.name.clone(),
                tier: b.tier,
                path: existing(dir.join(&b.image)),
            })
            .collect();
        return ResolvedStack {
            source: Source::Current { dir },
            binaries,
        };
    }

    // Build-tree fallback. Choose ONE directory per workspace, once.
    let backend = choose_dir(
        root,
        BACKEND_PROFILE_DIRS,
        &image_of(&entries, sn::LIFECYCLE),
    );
    let gui = choose_dir(root, GUI_PROFILE_DIRS, &image_of(&entries, sn::GUI));

    let binaries = entries
        .iter()
        .map(|b| {
            let path = match (&b.tier, &b.folder) {
                // A bucket sibling lives beside its own manifest, not in a
                // build profile. Not profile drift — that IS its home.
                (_, Some(folder)) => sibling_path(folder, &b.image),
                (Tier::Gui, None) => gui.as_ref().and_then(|d| existing(d.join(&b.image))),
                (_, None) => backend.as_ref().and_then(|d| existing(d.join(&b.image))),
            };
            ResolvedBinary {
                name: b.name.clone(),
                tier: b.tier,
                path,
            }
        })
        .collect();

    ResolvedStack {
        source: Source::BuildTree { backend, gui },
        binaries,
    }
}

fn image_of(entries: &[StackBinary], name: &str) -> String {
    entries
        .iter()
        .find(|b| b.name == name)
        .map(|b| b.image.clone())
        .unwrap_or_else(|| format!("{name}{}", crate::EXE_SUFFIX))
}

/// Choose the single directory the stack runs from: the first candidate that
/// actually holds `anchor` (the daemon for the backend, the shell for the
/// GUI). Anchoring on a binary that must exist anyway means the choice is
/// identical to the one the old launcher made for that binary — so a dev rig
/// keeps launching the same daemon it always did — while every *other* binary
/// now follows it instead of running its own walk.
fn choose_dir(root: &Path, candidates: &[&[&str]], anchor: &str) -> Option<PathBuf> {
    let mut first_nonempty: Option<PathBuf> = None;
    for parts in candidates {
        let dir = parts.iter().fold(root.to_path_buf(), |acc, p| acc.join(p));
        if dir.join(anchor).is_file() {
            return Some(dir);
        }
        if first_nonempty.is_none() && dir.is_dir() {
            first_nonempty = Some(dir);
        }
    }
    // No candidate holds the anchor. Return the first directory that at
    // least exists so the caller can report *where* it looked; every lookup
    // in it will simply come back `None`.
    first_nonempty
}

/// Resolve a bucket sibling's binary. Mirrors
/// `wylde_lifecycle::state::services::sibling_binary_path`: beside the
/// manifest first, then the sibling's own cargo target.
fn sibling_path(folder: &Path, image: &str) -> Option<PathBuf> {
    let bare = image.strip_prefix("wylde-").unwrap_or(image);
    for dir in [
        folder.to_path_buf(),
        folder.join("target").join("release"),
        folder.join("target").join("debug"),
    ] {
        for name in [image, bare] {
            if let Some(p) = existing(dir.join(name)) {
                return Some(p);
            }
        }
    }
    None
}

fn existing(p: PathBuf) -> Option<PathBuf> {
    p.is_file().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn touch(path: PathBuf) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"binary").unwrap();
        path
    }

    fn exe(name: &str) -> String {
        format!("{name}{}", crate::EXE_SUFFIX)
    }

    /// Isolate the pointer + current-dir env for a test.
    struct Env;
    impl Env {
        fn isolated(home: &Path) -> Self {
            std::env::set_var(HOME_DIR_ENV, home);
            std::env::remove_var(CURRENT_DIR_ENV);
            Env
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var(HOME_DIR_ENV);
            std::env::remove_var(CURRENT_DIR_ENV);
        }
    }

    /// **The #92 property.** A stale artifact sitting at an earlier candidate
    /// must not shadow the fresh build the daemon was taken from.
    ///
    /// Layout: `rust/bin` holds ONLY a stale gateway (no daemon).
    /// `rust/target/release` holds the daemon and a fresh gateway. The old
    /// per-binary walk hit `rust/bin` first for the gateway and ran the stale
    /// one alongside a release daemon. Resolution now anchors on the daemon
    /// and takes the gateway from the same directory.
    #[test]
    #[serial]
    fn stale_artifact_never_shadows_the_directory_the_daemon_came_from() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        let stale = touch(root.path().join("rust/bin").join(exe("wylde-gateway")));
        touch(
            root.path()
                .join("rust/target/release")
                .join(exe("wylde-lifecycle")),
        );
        let fresh = touch(
            root.path()
                .join("rust/target/release")
                .join(exe("wylde-gateway")),
        );

        let stack = resolve_in(root.path());
        let gateway = stack.path_of(sn::GATEWAY).unwrap();

        assert_eq!(
            gateway, fresh,
            "the gateway must come from the same directory as the daemon"
        );
        assert_ne!(
            gateway, stale,
            "first-match-across-profiles regressed: the stale rust/bin \
             artifact shadowed the fresh release build"
        );
    }

    /// The generalisation of the above: whatever directory wins, the WHOLE
    /// in-tree backend comes from it. No mixed-profile stack is reachable.
    #[test]
    #[serial]
    fn every_in_tree_backend_binary_resolves_from_one_directory() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        // Scatter binaries across all three profiles, daemon in `debug`.
        touch(root.path().join("rust/bin").join(exe("wylde-gateway")));
        touch(
            root.path()
                .join("rust/target/release")
                .join(exe("wylde-harness")),
        );
        for image in ["wylde-lifecycle", "wylde-gateway", "wylde-harness"] {
            touch(root.path().join("rust/target/debug").join(exe(image)));
        }

        let stack = resolve_in(root.path());
        let dirs: std::collections::BTreeSet<_> = stack
            .binaries
            .iter()
            .filter(|b| b.tier == Tier::Service)
            .filter_map(|b| b.path.as_ref())
            .filter_map(|p| p.parent())
            .collect();

        assert_eq!(
            dirs.len(),
            1,
            "the in-tree backend resolved from more than one profile: {dirs:?}"
        );
        assert_eq!(
            dirs.into_iter().next().unwrap(),
            stack.daemon().unwrap().parent().unwrap(),
            "backend services must share the daemon's directory"
        );
    }

    /// **The maintainer's rig must keep working.** With no pointer, the daemon
    /// resolves to exactly the file the old launcher's candidate order would
    /// have picked.
    #[test]
    #[serial]
    fn with_no_pointer_the_build_tree_still_launches() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        let daemon = touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        let gui = touch(
            root.path()
                .join("Core/GUI/target/release")
                .join(exe("wylde-gui")),
        );

        let stack = resolve_in(root.path());
        assert!(matches!(stack.source, Source::BuildTree { .. }));
        assert_eq!(stack.daemon().unwrap(), daemon);
        assert_eq!(stack.gui().unwrap(), gui);
    }

    /// `rust/bin` wins over `release` for the daemon, as it always did — the
    /// candidate ORDER is unchanged, only its per-binary repetition is gone.
    #[test]
    #[serial]
    fn candidate_order_is_preserved_for_the_anchor() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        let bundled = touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        touch(
            root.path()
                .join("rust/target/release")
                .join(exe("wylde-lifecycle")),
        );
        assert_eq!(resolve_in(root.path()).daemon().unwrap(), bundled);
    }

    #[test]
    #[serial]
    fn a_present_pointer_wins_over_the_build_tree() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let installed = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        // A full build tree exists...
        touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        // ...but an installed stack is pointed at.
        let installed_daemon = touch(installed.path().join(exe("wylde-lifecycle")));
        set_current(installed.path()).unwrap();

        let stack = resolve_in(root.path());
        assert_eq!(
            stack.source,
            Source::Current {
                dir: installed.path().to_path_buf()
            }
        );
        assert_eq!(stack.daemon().unwrap(), installed_daemon);
    }

    #[test]
    #[serial]
    fn a_pointer_at_a_vanished_directory_falls_back_instead_of_bricking() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        let daemon = touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        // Point at a directory, then delete it.
        let gone = TempDir::new().unwrap();
        let gone_path = gone.path().to_path_buf();
        set_current(&gone_path).unwrap();
        drop(gone);

        let stack = resolve_in(root.path());
        assert!(
            matches!(stack.source, Source::BuildTree { .. }),
            "a dangling pointer must fall back to the build tree, not refuse \
             to launch"
        );
        assert_eq!(stack.daemon().unwrap(), daemon);
    }

    #[test]
    #[serial]
    fn set_current_is_atomic_and_leaves_no_temp_behind() {
        let home = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        set_current(target.path()).unwrap();
        let pointer = pointer_path().unwrap();
        assert!(pointer.is_file());
        assert_eq!(
            fs::read_to_string(&pointer).unwrap().trim(),
            target.path().display().to_string()
        );
        assert!(!pointer.with_extension("current.tmp").exists());

        // Repointing replaces wholesale.
        let second = TempDir::new().unwrap();
        set_current(second.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&pointer).unwrap().trim(),
            second.path().display().to_string()
        );
    }

    #[test]
    #[serial]
    fn set_current_refuses_a_non_directory() {
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());
        assert!(set_current(Path::new("does-not-exist-anywhere")).is_err());
    }

    #[test]
    #[serial]
    fn a_bucket_sibling_resolves_beside_its_manifest() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        let folder = root.path().join("Services").join("acme");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("manifest.json"), "{}").unwrap();
        let bin = touch(folder.join(exe("wylde-acme")));

        let stack = resolve_in(root.path());
        assert_eq!(stack.path_of("wylde-acme").unwrap(), bin);
    }

    #[test]
    #[serial]
    fn an_unbuilt_service_is_reported_missing_not_fatal() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let _env = Env::isolated(home.path());

        touch(root.path().join("rust/bin").join(exe("wylde-lifecycle")));
        let stack = resolve_in(root.path());

        assert!(stack.daemon().is_ok());
        assert!(stack.missing().contains(&sn::GATEWAY));
        assert!(stack.gui().is_err(), "a missing GUI is a reportable error");
    }
}
