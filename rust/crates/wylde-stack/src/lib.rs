//! What the Wylde stack **is**, and where the **current** one lives.
//!
//! Two questions used to be answered by parallel hand-kept lists in
//! unrelated places, and both drifted:
//!
//! * *"Which binaries make up the stack?"* — the updater answered
//!   `wylde-gui` and nothing else (`pick_asset`'s `starts_with("wylde-gui")`
//!   literal), so the lifecycle daemon and every backend service were
//!   invisible to it. Most of Wylde's logic is backend, so a backend fix
//!   could not reach an installed user at all (issue #97).
//! * *"Where do I run them from?"* — `launch_wylde.ps1` answered with a
//!   **per-binary first-match** across build profiles (`rust/bin` →
//!   `target/release` → `target/debug`), so one stale artifact at an earlier
//!   candidate silently shadowed a fresh build, and different binaries in the
//!   same launch could come from different profiles (issue #92).
//!
//! This crate is the one answer to both. [`roster`] derives the binary set by
//! **discovery** — the in-tree core tier plus whatever the `Services/` bucket
//! currently holds — and [`current::resolve`] resolves that roster against a
//! single directory, never a per-binary candidate walk.
//!
//! ## Why the crate is dependency-lean
//!
//! `wylde-updater` depends on this, and the GUI workspace depends on
//! `wylde-updater`. The service start/stop hooks (and their tokio/anyhow
//! graph) therefore **cannot** live here — they stay in `wylde-lifecycle`,
//! which references [`service_name`] and [`CORE_STACK`] from here so the
//! names themselves exist exactly once. The
//! `wylde_lifecycle::daemon_managed` gate asserts the two tables agree, so
//! adding a row there without one here turns red rather than silently
//! shipping a service the updater can't carry.
//!
//! ## The autonomy property
//!
//! Adding service N+1 requires **zero edits to the updater and zero to the
//! launcher**. An out-of-tree service under `Services/<name>/` is picked up
//! by [`roster`] the moment it is dropped in, and flows to both consumers
//! from there.

pub mod current;
pub mod roster;
pub mod service_name;

pub use current::{resolve, ResolvedBinary, ResolvedStack, Source};
pub use roster::{roster, roster_in, CoreEntry, StackBinary, Tier, CORE_STACK};

use std::path::PathBuf;

/// Errors surfaced by the resolver. Kept coarse and `Display`-friendly: the
/// launcher prints these verbatim, and a launch failure has to be readable
/// without a debugger.
#[derive(Debug, thiserror::Error)]
pub enum StackError {
    #[error("io error: {0}")]
    Io(String),
    #[error("no runnable Wylde stack found: {0}")]
    NotFound(String),
}

/// The estate root — `WYLDE_ROOT`, or the process working directory.
///
/// Mirrors the identical helper in `wylde-lifecycle` (`registry.rs`,
/// `paths.rs`, `state/services.rs`). Duplicated rather than shared because
/// this crate must not depend on the daemon; the semantics are pinned by
/// [`crate::roster::tests`].
pub fn wylde_root() -> PathBuf {
    std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Executable suffix for the host. Windows in production; the tests exercise
/// the resolver on any platform.
pub const EXE_SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };

/// The release-asset target triple Wylde publishes for. Asset names follow
/// `<image-stem>-<target>.exe` (+ `.minisig`), matching the convention
/// already in `docs/self-updater-design.md`.
pub const RELEASE_TARGET: &str = "x86_64-pc-windows-msvc";
