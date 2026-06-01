//! Wylde panel registry — manifest schema (v2), build-time aggregator,
//! runtime registry, and the `gui.list_tabs` action handler.
//!
//! The crate split mirrors `docs/wylde-gpui-rewrite-plan.md` §5:
//!
//! * `manifest`  — on-disk JSON parser + the `PanelManifest` /
//!   `PanelEntry` / `PanelSource` types.  Schema is v2; anything
//!   tagged `schema_version: 1` (the Phase-12.7 custom-element world)
//!   is rejected at parse time per §12 question 10 (hard bump).
//! * `registry`  — `PanelRegistry` struct and the static `OnceCell`
//!   the generated source populates.  Owns no state beyond the panel
//!   entry list.
//! * `generated` — emitted by `wylde-panel-aggregator`.  Wires each
//!   discovered first-party panel's `factory:` string to a real Rust
//!   call into the panel crate.  Kept small enough that a future
//!   gpui-rev breakage is a one-place fix.
//! * `overlay`   — runtime union of static (this crate) + extension
//!   panels (`extensions.list_panels` from the extension bridge).
//!   Pure function; the network side lives in the Shell.
//! * `list_tabs` — handler for the `gui.list_tabs` action.
//!
//! The build-time aggregator is `src/bin/wylde_panel_aggregator.rs`.

pub mod factories;
pub mod generated;
pub mod list_tabs;
pub mod loopback;
pub mod manifest;
pub mod overlay;
pub mod registry;

pub use list_tabs::list_tabs;
pub use manifest::{
    parse_panel_manifest, ExtensionPanel, PanelEntry, PanelManifest, PanelOrigin, PanelSource,
    SCHEMA_VERSION,
};
pub use overlay::union_for_runtime;
pub use registry::{PanelRegistry, RegistryError};
