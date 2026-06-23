//! Wylde Organize panel — native gpui cockpit over the `wylde-organize` Service.
//!
//! Drives the six `organize.*` verbs through the `wylde_gui_pipe::call` seam:
//!
//!   * **Scope picker** — pick a tier (User data / Whole profile / Whole drive),
//!     toggle the opt-in for the broader tiers, optionally name explicit roots,
//!     and (drive only) type the confirmation phrase. The gates mirror the
//!     service's authoritative checks — the service still enforces them, so a
//!     mis-built request is refused server-side too.
//!   * **Scan** — `organize.propose`, a read-only dry run, renders the plan.
//!   * **Plan review** — proposed moves + removal candidates as rows with a
//!     per-row Keep/Skip toggle; protected paths are reported as skipped. The
//!     curated set (accepted rows only) is what Apply sends.
//!   * **Apply** — `organize.apply`; removals go to the recycle bin, every
//!     mutation is journaled.
//!   * **Undo** — `organize.undo`, reversing the last applied plan.
//!
//! Greys out via the Shell's ServiceUnavailable stub when `wylde-organize` is
//! absent (declared in `manifest.json` `required_services`).

pub mod ipc;
pub mod organize_panel;

pub use organize_panel::OrganizePanel;
