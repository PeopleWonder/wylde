//! The two process-name sets the GUI's Quit path needs, derived from the
//! stack roster rather than hand-kept beside it.
//!
//! # Why this lives here and not in the GUI
//!
//! `Core/GUI/Shell/src/shutdown.rs` used to carry both sets as literal
//! arrays of four image names. There are eleven killable services. The
//! eight it never named (`voice`, `extension-bridge`, `ollama`,
//! `harness`, `treesitter`, `workspaces`, `n8n`, `vpn`) survived Quit
//! holding VRAM and named pipes (issue #124).
//!
//! The compounding half is why it was silent: the **drain wait polled the
//! same four names**. When those four exited the wait concluded "the
//! stack drained", returned success, and the hard-kill fallback — the
//! path that would have caught the other eight — was never reached. The
//! GUI reported a clean shutdown that wasn't one. Deriving only the kill
//! list would have left that early exit intact, so both sets derive here.
//!
//! # Why `wylde-stack` and not `wylde-lifecycle`
//!
//! `wylde-lifecycle::daemon_managed::DAEMON_MANAGED` owns the same
//! services' start/stop hooks, but it drags in tokio + anyhow, which must
//! not ripple into the shipped GUI binary (the objection that deferred
//! PR #109). This crate is dependency-lean by charter — names and paths
//! only — and is already in the GUI workspace's lock graph via
//! `wylde-updater`, so the GUI reaches it at no new dependency cost.
//!
//! It also puts the derivation in the `rust/` workspace, which is the
//! only workspace whose `cargo test` runs in CI. The counting gate in
//! `tests/shutdown_target_coverage.rs` is therefore an actual merge gate;
//! the same test sitting in `Core/GUI` would never have run (see #95).

use std::path::Path;

use crate::roster::{roster_in, Tier};
use crate::wylde_root;

/// GUI image names that are not roster rows and so cannot be derived.
///
/// `fletch-gui.exe` is the pre-cutover Tauri shell, retained only for the
/// overlap window. Nothing ships it from a roster row, so it is the one
/// name that stays listed by hand. It is a GUI binary, so it obeys the
/// GUI rules: last in the kill order, and never in the poll set.
///
/// `wylde-lifecycle.exe` used to need a carve-out here too. It no longer
/// does — the roster carries it as a [`Tier::Daemon`] row, so it derives
/// like everything else. That was a real hazard: as a hand-listed name it
/// could silently drop out of the kill list.
pub const NON_ROSTER_GUI_IMAGES: &[&str] = &["fletch-gui.exe"];

/// Image names for the hard-kill fallback (`taskkill /IM ...`).
///
/// GUI binaries are last so the services are signalled before the process
/// that issued the command goes down.
pub fn kill_targets() -> Vec<String> {
    kill_targets_in(&wylde_root())
}

/// [`kill_targets`] rooted at an explicit path — the tempdir-testable
/// seam the coverage gate drives a synthetic service through.
pub fn kill_targets_in(root: &Path) -> Vec<String> {
    let (gui, services): (Vec<_>, Vec<_>) = roster_in(root)
        .into_iter()
        .partition(|b| b.tier == Tier::Gui);

    // Partition rather than sort: the roster's own ordering is meaningful
    // and survives inside each half.
    let mut out: Vec<String> = services.into_iter().map(|b| b.image).collect();
    out.extend(gui.into_iter().map(|b| b.image));
    out.extend(NON_ROSTER_GUI_IMAGES.iter().map(|s| (*s).to_owned()));
    out
}

/// Image names polled during the drain wait.
///
/// The GUI binaries are excluded deliberately: the GUI is the process
/// doing the polling, so it always reads as alive and every Quit would
/// burn the full grace window before falling through to the hard kill.
pub fn poll_set() -> Vec<String> {
    poll_set_in(&wylde_root())
}

/// [`poll_set`] rooted at an explicit path — the tempdir-testable seam
/// the coverage gate drives a synthetic service through.
pub fn poll_set_in(root: &Path) -> Vec<String> {
    roster_in(root)
        .into_iter()
        .filter(|b| b.tier != Tier::Gui)
        .map(|b| b.image)
        .collect()
}
