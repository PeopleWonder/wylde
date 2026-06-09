//! `notify`-crate integration for the file watcher (Slice I).
//!
//! Two responsibilities, both pure-ish so they can be reasoned about apart
//! from the async loop in [`super`]:
//!   * [`translate`] — fold one cross-platform [`::notify::Event`] into zero or
//!     more `(PathBuf, ChangeKind)` raw changes. Metadata-only events (chmod,
//!     access) translate to nothing — they can't change a file's indexed
//!     content. This is unit-tested without touching the filesystem.
//!   * [`build_watcher`] — construct a [`RecommendedWatcher`] watching a folder
//!     recursively and forwarding translated changes into a tokio channel the
//!     watcher loop drains.
//!
//! The external crate is referenced as `::notify::` throughout because this
//! module is itself named `notify` (matching Build Order §2's file layout).

use std::path::{Path, PathBuf};

use ::notify::event::{EventKind, ModifyKind, RenameMode};
use ::notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

use super::debouncer::ChangeKind;

/// One raw, pre-debounce change: a path and whether it now exists or is gone.
pub type RawChange = (PathBuf, ChangeKind);

/// Fold a notify event into the raw changes it implies. Returns an empty vec
/// for events that can't affect indexed content (metadata/attribute changes,
/// access events, the catch-all `Other`).
///
/// Mapping:
///   * **Create** / **Modify(Data|Any)** → [`ChangeKind::Upsert`] for each path.
///   * **Remove** → [`ChangeKind::Remove`] for each path.
///   * **Modify(Name(..))** (rename) → the `From` side is a Remove, the `To`
///     side an Upsert. `Both` carries `[from, to]` in `paths`; the single-sided
///     `From`/`To` modes carry just their one side. The `Any`/`Other` rename
///     modes (some Linux backends) are resolved by existence: a path that's
///     still on disk is an Upsert, one that's gone is a Remove.
pub fn translate(event: &Event) -> Vec<RawChange> {
    match &event.kind {
        EventKind::Create(_) => upsert_all(&event.paths),
        EventKind::Remove(_) => remove_all(&event.paths),
        EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
            upsert_all(&event.paths)
        }
        EventKind::Modify(ModifyKind::Name(mode)) => match mode {
            RenameMode::From => remove_all(&event.paths),
            RenameMode::To => upsert_all(&event.paths),
            RenameMode::Both => {
                let mut out = Vec::with_capacity(2);
                if let Some(from) = event.paths.first() {
                    out.push((from.clone(), ChangeKind::Remove));
                }
                if let Some(to) = event.paths.get(1) {
                    out.push((to.clone(), ChangeKind::Upsert));
                }
                out
            }
            // Backend couldn't tell which side — fall back to existence.
            RenameMode::Any | RenameMode::Other => event
                .paths
                .iter()
                .map(|p| {
                    let kind = if p.exists() {
                        ChangeKind::Upsert
                    } else {
                        ChangeKind::Remove
                    };
                    (p.clone(), kind)
                })
                .collect(),
        },
        // Metadata/attribute changes, access events, and the catch-alls don't
        // change indexed content.
        _ => Vec::new(),
    }
}

fn upsert_all(paths: &[PathBuf]) -> Vec<RawChange> {
    paths.iter().map(|p| (p.clone(), ChangeKind::Upsert)).collect()
}

fn remove_all(paths: &[PathBuf]) -> Vec<RawChange> {
    paths.iter().map(|p| (p.clone(), ChangeKind::Remove)).collect()
}

/// Build a recursive watcher over `folder`, forwarding every translated raw
/// change into `tx`. The returned watcher must be kept alive — dropping it
/// stops the OS-level watch. The notify callback runs on notify's own thread;
/// an unbounded tokio sender is safe to use from there.
pub fn build_watcher(folder: &str, tx: UnboundedSender<RawChange>) -> ::notify::Result<RecommendedWatcher> {
    let mut watcher = ::notify::recommended_watcher(move |res: ::notify::Result<Event>| match res {
        Ok(event) => {
            for change in translate(&event) {
                // A closed receiver means the watch is being torn down; stop
                // forwarding (the watcher itself is about to be dropped).
                if tx.send(change).is_err() {
                    break;
                }
            }
        }
        Err(e) => tracing::debug!("workspaces.watcher: notify error: {e}"),
    })?;
    watcher.watch(Path::new(folder), RecursiveMode::Recursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RemoveKind};

    fn ev(kind: EventKind, paths: Vec<&str>) -> Event {
        Event {
            kind,
            paths: paths.into_iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    #[test]
    fn create_and_modify_are_upserts() {
        let c = translate(&ev(EventKind::Create(CreateKind::File), vec!["/a.rs"]));
        assert_eq!(c, vec![(PathBuf::from("/a.rs"), ChangeKind::Upsert)]);

        let m = translate(&ev(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            vec!["/a.rs"],
        ));
        assert_eq!(m, vec![(PathBuf::from("/a.rs"), ChangeKind::Upsert)]);

        let any = translate(&ev(EventKind::Modify(ModifyKind::Any), vec!["/a.rs"]));
        assert_eq!(any, vec![(PathBuf::from("/a.rs"), ChangeKind::Upsert)]);
    }

    #[test]
    fn remove_is_a_remove() {
        let r = translate(&ev(EventKind::Remove(RemoveKind::File), vec!["/gone.rs"]));
        assert_eq!(r, vec![(PathBuf::from("/gone.rs"), ChangeKind::Remove)]);
    }

    #[test]
    fn rename_both_is_remove_then_upsert() {
        let r = translate(&ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec!["/old.rs", "/new.rs"],
        ));
        assert_eq!(
            r,
            vec![
                (PathBuf::from("/old.rs"), ChangeKind::Remove),
                (PathBuf::from("/new.rs"), ChangeKind::Upsert),
            ]
        );
    }

    #[test]
    fn rename_single_sides_map_each_way() {
        let from = translate(&ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec!["/old.rs"],
        ));
        assert_eq!(from, vec![(PathBuf::from("/old.rs"), ChangeKind::Remove)]);

        let to = translate(&ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            vec!["/new.rs"],
        ));
        assert_eq!(to, vec![(PathBuf::from("/new.rs"), ChangeKind::Upsert)]);
    }

    #[test]
    fn metadata_and_access_translate_to_nothing() {
        let meta = translate(&ev(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
            vec!["/a.rs"],
        ));
        assert!(meta.is_empty(), "chmod must not trigger a re-index");

        let access = translate(&ev(EventKind::Access(AccessKind::Any), vec!["/a.rs"]));
        assert!(access.is_empty());

        let other = translate(&ev(EventKind::Other, vec!["/a.rs"]));
        assert!(other.is_empty());
    }
}
