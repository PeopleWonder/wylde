//! Per-panel IPC helpers for the Organize panel.
//!
//! Every call goes through `wylde-organize`'s `/__action__` pipe envelope.
//! Helpers translate the JSON reply into small Rust view-structs the View
//! consumes, so the rendering layer never sees `serde_json::Value` directly.
//! The panel keeps the *raw* plan `Value` alongside the parsed view so an apply
//! can resend the exact curated plan without a lossy round-trip.

use serde::Deserialize;
use serde_json::{json, Value};

pub const SVC_ORGANIZE: &str = "wylde-organize";

// ── View structs (mirrors of the service's plan.rs, parsed from the wire) ──

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct OpView {
    pub id: u32,
    pub kind: String,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RemovalView {
    pub path: String,
    pub reason: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SkippedView {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct StatsView {
    #[serde(default)]
    pub files_scanned: u32,
    #[serde(default)]
    pub ops_proposed: u32,
    #[serde(default)]
    pub removals_proposed: u32,
    #[serde(default)]
    pub skipped_protected: u32,
    #[serde(default)]
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PlanView {
    pub plan_id: String,
    #[serde(default)]
    pub scope_tier: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub ops: Vec<OpView>,
    #[serde(default)]
    pub removals: Vec<RemovalView>,
    #[serde(default)]
    pub skipped: Vec<SkippedView>,
    #[serde(default)]
    pub stats: StatsView,
}

/// A proposed plan + the raw wire value (so apply can resend the curated form).
#[derive(Debug, Clone)]
pub struct Proposal {
    pub view: PlanView,
    pub raw: Value,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ApplyView {
    #[serde(default)]
    pub applied: u32,
    #[serde(default)]
    pub skipped: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default)]
    pub undo_token: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct UndoView {
    #[serde(default)]
    pub plan_id: String,
    #[serde(default)]
    pub restored: u32,
    #[serde(default)]
    pub skipped: u32,
    #[serde(default)]
    pub failed: u32,
}

// ── verb helpers ─────────────────────────────────────────────────────────

async fn action(verb: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_ORGANIZE,
        "POST",
        "/__action__",
        Some(json!({ "action": verb, "payload": payload })),
    )
    .await
}

/// Scan a scope and return the read-only proposal. `payload` is the full scope
/// request (tier / roots / opt_in / typed_confirmation / options).
pub async fn propose(payload: Value) -> Result<Proposal, String> {
    let raw = action("organize.propose", payload).await?;
    let view: PlanView =
        serde_json::from_value(raw.clone()).map_err(|e| format!("malformed plan: {e}"))?;
    Ok(Proposal { view, raw })
}

/// Apply a curated plan (already filtered to the accepted ops + removals).
pub async fn apply(curated_plan: Value) -> Result<ApplyView, String> {
    let v = action("organize.apply", json!({ "plan": curated_plan })).await?;
    serde_json::from_value(v).map_err(|e| format!("malformed apply outcome: {e}"))
}

/// Undo a plan by token (or `"latest"`).
pub async fn undo(token: &str) -> Result<UndoView, String> {
    let v = action("organize.undo", json!({ "undo_token": token })).await?;
    serde_json::from_value(v).map_err(|e| format!("malformed undo outcome: {e}"))
}

/// Build the curated plan to send to apply: clone the raw proposal and drop the
/// ops whose ids are rejected + the removals whose paths are rejected.
pub fn curate(
    raw: &Value,
    rejected_ops: &std::collections::HashSet<u32>,
    rejected_removals: &std::collections::HashSet<String>,
) -> Value {
    let mut plan = raw.clone();
    if let Some(ops) = plan.get_mut("ops").and_then(Value::as_array_mut) {
        ops.retain(|op| {
            op.get("id")
                .and_then(Value::as_u64)
                .map(|id| !rejected_ops.contains(&(id as u32)))
                .unwrap_or(true)
        });
    }
    if let Some(rems) = plan.get_mut("removals").and_then(Value::as_array_mut) {
        rems.retain(|r| {
            r.get("path")
                .and_then(Value::as_str)
                .map(|p| !rejected_removals.contains(p))
                .unwrap_or(true)
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn plan_view_parses_service_reply() {
        let v = json!({
            "plan_id": "plan-1",
            "scope_tier": "user_data",
            "roots": ["C:/Users/x/Downloads"],
            "ops": [{
                "id": 1, "kind": "move",
                "from": "C:/Users/x/Downloads/a.pdf",
                "to": "C:/Users/x/Downloads/Documents/a.pdf",
                "rationale": "group", "confidence": 0.95
            }],
            "removals": [{ "path": "C:/Users/x/Downloads/old.tmp", "reason": "temp", "size": 12, "detail": "temp" }],
            "skipped": [{ "path": "C:/Windows", "reason": "protected: os_directory" }],
            "stats": { "files_scanned": 3, "ops_proposed": 1, "removals_proposed": 1, "skipped_protected": 1, "reclaimable_bytes": 12 }
        });
        let plan: PlanView = serde_json::from_value(v).unwrap();
        assert_eq!(plan.plan_id, "plan-1");
        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.ops[0].kind, "move");
        assert_eq!(plan.removals[0].reason, "temp");
        assert_eq!(plan.skipped[0].reason, "protected: os_directory");
        assert_eq!(plan.stats.reclaimable_bytes, 12);
    }

    #[test]
    fn curate_drops_rejected_ops_and_removals() {
        let raw = json!({
            "ops": [{ "id": 1, "kind": "move", "to": "x" }, { "id": 2, "kind": "mkdir", "to": "y" }],
            "removals": [{ "path": "keep", "reason": "temp" }, { "path": "drop", "reason": "junk" }],
        });
        let mut rej_ops = HashSet::new();
        rej_ops.insert(2u32);
        let mut rej_rems = HashSet::new();
        rej_rems.insert("drop".to_string());
        let curated = curate(&raw, &rej_ops, &rej_rems);
        let ops = curated["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["id"], 1);
        let rems = curated["removals"].as_array().unwrap();
        assert_eq!(rems.len(), 1);
        assert_eq!(rems[0]["path"], "keep");
    }
}
