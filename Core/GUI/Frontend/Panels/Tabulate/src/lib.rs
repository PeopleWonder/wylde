//! Wylde Tabulate panel — native gpui cockpit over the `wylde-tabulate` Service.
//!
//! Minimum usable surface over the `tabulate.*` verbs (through the
//! `wylde_gui_pipe::call` seam):
//!
//!   * **Input** — a file path + an output-format toggle (`.xlsx` / `.csv`).
//!   * **Probe** — `tabulate.probe`, a PHI-safe structure probe. Renders the
//!     detected file type, table shape, and per-column header + inferred type.
//!     It NEVER shows a cell value (the service guarantees that), and the
//!     mandatory redaction-review gate is shown alongside.
//!   * **Extract** — `tabulate.extract`, which writes the spreadsheet and
//!     reports the absolute output path + a success/error line.
//!   * **Safety chip** — read once from `tabulate.capabilities`: local-only
//!     (no network egress), encrypted at rest, audit logging on.
//!
//! Greys out via the Shell's ServiceUnavailable stub when `wylde-tabulate` is
//! absent (declared in `manifest.json` `required_services`).

pub mod ipc;
pub mod tabulate_panel;

pub use tabulate_panel::TabulatePanel;
