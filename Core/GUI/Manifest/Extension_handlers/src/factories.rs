//! Factory wiring — maps the path-like `factory:` strings the
//! aggregator emits to real Rust calls into the first-party panel
//! crates.
//!
//! This is the *hand-maintained* half of the build-time aggregator
//! design.  The generated source in `generated.rs` declares panels by
//! their factory string; the table here resolves the string to an
//! actual `ViewFactory` closure.  Adding a new first-party panel
//! means:
//!
//!   1. Drop a `manifest.json` under `Frontend/Panels/<Name>/`.
//!   2. Add a dep on the panel crate in this crate's `Cargo.toml`.
//!   3. Register its factory string here.
//!   4. Re-run the aggregator to refresh `generated.rs`, then `cargo fmt`.
//!
//! Step 4 is no longer trust-me: the `gui` CI job runs
//! `wylde-panel-aggregator --check`, which regenerates in memory and fails the
//! build if the committed `generated.rs` is out of date — so a forgotten regen
//! is a red build, not a panel that silently never appears (#125).
//!
//! Why not generate the factory wiring too?  The aggregator binary
//! reads JSON; it can't introspect Rust crate exports.  Keeping the
//! wiring hand-maintained (a) lets the compiler catch a typo in a
//! factory name immediately, and (b) keeps the generated file pure
//! data so its snapshot test is byte-stable.

use std::collections::HashMap;

use crate::registry::ViewFactory;

/// A typed lookup table from factory strings to `ViewFactory`
/// closures.  Construct one via `default_first_party()` and look up
/// against it from the generated source.
pub struct FactoryMap {
    inner: HashMap<&'static str, ViewFactory>,
}

impl FactoryMap {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn register(&mut self, key: &'static str, factory: ViewFactory) {
        self.inner.insert(key, factory);
    }

    pub fn take(&mut self, key: &str) -> Option<ViewFactory> {
        self.inner.remove(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.inner.keys().copied()
    }
}

impl Default for FactoryMap {
    fn default() -> Self {
        default_first_party()
    }
}

/// The first-party factory map shipped with this binary.  Every
/// `gpui_view` factory string the aggregator might emit for a
/// first-party panel resolves through here.
///
/// Each entry corresponds to a panel crate this registry depends on
/// (see `Cargo.toml [dependencies]`).
pub fn default_first_party() -> FactoryMap {
    let mut m = FactoryMap::new();

    // Settings panel — Phase §9 step 6.
    m.register(
        "wylde_panel_settings::SettingsPanel::view",
        Box::new(wylde_panel_settings::SettingsPanel::view),
    );

    // Workspaces panel — slice 3, plan §9 step 7.
    m.register(
        "wylde_panel_workspaces::WorkspacesPanel::view",
        Box::new(wylde_panel_workspaces::WorkspacesPanel::view),
    );

    // Tools panel — slice 4 (extension manager).
    m.register(
        "wylde_panel_tools::ToolsPanel::view",
        Box::new(wylde_panel_tools::ToolsPanel::view),
    );

    // Memory panel — slice 5 (three-layer memory browser).
    m.register(
        "wylde_panel_memory::MemoryPanel::view",
        Box::new(wylde_panel_memory::MemoryPanel::view),
    );

    // Chat panel — slice 5 (streaming message log + InferenceBar).
    m.register(
        "wylde_panel_chat::ChatPanel::view",
        Box::new(wylde_panel_chat::ChatPanel::view),
    );

    // Models panel — slice 6 (Ollama install/pull/delete + recs).
    m.register(
        "wylde_panel_models::ModelsPanel::view",
        Box::new(wylde_panel_models::ModelsPanel::view),
    );

    // Dashboard panel — slice 6 (service health + hardware + active
    // model + recent activity, auto-refreshing).
    m.register(
        "wylde_panel_dashboard::DashboardPanel::view",
        Box::new(wylde_panel_dashboard::DashboardPanel::view),
    );

    // Devices panel — slice 7 (`device_gate` paired-device list,
    // pairing flow with QR + countdown, three-tier permissions,
    // rotate-token + revoke).
    m.register(
        "wylde_panel_devices::DevicesPanel::view",
        Box::new(wylde_panel_devices::DevicesPanel::view),
    );

    // Remote Access panel — slice 7 (`wylde-vpn` status + peer list +
    // DDNS + port-forwarding hint + DNS rewrites).
    m.register(
        "wylde_panel_remote_access::RemoteAccessPanel::view",
        Box::new(wylde_panel_remote_access::RemoteAccessPanel::view),
    );

    // (The Images panel was extracted to the standalone `wylde-images`
    // Service — it now surfaces as a loopback iframe via the
    // Extensions/wylde-images stub, so there is no compiled-in factory.)

    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_contains_settings_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_settings::SettingsPanel::view"));
    }

    #[test]
    fn default_map_contains_workspaces_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_workspaces::WorkspacesPanel::view"));
    }

    #[test]
    fn default_map_contains_tools_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_tools::ToolsPanel::view"));
    }

    #[test]
    fn default_map_contains_memory_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_memory::MemoryPanel::view"));
    }

    #[test]
    fn default_map_contains_chat_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_chat::ChatPanel::view"));
    }

    #[test]
    fn default_map_contains_models_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_models::ModelsPanel::view"));
    }

    #[test]
    fn default_map_contains_dashboard_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_dashboard::DashboardPanel::view"));
    }

    #[test]
    fn default_map_contains_devices_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_devices::DevicesPanel::view"));
    }

    #[test]
    fn default_map_contains_remote_access_panel() {
        let m = default_first_party();
        assert!(m.contains("wylde_panel_remote_access::RemoteAccessPanel::view"));
    }

    #[test]
    fn take_consumes_the_entry() {
        let mut m = default_first_party();
        assert!(m
            .take("wylde_panel_settings::SettingsPanel::view")
            .is_some());
        assert!(m
            .take("wylde_panel_settings::SettingsPanel::view")
            .is_none());
    }
}
