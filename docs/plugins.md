# Core Plugins

Status: shipped (taxonomy reorg TX S4, 2026-06-11).

## The taxonomy — three distinct things, all-Rust

Wylde has three add-on tiers. They are not interchangeable, and each
has a defining test:

| Tier | What it is | Defining test | Lives at | Mechanism |
|---|---|---|---|---|
| **Extensions** | Plugins that **leave the ecosystem** | If it leaves the system, it's an Extension | `Extensions/` | Bridge/Gateway path — MCP processes supervised by `wylde-extension-bridge`, egress via the Gateway |
| **Plugins** | Add-ons **for the Core**, added directly into the Core filesystem and **compiled into the core** | If it's compiled into the core binary, it's a Plugin | `Core/Plugins/<name>/` | Rust-native, in-process — the `wylde-plugin-api` trait + the harness host. NOT the bridge, NOT the gateway, NOT a service crate |
| **Services** | Sibling full-tier suites | The core must keep working **without** it | `rust/crates/wylde-<name>` + its data home | Own pipe service, reached over IPC; graceful degradation when absent |

This page covers the middle tier only.

## What a plugin is

A Core plugin is a Rust lib crate at `Core/Plugins/<plugin_name>/`
(package `wylde-plugin-<name>`) implementing the `CorePlugin` trait
from `rust/crates/wylde-plugin-api`. The harness host
(`rust/crates/wylde-harness/src/plugins/mod.rs`) links every installed
plugin at compile time and registers its tools into the core tool
catalog at registry construction:

- catalog identity: canonical id `plugin_<plugin>_<tool>`, dotted name
  `plugin.<plugin>.<tool>`, group `plugins` — same conventions as the
  built-in groups, so the alias map, salvage parser, and `tools.list`
  all work unchanged;
- the `destructive` flag is forwarded verbatim, so the existing device
  tier gate and consent gate apply to plugin tools with no extra code;
- group `plugins` is advertised to the model even in verb mode
  (`turn/prompt.rs::advertise`) — plugin tools have no resource
  equivalent, so unlike the retired named tools there is no verb path
  to reach them.

## The trait surface

```rust
/// One tool a plugin contributes to the core tool catalog.
pub struct PluginTool {
    pub name: &'static str,        // plugin-local, snake_case; the host namespaces it
    pub description: &'static str, // the LLM-facing contract
    pub parameters: serde_json::Value, // catalog parameter-array shape; build with param()/param_default()
    pub destructive: bool,         // routes through the existing consent gate
}

/// A Core plugin: an in-process, Rust-native add-on compiled into the core.
pub trait CorePlugin: Send + Sync {
    fn name(&self) -> &'static str;     // plugin id, snake_case (e.g. "hello_wylde")
    fn version(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn tools(&self) -> Vec<PluginTool>;
    fn call(&self, tool: &str, args: &serde_json::Value) -> serde_json::Value;
}
```

`call` returns the standard tool result envelope —
`{"status": "success", ...}` / `{"status": "error", "error": "..."}` —
and must return an error envelope (not panic) for an unknown tool.

## Installing a plugin — the four steps

Discovery is **compile-time, deliberately**: in-process code is trusted
code, so the explicit linkage table plus the filesystem IS the
discovery story. No manifest scan, no registry file.

1. **Folder** — `Core/Plugins/<plugin_name>/`, a lib crate named
   `wylde-plugin-<name>` depending on `wylde-plugin-api` (+
   `serde_json`). Add `workspace = "../../../rust"` under `[package]`
   (the crate is outside the workspace root directory, so cargo's
   upward workspace discovery needs the pointer).
2. **Workspace member** — `"../Core/Plugins/<plugin_name>"` in
   `rust/Cargo.toml` `members`.
3. **Host dependency** — one line in
   `rust/crates/wylde-harness/Cargo.toml`:
   `wylde-plugin-<name> = { path = "../../../Core/Plugins/<plugin_name>" }`.
4. **Linkage table** — one `Box::new(...)` line in `installed()` in
   `rust/crates/wylde-harness/src/plugins/mod.rs`.

## Hello-world walkthrough

`Core/Plugins/hello_wylde/` is the shipped reference plugin (and the
authoring template — its `README.md` repeats the steps above). It
contributes two tools to prove multi-tool registration:

- `plugin.hello_wylde.greet` — `{name?: string}` →
  `"Hello, <name>! — from a Wylde core plugin"`;
- `plugin.hello_wylde.about` — no args → plugin name/version/description.

Try it end-to-end: `cargo test -p wylde-harness --lib plugins` runs the
host-side tests, including a real `dispatch_tool` round trip through
the registry, tier gate, and consent bypass to the plugin's `call`.

Authoring checklist distilled from it:

- implement `CorePlugin` on a `Send + Sync` type;
- declare parameters with `wylde_plugin_api::param` /
  `param_default` so schemas stay byte-compatible with built-in tools;
- match on the plugin-local tool name in `call`; return an error
  envelope on the wildcard arm;
- ship inline `#[cfg(test)]` tests in the plugin crate itself.

## What plugins are NOT for

- **Leaving the ecosystem.** Network calls, third-party APIs, anything
  that exits the machine → that's an **Extension** (bridge + Gateway
  egress, consent, audit). A plugin has no `Config`, no IPC client, and
  no business making sockets.
- **A full-tier optional suite.** If it has its own data home,
  lifecycle, and the core should degrade gracefully without it → that's
  a **Service** (own crate under `rust/crates/`, own pipe).
- **Untrusted code.** Plugins run in the harness's address space.
  Enabling one = trusting its source. There is no sandbox at this tier
  by design.

## Deliberate v1 limits

- **Sync `call`.** v1 plugins are compute/local-state add-ons; the host
  wraps the call in its async handler. An async trait variant is a v2
  decision, to be made when a plugin actually needs to await something.
- **Compile-time linkage.** Installing a plugin means recompiling the
  core. That is the point: the plugin tier is for code that belongs *in*
  the core binary but not in the core tree's mainline modules.
- **Trusted, in-process.** A panic in a plugin is a panic in the
  harness. Review plugin code like core code.
- **Dynamic loading rejected.** Rust has no stable ABI: a `cdylib`
  loaded via `libloading` degrades the surface to `extern "C"` (losing
  traits, generics, and ownership across the boundary) and is latent UB
  the moment plugin and host disagree on rustc version or struct
  layout. The architecture review (outputs/wylde-architecture-review.md
  §3.1) evaluated trait-static-link / C-ABI-dynamic / WASM / subprocess
  and rejected C-ABI "firmly" — it looks the most native and is the
  worst engineering. Add-ons that need isolation or third-party
  distribution belong in the Extension tier (process boundary today,
  WASM as the v2 enforcement track).

## Lint note

wylde_check rule 26 (`import_paths_rust`) forbids cross-crate `use`
outside `wylde_shared`. It carries two TX S4 exemptions:
`wylde_plugin_api` is importable everywhere (SDK/shared surface), and
`wylde_plugin_*` crates are importable from `wylde-harness` only (the
plugin host). The plugin crates themselves live outside the rule's
`rust/crates/*/src` walk.
