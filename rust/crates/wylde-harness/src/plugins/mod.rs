//! Core-plugin host — links the installed plugins and registers their
//! tools into the in-process tool catalog (taxonomy reorg TX S4).
//!
//! ## What a "plugin" is here
//!
//! One of the three add-on tiers, with their defining tests:
//!
//! * **Extensions** leave the ecosystem — bridge/Gateway path
//!   (`Extensions/` + `wylde-extension-bridge`). Not this module.
//! * **Plugins** are add-ons FOR the core, compiled INTO the core —
//!   Rust lib crates under `Core/Plugins/<name>/` against the
//!   `wylde-plugin-api` SDK. This module is their host.
//! * **Services** are sibling full-tier suites the core must keep
//!   working without (`wylde-workspaces`, `wylde-n8n`, …). Not this
//!   module either.
//!
//! ## The `Core/Plugins/` bucket ships EMPTY
//!
//! `Core/Plugins/` is one of the three git-ignored out-of-tree buckets
//! (the others are `Services/` and `Extensions/`). Core's tracked tree is
//! just-Core, so NO plugin is wired in by default: [`installed`] returns
//! an empty vec and Core builds plugin-free. This is what makes the
//! removability contract hold for the Plugins bucket — with the bucket
//! empty, Core still builds and boots, with no plugin tools in the
//! catalog. The `hello_wylde` seed crate stays on disk under
//! `Core/Plugins/hello_wylde/` (git-ignored) as the worked example.
//!
//! ## Installing a plugin — the four steps
//!
//! Discovery is compile-time and deliberate: in-process code is trusted
//! code, so the linkage table below plus the filesystem IS the
//! discovery story (no manifest scan, no dynamic loading — Rust has no
//! stable ABI, so `dlopen`-style plugins are rejected outright).
//!
//! 1. Create the crate folder `Core/Plugins/<plugin_name>/` (package
//!    `wylde-plugin-<name>`, lib-only, depends on `wylde-plugin-api`).
//! 2. Add the workspace member line in `rust/Cargo.toml`:
//!    `"../Core/Plugins/<plugin_name>"`.
//! 3. Add one dependency line in `rust/crates/wylde-harness/Cargo.toml`:
//!    `wylde-plugin-<name> = { path = "../../../Core/Plugins/<plugin_name>" }`.
//! 4. Add one `Box::new(...)` line to [`installed`] below.
//!
//! Uninstalling is the same four steps in reverse.
//!
//! ## Catalog identity
//!
//! Each [`PluginTool`] lands in the registry namespaced by its plugin:
//! canonical id `plugin_<plugin>_<tool>`, dotted name
//! `plugin.<plugin>.<tool>`, group `"plugins"` — matching the
//! id/name/group conventions of the built-in groups (and the
//! `tool_id_regex` canonical form). The `destructive` flag is forwarded
//! verbatim so the existing tier + consent gates apply unchanged.
//!
//! [`CorePlugin::call`] is sync (v1 — see the SDK crate header); the
//! host wraps it in the registry's async handler shape here.

use std::sync::Arc;

use wylde_plugin_api::CorePlugin;

use crate::tooling::registry::{entry_active, Registry};

/// The explicit linkage table: every installed Core plugin. Empty by
/// default — the `Core/Plugins/` bucket ships empty (out-of-tree runtime
/// foundation), so Core builds plugin-free. Installing a plugin = the
/// four steps in the module header, the last of which is one
/// `Box::new(...)` line here, e.g.
/// `vec![Box::new(wylde_plugin_hello_wylde::HelloWylde)]`.
pub fn installed() -> Vec<Box<dyn CorePlugin>> {
    Vec::new()
}

/// Register every installed plugin's tools into the catalog. Called
/// once from [`Registry::default`] alongside the built-in tool groups.
pub fn register(reg: &mut Registry) {
    for plugin in installed() {
        register_plugin(reg, Arc::from(plugin));
    }
}

/// Register one plugin's tools. Split out (and `Arc`-shaped) so tests
/// can register a fixture plugin into a scratch registry.
fn register_plugin(reg: &mut Registry, plugin: Arc<dyn CorePlugin>) {
    let plugin_name = plugin.name();
    for tool in plugin.tools() {
        let id = format!("plugin_{plugin_name}_{}", tool.name);
        let name = format!("plugin.{plugin_name}.{}", tool.name);
        let parameters = match tool.parameters {
            serde_json::Value::Array(items) => items,
            // A non-array `parameters` is a plugin authoring bug; treat
            // it as "no parameters" rather than poisoning the catalog.
            _ => Vec::new(),
        };
        let local_tool = tool.name;
        let handler_plugin = Arc::clone(&plugin);
        reg.insert(entry_active(
            &id,
            &name,
            "plugins",
            tool.description,
            parameters,
            tool.destructive,
            move |args, _cfg| {
                let plugin = Arc::clone(&handler_plugin);
                async move {
                    // CorePlugin::call is sync by design (v1) — wrap it
                    // here so the registry's async handler contract is
                    // satisfied without the SDK growing a runtime dep.
                    Ok(plugin.call(local_tool, &args))
                }
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use wylde_plugin_api::{param, PluginTool};

    use crate::config::Config;
    use crate::tooling::consent;
    use crate::tooling::runner::{catalog_payload, dispatch_tool};
    use crate::turn::tool_round::TIER_TOOL_USE;

    // ── the empty default linkage table ──────────────────────────────

    #[test]
    fn installed_is_empty_by_default() {
        // The `Core/Plugins/` bucket ships EMPTY (out-of-tree runtime
        // foundation): Core builds plugin-free. Installing a plugin is the
        // opt-in four-step wiring in the module header.
        assert!(
            installed().is_empty(),
            "no plugin should be compiled into Core by default"
        );
    }

    #[test]
    fn default_registry_has_no_compiled_in_plugins() {
        // With the bucket empty, Registry::default registers zero plugin
        // tools next to the built-in groups — none in the `plugins` group.
        let reg = Registry::default();
        let rows = catalog_payload(&reg);
        assert!(
            !rows.iter().any(|r| r["group"] == "plugins"),
            "default registry must carry no plugin-group tools when the bucket is empty"
        );
    }

    // ── host behavior against a fixture plugin ──────────────────────
    //
    // The plugin-host machinery (register / namespacing / catalog rows /
    // end-to-end dispatch) is exercised against the in-test `Fixture`
    // below — coverage that no longer depends on a shipped plugin now that
    // the bucket ships empty.

    #[test]
    fn register_namespaces_plugin_tools() {
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        // Canonical id + dotted name forms both resolve.
        let hit = reg
            .lookup("plugin.fixture.ping")
            .expect("dotted name resolves");
        assert_eq!(hit.id, "plugin_fixture_ping");
        assert_eq!(hit.group, "plugins");
        assert!(!hit.destructive);
        assert!(reg.lookup("plugin_fixture_ping").is_some());
    }

    #[test]
    fn catalog_rows_carry_plugin_group_and_active_status() {
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        let rows = catalog_payload(&reg);
        let ping = rows
            .iter()
            .find(|r| r["id"] == "plugin_fixture_ping")
            .expect("ping in catalog");
        assert_eq!(ping["name"], "plugin.fixture.ping");
        assert_eq!(ping["group"], "plugins");
        assert_eq!(ping["status"], "active");
        // The optional `name` param survives namespacing untouched.
        assert_eq!(ping["parameters"][0]["name"], "name");
        assert_eq!(ping["parameters"][0]["required"], false);
    }

    #[tokio::test]
    async fn dispatch_reaches_plugin_end_to_end() {
        // Same harness the built-in dispatch tests use: consent bypass
        // scoped under the serial guard so the gate doesn't intercept
        // (and the flag doesn't leak to the next test).
        let _g = consent::bypass_scope(true).await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "plugin.fixture.ping",
            TIER_TOOL_USE,
            json!({"name": "Sam"}),
        )
        .await;
        assert_eq!(outcome.canonical_id, "plugin_fixture_ping");
        let ok = outcome.result.expect("plugin handler runs");
        assert_eq!(ok["status"], "success");
        assert_eq!(ok["pong"], "Sam");
    }

    // ── more host behavior against the fixture plugin ───────────────

    struct Fixture;

    impl CorePlugin for Fixture {
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn version(&self) -> &'static str {
            "0.0.0"
        }
        fn description(&self) -> &'static str {
            "host-test fixture"
        }
        fn tools(&self) -> Vec<PluginTool> {
            vec![
                PluginTool {
                    name: "ping",
                    description: "Non-destructive sample tool.",
                    parameters: json!([param("name", "string", false, "Optional name")]),
                    destructive: false,
                },
                PluginTool {
                    name: "wipe",
                    description: "Pretend-destructive tool.",
                    parameters: json!([param("target", "string", true, "What to wipe")]),
                    destructive: true,
                },
                PluginTool {
                    name: "broken_schema",
                    description: "Parameters is not an array.",
                    parameters: json!({"oops": true}),
                    destructive: false,
                },
            ]
        }
        fn call(&self, tool: &str, args: &Value) -> Value {
            if tool == "ping" {
                let who = args.get("name").and_then(Value::as_str).unwrap_or("world");
                return json!({"status": "success", "tool": tool, "pong": who});
            }
            json!({"status": "success", "tool": tool})
        }
    }

    #[test]
    fn destructive_flag_is_forwarded_to_the_entry() {
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        let wipe = reg.lookup("plugin.fixture.wipe").expect("registered");
        assert!(
            wipe.destructive,
            "destructive must survive namespacing so tier + consent gates apply"
        );
    }

    #[test]
    fn non_array_parameters_degrade_to_empty_schema() {
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        let entry = reg
            .lookup("plugin.fixture.broken_schema")
            .expect("registered");
        assert!(entry.parameters.is_empty());
    }

    #[tokio::test]
    async fn destructive_plugin_tool_blocked_on_tool_use_tier() {
        let _g = consent::bypass_scope(true).await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let mut reg = Registry::empty();
        register_plugin(&mut reg, Arc::new(Fixture));
        let outcome =
            dispatch_tool(&reg, cfg, "plugin.fixture.wipe", TIER_TOOL_USE, json!({})).await;
        let err = outcome.result.expect_err("tier gate must block");
        assert_eq!(err.error.code, "tier_tool_use_destructive_blocked");
    }
}
