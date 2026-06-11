//! `wylde-plugin-api` — the SDK surface for **Core plugins**.
//!
//! ## The taxonomy (don't mix these up)
//!
//! Wylde has three distinct add-on tiers, and this crate serves exactly
//! one of them:
//!
//! * **Extensions** — plugins that *leave the ecosystem*. They live in
//!   `Extensions/`, run as bridge-supervised MCP processes, and reach
//!   the outside world through the Gateway (`wylde-extension-bridge`).
//!   The defining test: if it leaves the system, it's an Extension.
//! * **Plugins** (this crate) — add-ons **for the Core**, added
//!   directly into the Core filesystem (`Core/Plugins/<name>/`) and
//!   **compiled into the core**. Rust-native, in-process, no bridge,
//!   no gateway, no service crate. The defining test: if it's compiled
//!   into the core binary, it's a Plugin.
//! * **Services** — sibling full-tier suites (`wylde-workspaces`,
//!   `wylde-n8n`, …) the core must keep working *without*. The defining
//!   test: if the core degrades gracefully when it's absent, it's a
//!   Service.
//!
//! ## What a plugin is
//!
//! A Core plugin is a Rust lib crate under `Core/Plugins/<plugin_name>/`
//! (package name `wylde-plugin-<name>`) that implements [`CorePlugin`].
//! The harness host (`wylde-harness/src/plugins/`) links each installed
//! plugin at compile time and registers its [`PluginTool`]s into the
//! core tool catalog under group `"plugins"`, namespaced as
//! `plugin.<plugin_name>.<tool_name>`. Discovery is deliberately
//! compile-time: in-process code is trusted code, so the linkage table
//! plus the filesystem IS the registry — no manifest scan, no dynamic
//! loading (Rust has no stable ABI; a `dlopen` plugin built by a
//! different rustc is latent UB inside the host's address space).
//!
//! ## v1 limits (deliberate)
//!
//! * [`CorePlugin::call`] is **sync**. v1 plugins are compute/local-state
//!   add-ons; the host wraps the call in its async handler. An async
//!   variant of the trait is a v2 decision — don't fake it with a
//!   runtime handle in the plugin.
//! * No `Config`/IPC access. A plugin that needs to talk to peers over
//!   the pipe is either core code or a Service — not a plugin.

use serde_json::Value;

/// One tool a plugin contributes to the core tool catalog.
///
/// `name` is the plugin-local tool id (snake_case, e.g. `greet`); the
/// host namespaces it into the final catalog identity (canonical id
/// `plugin_<plugin>_<tool>`, dotted name `plugin.<plugin>.<tool>`), so
/// two plugins may both ship a `status` tool without colliding.
pub struct PluginTool {
    /// Plugin-local tool name, snake_case. Final catalog id is
    /// namespaced by the host — never embed the plugin name here.
    pub name: &'static str,
    /// Passed verbatim to the LLM as the tool's contract. Describe what
    /// the tool does, its arguments, and what it returns.
    pub description: &'static str,
    /// The catalog's parameter-array shape: a JSON array of
    /// `{name, type, required, description[, default]}` descriptors.
    /// Build entries with [`param`] / [`param_default`].
    pub parameters: Value,
    /// `true` routes the tool through the existing consent gate and
    /// restricts it to the `destructive_tool_access` device tier.
    pub destructive: bool,
}

/// A Core plugin: an in-process, Rust-native add-on compiled into the
/// core.
///
/// Implementations must be `Send + Sync` — the host stores them behind
/// an `Arc` and invokes [`call`](CorePlugin::call) from async tool
/// handlers on the harness runtime. Keep `call` fast and non-blocking;
/// it runs inline on a runtime worker.
pub trait CorePlugin: Send + Sync {
    /// Plugin id, snake_case (e.g. `"hello_wylde"`). Must match the
    /// folder name under `Core/Plugins/` and is the `<plugin_name>`
    /// segment of every namespaced catalog id.
    fn name(&self) -> &'static str;
    /// Semver-ish version string, surfaced for diagnostics (`about`
    /// tools, logs). Not used for resolution — linkage is compile-time.
    fn version(&self) -> &'static str;
    /// One-line human description of the plugin as a whole.
    fn description(&self) -> &'static str;
    /// Every tool this plugin contributes. Called once at registry
    /// construction; must be deterministic.
    fn tools(&self) -> Vec<PluginTool>;
    /// Dispatch one tool call. `tool` is the plugin-local name (the
    /// host strips its namespace before delegating); `args` is the raw
    /// JSON object from the model.
    ///
    /// Returns the tool result envelope the rest of the catalog uses:
    /// `{"status": "success", ...}` on success and
    /// `{"status": "error", "error": "..."}` on failure (including an
    /// unknown `tool` — return an error envelope, don't panic).
    ///
    /// Sync by design (v1) — the host wraps it async-side. See the
    /// crate header for why an async variant is deferred to v2.
    fn call(&self, tool: &str, args: &Value) -> Value;
}

/// Build one JSON parameter descriptor in the catalog's parameter-array
/// shape. Mirrors the harness registry's `param` helper so plugin
/// schemas stay byte-compatible with built-in tool schemas without the
/// plugin depending on the harness.
pub fn param(name: &str, typ: &str, required: bool, description: &str) -> Value {
    serde_json::json!({
        "name": name,
        "type": typ,
        "required": required,
        "description": description,
    })
}

/// Build a parameter descriptor with a default value (implies
/// `required: false`).
pub fn param_default(name: &str, typ: &str, description: &str, default: Value) -> Value {
    serde_json::json!({
        "name": name,
        "type": typ,
        "required": false,
        "description": description,
        "default": default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Dummy;

    impl CorePlugin for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn version(&self) -> &'static str {
            "0.0.1"
        }
        fn description(&self) -> &'static str {
            "test fixture"
        }
        fn tools(&self) -> Vec<PluginTool> {
            vec![PluginTool {
                name: "echo",
                description: "Echo the args back.",
                parameters: json!([param("text", "string", true, "Text to echo")]),
                destructive: false,
            }]
        }
        fn call(&self, tool: &str, args: &Value) -> Value {
            match tool {
                "echo" => json!({"status": "success", "echo": args}),
                other => json!({"status": "error", "error": format!("unknown tool {other:?}")}),
            }
        }
    }

    #[test]
    fn trait_is_object_safe_and_dispatches() {
        // The host stores plugins as `Arc<dyn CorePlugin>`; this pins
        // object safety and the basic call contract.
        let plugin: std::sync::Arc<dyn CorePlugin> = std::sync::Arc::new(Dummy);
        assert_eq!(plugin.name(), "dummy");
        let out = plugin.call("echo", &json!({"text": "hi"}));
        assert_eq!(out["status"], "success");
        assert_eq!(out["echo"]["text"], "hi");
    }

    #[test]
    fn unknown_tool_returns_error_envelope_not_panic() {
        let plugin = Dummy;
        let out = plugin.call("nope", &json!({}));
        assert_eq!(out["status"], "error");
    }

    #[test]
    fn param_matches_catalog_descriptor_shape() {
        let p = param("path", "string", true, "Path to the file");
        assert_eq!(p["name"], "path");
        assert_eq!(p["type"], "string");
        assert_eq!(p["required"], true);
        assert_eq!(p["description"], "Path to the file");
    }

    #[test]
    fn param_default_is_optional_and_carries_default() {
        let p = param_default("tz", "string", "Time zone", json!("utc"));
        assert_eq!(p["required"], false);
        assert_eq!(p["default"], "utc");
    }

    #[test]
    fn tools_declares_parameter_array() {
        let tools = Dummy.tools();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].parameters.is_array());
        assert!(!tools[0].destructive);
    }
}
