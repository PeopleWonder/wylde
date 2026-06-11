# hello_wylde — the reference Core plugin

This folder is both a working plugin and the template you copy when
authoring a new one.

**Taxonomy one-liner:** Extensions leave the ecosystem (bridge/Gateway);
**Plugins are compiled into the core** (this mechanism); Services are
sibling suites the core must work without. If your add-on talks to the
outside world, you want an Extension; if it's a full optional tier, you
want a Service. Full story: `docs/plugins.md`.

## The four install steps

Discovery is compile-time and deliberate (in-process code is trusted
code; Rust has no stable ABI, so there is no dynamic loading). The
linkage table plus this filesystem IS the registry:

1. **Folder** — create `Core/Plugins/<plugin_name>/` as a lib crate,
   package name `wylde-plugin-<name>`, depending on `wylde-plugin-api`
   (+ `serde_json`). Because the crate lives outside `rust/`, its
   `Cargo.toml` needs `workspace = "../../../rust"` under `[package]`
   for `*.workspace = true` inheritance to resolve.
2. **Workspace member** — add `"../Core/Plugins/<plugin_name>"` to the
   `members` list in `rust/Cargo.toml`.
3. **Host dependency** — add one line to
   `rust/crates/wylde-harness/Cargo.toml`:
   `wylde-plugin-<name> = { path = "../../../Core/Plugins/<plugin_name>" }`.
4. **Linkage table** — add one `Box::new(your_crate::YourPlugin)` line
   to `installed()` in `rust/crates/wylde-harness/src/plugins/mod.rs`.

Uninstall = the same four steps in reverse.

## The trait

```rust
pub struct PluginTool {
    pub name: &'static str,        // plugin-local, snake_case; host namespaces it
    pub description: &'static str, // shown to the LLM verbatim
    pub parameters: serde_json::Value, // [{name, type, required, description}, ...]
    pub destructive: bool,         // routes through the consent gate + tier gate
}

pub trait CorePlugin: Send + Sync {
    fn name(&self) -> &'static str;        // "hello_wylde" — matches this folder
    fn version(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn tools(&self) -> Vec<PluginTool>;
    fn call(&self, tool: &str, args: &serde_json::Value) -> serde_json::Value;
}
```

`call` is **sync by design** (v1 — plugins are compute/local-state
add-ons; the host wraps it async-side). Return the standard envelope:
`{"status": "success", ...}` or `{"status": "error", "error": "..."}` —
including for an unknown `tool` name. Never panic.

## What you get

Each tool lands in the core catalog as
`plugin.<plugin_name>.<tool_name>` (canonical id
`plugin_<plugin_name>_<tool_name>`), group `plugins`, advertised to the
model even in verb mode. This plugin's tools:

- `plugin.hello_wylde.greet` — `{name?: string}` → a greeting.
- `plugin.hello_wylde.about` — no args → name/version/description.

## Verify

```text
cd rust
cargo test -p wylde-plugin-hello-wylde --lib   # the plugin's own tests
cargo test -p wylde-harness --lib plugins      # catalog + dispatch wiring
```
