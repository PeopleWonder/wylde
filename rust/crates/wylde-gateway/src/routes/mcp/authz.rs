//! Authorization — which tools an MCP caller may see and run.
//!
//! Pure policy, no I/O: the curated read/query allow-list, the device tier
//! allowed to reach destructive tools, and the [`ToolAccess`] classification
//! of a harness catalog entry. [`super::adapters::classify_tool`] feeds it the
//! live catalog entry; [`super::handlers`] turns the verdict into a JSON-RPC
//! error or a dispatch. The harness `destructive` flag is the source of truth;
//! the harness tier gate remains the final backstop behind this layer.

use serde_json::Value;

/// Tools exposed over MCP — a curated allow-list, not the full harness
/// catalog. Every entry is verified non-`destructive` in the harness
/// registry AND is a pure read/query with no execution or egress side
/// effect (so `wylde_execute`, `wylde_create`, `voice_*`, model-management
/// and every `destructive` tool are deliberately absent). MCP is an
/// unattended surface: a tool that mutates, deletes, executes, or reaches
/// the network does not belong here regardless of its `destructive` flag.
///
/// This is defence in depth over the harness tier gate — even a caller on
/// the `destructive_tool_access` tier only ever sees these tools through
/// MCP. Widen it deliberately; a new entry is a new remote capability.
pub const MCP_TOOL_ALLOWLIST: &[&str] = &[
    "read_file",
    "list_files",
    "code_search",
    "code_search_files",
    "graph_query",
    "memory_search",
    "memory_workspace_search",
    "memory_workspace_list",
    "show_diff",
    "time_now",
    "time_format",
    "tool_search",
];

/// Whether `name` is on the non-destructive read/query allow-list.
pub fn is_exposable(name: &str) -> bool {
    MCP_TOOL_ALLOWLIST.contains(&name)
}

/// The device tier that may run destructive tools. Mirrors the harness
/// `destructive_tool_access` tier — the harness tier gate is the final
/// backstop, this is the MCP-layer mirror so a lower tier is refused
/// before the pipe is touched.
pub const TIER_DESTRUCTIVE: &str = "destructive_tool_access";

/// Whether `tier` may run destructive tools over MCP.
pub fn tier_allows_destructive(tier: &str) -> bool {
    tier == TIER_DESTRUCTIVE
}

/// How a named tool may be reached over MCP, resolved against the live
/// harness catalog — the `destructive` flag is the source of truth.
#[derive(Debug, PartialEq, Eq)]
pub enum ToolAccess {
    /// Non-destructive and allow-listed — runnable with no confirmation.
    Allowed,
    /// Destructive — runnable only by a `destructive_tool_access` caller
    /// that explicitly confirms.
    Destructive,
    /// Unknown tool, or a non-destructive tool that is not allow-listed.
    NotExposed,
}

/// The `destructive` flag on a raw catalog entry (default `false`).
pub(super) fn entry_destructive(entry: &Value) -> bool {
    entry
        .get("destructive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Pure classification of a catalog entry (or its absence) for `name`.
/// Destructive tools are `Destructive`; a non-destructive tool is
/// `Allowed` only when allow-listed, else `NotExposed`; a missing entry is
/// `NotExposed`.
pub(super) fn classify_entry(entry: Option<&Value>, name: &str) -> ToolAccess {
    match entry {
        None => ToolAccess::NotExposed,
        Some(e) if entry_destructive(e) => ToolAccess::Destructive,
        Some(_) if is_exposable(name) => ToolAccess::Allowed,
        Some(_) => ToolAccess::NotExposed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_entry_maps_catalog_to_access() {
        assert_eq!(
            classify_entry(
                Some(&json!({ "id": "read_file", "destructive": false })),
                "read_file"
            ),
            ToolAccess::Allowed
        );
        assert_eq!(
            classify_entry(
                Some(&json!({ "id": "write_file", "destructive": true })),
                "write_file"
            ),
            ToolAccess::Destructive
        );
        // Non-destructive off-list tool → not exposed.
        assert_eq!(
            classify_entry(
                Some(&json!({ "id": "execute_bash", "destructive": false })),
                "execute_bash"
            ),
            ToolAccess::NotExposed
        );
        // Missing entry → not exposed.
        assert_eq!(classify_entry(None, "ghost"), ToolAccess::NotExposed);
    }

    #[test]
    fn allowlist_contains_no_known_destructive_tool() {
        // Drift guard: the hand-maintained allow-list must never gain a
        // tool the harness classifies destructive. If a rename or a new
        // entry trips this, the fix is to keep the allow-list read-only.
        const KNOWN_DESTRUCTIVE: &[&str] = &[
            "write_file",
            "edit_file",
            "apply_patch",
            "memory_long_term_save",
            "memory_update",
            "memory_delete",
            "memory_workspace_save",
            "memory_workspace_update",
            "memory_workspace_delete",
            "preload_model",
            "evict_model",
            "auto_evict_lru",
            "voice_mic_start",
            "voice_mic_stop",
            "voice_wakeword_start",
            "voice_wakeword_stop",
            "wylde_create",
            "wylde_update",
            "wylde_delete",
            "wylde_list",
            "wylde_search",
            "wylde_execute",
        ];
        for name in MCP_TOOL_ALLOWLIST {
            assert!(
                !KNOWN_DESTRUCTIVE.contains(name),
                "allow-listed tool {name:?} is destructive — MCP must not expose it"
            );
        }
    }
}
