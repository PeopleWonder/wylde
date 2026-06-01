//! Dashboard panel — gpui-era at-a-glance state.
//!
//! Reads from every Wylde service the user expects to "just know"
//! about: lifecycle health for each daemon, the broker's hardware
//! inventory, Ollama's loaded-models list, and the harness's recent
//! curated memories.  Auto-refreshes every 5 s with cards that
//! degrade individually when a service is down — the dashboard never
//! turns into a wall of error banners.
//!
//! Cross-panel navigation uses `wylde_gui_pipe::request_nav(key)`:
//!   * Service-health row click → `core/tools`
//!   * Empty active-model card → `core/models`
//!   * Empty / clicked recent-activity rows → `core/chat` or `core/memory`
//!
//! Living in the lowest order band of the Core panels (Chat is 5,
//! Dashboard is 8) so the user lands on Chat by default but can flick
//! to the Dashboard with one click for the "is everything OK?" check.

pub mod dashboard_panel;
pub mod ipc;

pub use dashboard_panel::DashboardPanel;
