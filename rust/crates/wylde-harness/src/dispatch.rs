//! Tool-call dispatch — routes a decoded tool call to either an
//! internal harness tool (Phase 6 owns the implementations) or an MCP
//! extension tool (Phase 4's `wylde-extension-bridge`).
//!
//! ## Phase 6 surface
//!
//! * [`route`] — pure routing decision over a tool name. Returns
//!   [`Route::McpExtension`] when the name's first dotted segment is a
//!   known MCP extension namespace, else [`Route::Internal`].
//! * [`call_mcp_extension`] — fires `ext.tools.call` against
//!   `wylde-extension-bridge` with the Phase 4 payload shape
//!   `{extension, tool, arguments}`.
//! * [`call_internal`] — real Phase 6 dispatch into the harness's
//!   in-process tool registry. Returns the handler's `Value` on
//!   success, a structured `DispatchError` on failure (tier-blocked,
//!   deferred-stub, or handler error).
//!
//! ## Routing heuristic
//!
//! A real registry lives in Phase 4 (extensions) + Phase 6 (internal
//! tools). For Phase 5.C the routing was configurable via
//! `WYLDE_HARNESS_MCP_NAMESPACES` (comma-separated list of extension
//! namespaces). Defaults to the two shipped extensions (`webcrawler`,
//! `wylde_study`). A tool name shaped `<ns>.<tool>` whose `<ns>` is in
//! that set routes to MCP; everything else stays internal.

use serde_json::{json, Value};
use wylde_shared::ipc::{self, IpcError};

use crate::config::Config;
use crate::tooling::registry::Registry;
use crate::tooling::runner::{dispatch_tool, DispatchOutcome};

/// Routing decision. The two arms of the dispatch surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Hand off to the Phase 6 harness tooling crate (Phase 5.C stub).
    /// Names like `fs.read`, `code.search`, `n8n.list`.
    Internal,
    /// Hand off to `wylde-extension-bridge` via `ext.tools.call`.
    /// Names that resolve to an installed MCP server tool.
    McpExtension,
}

/// Pure routing decision over a tool name.
///
/// Splits on `.`; if the first segment matches one of
/// `cfg.mcp_namespaces`, route to [`Route::McpExtension`]. Otherwise
/// route to [`Route::Internal`]. Matches are exact and
/// case-sensitive — extension folder names are canonical.
pub fn route(cfg: &Config, tool_name: &str) -> Route {
    let Some((ns, _)) = tool_name.split_once('.') else {
        return Route::Internal;
    };
    if cfg.mcp_namespaces.iter().any(|n| n == ns) {
        Route::McpExtension
    } else {
        Route::Internal
    }
}

/// Split a tool name like `webcrawler.scrape` into
/// `("webcrawler", "scrape")`. Used by [`call_mcp_extension`] to fill
/// the `extension` + `tool` fields the Phase 4 contract requires.
///
/// Falls back to `("", name)` when there's no dot — the caller has
/// already routed to MCP, so a name without a namespace is a wire
/// error the bridge will reject with `bad_request`.
pub fn split_mcp_name(tool_name: &str) -> (&str, &str) {
    match tool_name.split_once('.') {
        Some((ns, t)) => (ns, t),
        None => ("", tool_name),
    }
}

/// MCP-extension dispatch. Phase 4 contract:
/// `ext.tools.call({extension, tool, arguments})`. Returns whatever
/// `ext.tools.call` returns — the MCP server's `tools/call` reply
/// envelope, surfaced verbatim.
pub async fn call_mcp_extension(
    cfg: &Config,
    tool_name: &str,
    args: Value,
) -> Result<Value, IpcError> {
    let (extension, tool) = split_mcp_name(tool_name);
    if extension.is_empty() {
        return Err(IpcError::new(
            "bad_request",
            format!(
                "MCP tool name {tool_name:?} missing namespace prefix \
                 (expected `<extension>.<tool>`)"
            ),
        ));
    }
    let payload = json!({
        "extension": extension,
        "tool": tool,
        "arguments": args,
    });
    ipc::call_action(&cfg.extension_bridge_service, "ext.tools.call", payload).await
}

/// Internal-tool dispatch. Phase 6 routes the call into the
/// in-process tool registry; the returned [`DispatchOutcome`] carries
/// the canonical id (so `tool_calls_summary` records the registry's
/// id even when the model emitted an alias) and either the handler's
/// raw `Value` or a structured error.
pub async fn call_internal(
    cfg: &'static Config,
    registry: &Registry,
    tool_name: &str,
    device_tier: &str,
    args: Value,
) -> DispatchOutcome {
    dispatch_tool(registry, cfg, tool_name, device_tier, args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_namespaces(ns: &[&str]) -> Config {
        let mut cfg = Config::default_for_tests();
        cfg.mcp_namespaces = ns.iter().map(|s| (*s).to_string()).collect();
        cfg
    }

    #[test]
    fn route_is_distinct() {
        assert_ne!(Route::Internal, Route::McpExtension);
    }

    #[test]
    fn route_returns_internal_when_no_dot_in_name() {
        let cfg = cfg_with_namespaces(&["webcrawler"]);
        assert_eq!(route(&cfg, "fs_read"), Route::Internal);
    }

    #[test]
    fn route_returns_internal_when_prefix_unknown() {
        let cfg = cfg_with_namespaces(&["webcrawler"]);
        assert_eq!(route(&cfg, "fs.read"), Route::Internal);
        assert_eq!(route(&cfg, "n8n.list"), Route::Internal);
    }

    #[test]
    fn route_returns_mcp_when_prefix_matches() {
        let cfg = cfg_with_namespaces(&["webcrawler", "wylde_study"]);
        assert_eq!(route(&cfg, "webcrawler.scrape"), Route::McpExtension);
        assert_eq!(route(&cfg, "wylde_study.summarise"), Route::McpExtension);
    }

    #[test]
    fn route_is_case_sensitive() {
        let cfg = cfg_with_namespaces(&["webcrawler"]);
        assert_eq!(route(&cfg, "Webcrawler.scrape"), Route::Internal);
    }

    #[test]
    fn split_mcp_name_separates_extension_and_tool() {
        assert_eq!(
            split_mcp_name("webcrawler.scrape"),
            ("webcrawler", "scrape")
        );
    }

    #[test]
    fn split_mcp_name_handles_multi_dot() {
        // Only the first `.` is the separator; the rest is the tool id.
        assert_eq!(
            split_mcp_name("webcrawler.tools.scrape"),
            ("webcrawler", "tools.scrape")
        );
    }

    #[test]
    fn split_mcp_name_returns_empty_extension_when_no_dot() {
        assert_eq!(split_mcp_name("plain"), ("", "plain"));
    }

    #[tokio::test]
    async fn call_internal_dispatches_into_registry() {
        // Phase 12.2 consent gate guards every dispatch; bypass it
        // here under the shared serial guard so the existing
        // call_internal semantics keep being pinned. New consent
        // integration tests live in `tooling::runner::tests`.
        let _g = crate::tooling::consent::bypass_scope(true).await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let registry = crate::tooling::registry::Registry::default();
        // `time.now` is an active Phase 6 tool — should succeed.
        let outcome = call_internal(
            cfg,
            &registry,
            "time.now",
            crate::turn::tool_round::TIER_TOOL_USE,
            json!({}),
        )
        .await;
        let ok = outcome.result.expect("active handler succeeds");
        assert_eq!(ok["status"], "success");
        assert_eq!(outcome.canonical_id, "time_now");
    }

    #[tokio::test]
    async fn call_internal_returns_not_found_for_unknown_tool() {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let registry = crate::tooling::registry::Registry::with_only(vec![]);
        let outcome = call_internal(
            cfg,
            &registry,
            "totally.unknown",
            crate::turn::tool_round::TIER_TOOL_USE,
            json!({}),
        )
        .await;
        let err = outcome.result.expect_err("should fail");
        assert_eq!(err.error.code, "not_found");
    }

    #[tokio::test]
    async fn call_mcp_extension_rejects_unnamespaced_tool() {
        let cfg = Config::default_for_tests();
        let err = call_mcp_extension(&cfg, "plain", json!({}))
            .await
            .expect_err("should reject");
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("namespace"));
    }
}
