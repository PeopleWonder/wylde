//! **The counting gate for issue #124, criterion 5.**
//!
//! Adding the Nth service must carry it onto BOTH shutdown paths — the
//! hard-kill roster and the drain-wait poll set — with no second list to
//! remember. This file is what makes that true rather than claimed.
//!
//! It is written as a *behavioural* test, following the thesis of
//! `wylde-updater/tests/whole_stack_coverage.rs`: comparing two lists
//! that are both derived from the same table is tautological and would
//! stay green through exactly the bug it claims to prevent. Here a
//! service is created on disk, discovered by the real filesystem walk,
//! and then required to appear on both paths. Break discovery and this
//! goes red.
//!
//! # What it replaces
//!
//! Nothing counted before. `wylde_check` rule 45 only asserted that the
//! string `lifecycle.shutdown_all` was present in the GUI's shutdown
//! source — it never scanned the two image-name lists at all, and
//! explicitly exempted them as "a curated infra subset". The GUI-side
//! unit tests asserted ordering (GUI last) and exclusion (GUI not
//! polled), but never that any *service* was present. All of that was
//! green while eight of eleven services were unreachable by either path.
//!
//! # Why this file is in `rust/` and not beside the GUI code it guards
//!
//! `Core/GUI`'s `cargo test` does not run in CI — the `gui` job is
//! build-only and `gui-panel-walk` is scoped to the panel crates. A gate
//! living next to `Core/GUI/Shell/src/shutdown.rs` could not turn CI red,
//! which is the one thing criterion 5 requires. `rust/`'s
//! `cargo test --workspace` is the gate CI actually enforces, so the
//! derivation lives in `wylde-stack` and the GUI is a thin caller — and
//! `gui_shutdown_delegates_to_the_derived_sets` below reaches across the
//! workspace boundary to hold the GUI to being exactly that.

use std::fs;
use std::path::{Path, PathBuf};

use wylde_stack::roster::{roster_in, Tier};
use wylde_stack::shutdown_targets::{kill_targets_in, poll_set_in, NON_ROSTER_GUI_IMAGES};

/// Drop a synthetic service on disk exactly the way a real one arrives:
/// a folder under `Services/<name>/` carrying a `manifest.json`.
fn drop_service(root: &Path, name: &str) {
    let folder = root.join("Services").join(name);
    fs::create_dir_all(&folder).expect("create synthetic service folder");
    fs::write(
        folder.join("manifest.json"),
        format!(r#"{{"name": "{name}", "enabled": true}}"#),
    )
    .expect("write synthetic manifest");
}

/// Repo root, from this crate's manifest dir (`rust/crates/wylde-stack`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repo root is three levels above rust/crates/wylde-stack")
        .to_path_buf()
}

/// **The count.** Every roster binary must be on the kill path, and every
/// non-GUI roster binary must be on the drain-wait path — with a
/// synthetic service dropped in to prove the derivation is live discovery
/// and not a list that happens to match today.
///
/// Against the pre-#124 hand-typed lists this is RED twice over: the
/// synthetic service is not one of the four typed names, and neither are
/// seven of the real ones.
#[test]
fn every_roster_binary_reaches_both_shutdown_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    drop_service(root, "quasar");

    let roster = roster_in(root);
    let kill = kill_targets_in(root);
    let poll = poll_set_in(root);

    // The seam is live — otherwise this test asserts nothing.
    let synthetic = format!("wylde-quasar{}", wylde_stack::EXE_SUFFIX);
    assert!(
        roster.iter().any(|b| b.image == synthetic),
        "a service dropped into Services/ must appear in the roster; \
         without that this gate is vacuous",
    );

    let missing_from_kill: Vec<&str> = roster
        .iter()
        .filter(|b| !kill.contains(&b.image))
        .map(|b| b.image.as_str())
        .collect();
    assert!(
        missing_from_kill.is_empty(),
        "{} of {} roster binaries are absent from the hard-kill list: \
         {missing_from_kill:?} — these processes survive Quit",
        missing_from_kill.len(),
        roster.len(),
    );

    // The half that made the failure silent: a poll set narrower than the
    // kill set concludes "drained" as soon as the names it knows exit,
    // returns success, and the hard kill never runs.
    let missing_from_poll: Vec<&str> = roster
        .iter()
        .filter(|b| b.tier != Tier::Gui && !poll.contains(&b.image))
        .map(|b| b.image.as_str())
        .collect();
    assert!(
        missing_from_poll.is_empty(),
        "{} roster services are absent from the drain-wait poll set: \
         {missing_from_poll:?} — the drain wait would report a clean \
         shutdown while these are still alive",
        missing_from_poll.len(),
    );
}

/// The counts must also move together. If a service is ever added to one
/// path but not the other, the arithmetic breaks even if both lists are
/// individually non-empty.
#[test]
fn the_two_paths_differ_only_by_the_gui_binaries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    drop_service(root, "quasar");

    let roster = roster_in(root);
    let gui_count = roster.iter().filter(|b| b.tier == Tier::Gui).count();
    assert!(
        gui_count > 0,
        "the roster must carry at least one GUI tier row"
    );

    assert_eq!(
        kill_targets_in(root).len(),
        poll_set_in(root).len() + gui_count + NON_ROSTER_GUI_IMAGES.len(),
        "kill list and poll set must differ by exactly the GUI binaries",
    );
}

/// The lifecycle daemon is not a `Tier::Service` row, but it is a process
/// the shutdown has to reach. It used to be retained by being typed in by
/// hand — the failure mode being that it could silently drop out. Pin
/// that it now rides the roster's `Tier::Daemon` row instead.
#[test]
fn the_lifecycle_daemon_is_retained_on_both_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let image = format!("wylde-lifecycle{}", wylde_stack::EXE_SUFFIX);

    assert!(
        kill_targets_in(root).contains(&image),
        "{image} must be in the kill targets",
    );
    assert!(
        poll_set_in(root).contains(&image),
        "{image} must be in the drain-wait poll set",
    );
}

/// GUI binaries stay out of the poll set. They are the process doing the
/// polling, so including one makes every Quit hang for the full grace
/// window before falling through.
#[test]
fn gui_binaries_are_never_polled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let poll = poll_set_in(root);

    for gui in roster_in(root).iter().filter(|b| b.tier == Tier::Gui) {
        assert!(
            !poll.contains(&gui.image),
            "{} must not be polled during the drain wait",
            gui.image,
        );
    }
    for extra in NON_ROSTER_GUI_IMAGES {
        assert!(
            !poll.iter().any(|n| n == extra),
            "{extra} is a GUI binary and must not be polled",
        );
    }
}

/// GUI binaries stay last in the kill order, so services are signalled
/// before the process issuing the command goes down.
#[test]
fn gui_binaries_sort_last_in_the_kill_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let kill = kill_targets_in(root);

    let gui_images: Vec<String> = roster_in(root)
        .iter()
        .filter(|b| b.tier == Tier::Gui)
        .map(|b| b.image.clone())
        .chain(NON_ROSTER_GUI_IMAGES.iter().map(|s| (*s).to_owned()))
        .collect();

    let first_gui = kill
        .iter()
        .position(|n| gui_images.contains(n))
        .expect("the kill order must contain the GUI binaries");
    for later in &kill[first_gui..] {
        assert!(
            gui_images.contains(later),
            "{later} is a service and must not come after a GUI binary",
        );
    }
}

/// **The cross-workspace half.** `Core/GUI` builds in CI but never runs
/// its tests, so nothing on that side can enforce anything. This reaches
/// across the workspace boundary and holds the GUI to *delegating*:
/// it must call the derived sets, and must not reintroduce a hand-typed
/// image list beside them.
///
/// Deliberately fails on a missing file rather than skipping over it.
/// Rules 44/45 were guarded by `if <file>.exists()`, so when the
/// full-Rust cutover deleted their targets they skipped their bodies and
/// passed green — a dead gate (issue #101). This does not repeat that.
#[test]
fn gui_shutdown_delegates_to_the_derived_sets() {
    let gui_shutdown = repo_root().join("Core/GUI/Shell/src/shutdown.rs");
    let src = fs::read_to_string(&gui_shutdown).unwrap_or_else(|e| {
        panic!(
            "{} must exist and be readable — if it moved, repoint this gate \
             rather than deleting it: {e}",
            gui_shutdown.display(),
        )
    });

    for call in ["kill_targets", "poll_set"] {
        assert!(
            src.contains(call),
            "Core/GUI/Shell/src/shutdown.rs must call \
             wylde_stack::shutdown_targets::{call}() — the shutdown sets \
             are derived from the roster, never hand-kept",
        );
    }

    // COUNT the hand-typed Wylde image literals still in that file. The
    // derived sets carry no `.exe` literals at all; the only names
    // allowed to appear are the documented non-roster exceptions and the
    // GUI's own binaries, which the tests there reference by name.
    let allowed: Vec<&str> = NON_ROSTER_GUI_IMAGES
        .iter()
        .copied()
        .chain(["wylde-gui.exe", "wylde-lifecycle.exe"])
        .collect();

    let stray: Vec<&str> = src
        .match_indices("\"wylde-")
        .filter_map(|(i, _)| {
            let rest = &src[i + 1..];
            let end = rest.find('"')?;
            let name = &rest[..end];
            name.ends_with(".exe").then_some(name)
        })
        .filter(|name| !allowed.contains(name))
        .collect();

    assert!(
        stray.is_empty(),
        "Core/GUI/Shell/src/shutdown.rs carries {} hand-typed service image \
         name(s): {stray:?}. That is the #124 bug returning — these lists \
         must derive from the roster so a new service is covered by \
         construction. Allowed non-roster names: {allowed:?}",
        stray.len(),
    );
}
