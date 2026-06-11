//! `hello_wylde` — the reference Core plugin.
//!
//! A Core plugin is an add-on FOR the core: a Rust lib crate living in
//! the Core filesystem (`Core/Plugins/hello_wylde/`), compiled into the
//! core binary and registered into the tool catalog by the harness
//! plugin host (`wylde-harness/src/plugins/`). It is NOT an Extension
//! (those leave the ecosystem via the bridge/Gateway) and NOT a Service
//! (those are sibling suites the core works without).
//!
//! This plugin exists to prove the mechanism end-to-end and to be the
//! template you copy when authoring a real one — see the `README.md`
//! next to this crate for the four install steps.
//!
//! Tools (catalog names after host namespacing):
//!
//! * `plugin.hello_wylde.greet` — `{name?: string}` → a greeting.
//! * `plugin.hello_wylde.about` — no args → plugin name/version, proving
//!   a plugin can ship more than one tool.

use serde_json::{json, Value};
use wylde_plugin_api::{param, CorePlugin, PluginTool};

/// The plugin unit. Stateless here; a real plugin may hold local state
/// (it must stay `Send + Sync` — the host calls it from async handlers).
pub struct HelloWylde;

impl CorePlugin for HelloWylde {
    fn name(&self) -> &'static str {
        "hello_wylde"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &'static str {
        "Reference Core plugin: proves the in-process plugin mechanism with a greeting tool."
    }

    fn tools(&self) -> Vec<PluginTool> {
        vec![
            PluginTool {
                name: "greet",
                description: "Return a friendly greeting. Pass `name` to be greeted \
                              personally; omit it for a generic greeting.",
                parameters: json!([param(
                    "name",
                    "string",
                    false,
                    "Who to greet; defaults to 'world'"
                )]),
                destructive: false,
            },
            PluginTool {
                name: "about",
                description: "Return this plugin's name, version, and description. \
                              Takes no arguments.",
                parameters: json!([]),
                destructive: false,
            },
        ]
    }

    fn call(&self, tool: &str, args: &Value) -> Value {
        match tool {
            "greet" => {
                let who = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("world");
                json!({
                    "status": "success",
                    "greeting": format!("Hello, {who}! — from a Wylde core plugin"),
                })
            }
            "about" => json!({
                "status": "success",
                "plugin": self.name(),
                "version": self.version(),
                "description": self.description(),
            }),
            other => json!({
                "status": "error",
                "error": format!("plugin 'hello_wylde' has no tool {other:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greet_uses_provided_name() {
        let out = HelloWylde.call("greet", &json!({"name": "Aaron"}));
        assert_eq!(out["status"], "success");
        assert_eq!(out["greeting"], "Hello, Aaron! — from a Wylde core plugin");
    }

    #[test]
    fn greet_defaults_to_world_when_name_missing_or_blank() {
        for args in [json!({}), json!({"name": ""}), json!({"name": "   "})] {
            let out = HelloWylde.call("greet", &args);
            assert_eq!(out["greeting"], "Hello, world! — from a Wylde core plugin");
        }
    }

    #[test]
    fn about_reports_identity() {
        let out = HelloWylde.call("about", &json!({}));
        assert_eq!(out["status"], "success");
        assert_eq!(out["plugin"], "hello_wylde");
        assert_eq!(out["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn unknown_tool_returns_error_envelope() {
        let out = HelloWylde.call("nope", &json!({}));
        assert_eq!(out["status"], "error");
        assert!(out["error"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn tools_lists_greet_and_about_with_schemas() {
        let tools = HelloWylde.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["greet", "about"]);
        // greet's only param is optional — the host must not mark it
        // required in the advertised schema.
        assert_eq!(tools[0].parameters[0]["name"], "name");
        assert_eq!(tools[0].parameters[0]["required"], false);
        assert!(tools.iter().all(|t| !t.destructive));
    }
}
