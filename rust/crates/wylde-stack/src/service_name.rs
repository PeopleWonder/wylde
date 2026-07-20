//! Canonical service names.
//!
//! Moved here from `wylde_lifecycle::state::service_name` (which now
//! re-exports this module verbatim) so the lean crates — the updater and the
//! launcher — can name a service without pulling in the daemon. These strings
//! are simultaneously the pipe name, the manifest filename stem, the key into
//! the daemon's process map, and the `WYLDE_<NAME>_BIN` override stem, so
//! there must only ever be one copy of them.

/// Memgraph supervisor. JVM-supervised — no standalone Wylde image.
pub const MEMGRAPH: &str = "wylde-memgraph";
pub const VOICE: &str = "wylde-voice";
pub const VRAM_BROKER: &str = "wylde-vram-broker";
pub const DEVICE_GATE: &str = "wylde-device-gate";
pub const EXTENSION_BRIDGE: &str = "wylde-extension-bridge";
pub const GATEWAY: &str = "wylde-gateway";
pub const OLLAMA: &str = "wylde-ollama";
/// WyldeLink VPN — user-started, not spawned in the boot sweep.
pub const VPN: &str = "wylde-vpn";
/// In-process inside `wylde-harness` since slice R2b — owns no subprocess.
pub const MEMORY_SCHEDULER: &str = "wylde-memory-scheduler";
/// Chat-turn driver.
pub const HARNESS: &str = "wylde-harness";
/// Structural-parsing sidecar.
pub const TREESITTER: &str = "wylde-treesitter";
/// Workspace-scoped service — registry, persona, RAG indexer, code graph.
pub const WORKSPACES: &str = "wylde-workspaces";
/// Pipe surface over the external, user-managed n8n daemon.
pub const N8N: &str = "wylde-n8n";

/// The lifecycle daemon itself. Not a *managed* service (it is the thing
/// doing the managing), but very much part of the shipped stack — and the
/// single most important omission from the GUI-only updater, since it is the
/// process that spawns everything else.
pub const LIFECYCLE: &str = "wylde-lifecycle";

/// The gpui shell. Built out of the standalone `Core/GUI/` workspace, so its
/// binary lands under `Core/GUI/target/<profile>/`, not `rust/target/`.
pub const GUI: &str = "wylde-gui";
