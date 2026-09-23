//! Per-op tier/consent refinement for verb dispatch (plan §3.1, R7).
//!
//! ## The problem this solves
//!
//! The per-tool [`crate::tooling::runner`] gates key off one
//! `ToolEntry.destructive` bool. A verb tool like `wylde_delete` can't
//! carry a single honest flag: `delete(memory)` is destructive but
//! `get(memory)` is not. The resolved truth lives in
//! [`ResourceDefinition::is_destructive`] — known only *after* the
//! dispatcher resolves `(resource_type, op)`.
//!
//! ## How it layers with the existing gates (no runner/consent change)
//!
//! The eight verb `ToolEntry`s keep a **coarse** `destructive` flag
//! (read verbs `false`; mutating verbs `true`) so the *existing* runner
//! tier + consent gates fire correctly today — that is the "verb tools
//! go through the existing gates, same paths as today" guarantee.
//!
//! This module adds the **fine** layer: once the op resolves, the
//! dispatcher computes the effective per-`(resource, op)` destructive
//! bool. The fine consent gate only runs when the coarse verb gate did
//! *not* already prompt (i.e. the verb entry was non-destructive) — so
//! there is never a double prompt. It reuses the process-wide
//! [`crate::tooling::consent`] store verbatim; nothing in `consent.rs`
//! or `runner.rs` changes.
//!
//! In Slice 1 the resource registry is empty, so real dispatch returns
//! `not_found` before reaching this gate; the helper is exercised by the
//! unit tests here and wired into the dispatcher so Slice 2+ inherit it
//! the moment they register a destructive op under a read-ish verb.

use serde_json::json;
use wylde_shared::ipc::IpcError;

use crate::events::ToolErrorReason;
use crate::tooling::consent::{
    format_prompt, global_bypass_active, record_pending, store as consent_store, GateOutcome,
};

use super::definition::ResourceOp;

/// Outcome of the fine per-op consent gate.
pub enum OpGate {
    /// Proceed to the `OpHandler`.
    Allow,
    /// Block — the dispatcher surfaces this as the verb call's error,
    /// with the matching wire reason.
    Block {
        error: IpcError,
        reason: ToolErrorReason,
    },
}

/// Apply the fine per-`(resource, op)` consent gate.
///
/// * `coarse_destructive` — the verb `ToolEntry`'s static flag. When
///   `true`, the runner's consent gate already prompted (keyed on the
///   verb id), so this fine gate is skipped to avoid a double prompt.
/// * `effective_destructive` — `def.is_destructive(op)`. When `false`,
///   the op reads only and never needs consent.
///
/// The gate is keyed on `<resource_type>.<op>` so each resource/op pair
/// records its own decision independently of the coarse verb id.
pub fn op_consent_gate(
    resource_type: &str,
    op: ResourceOp,
    effective_destructive: bool,
    coarse_destructive: bool,
) -> OpGate {
    // Read-only op, or the runner already handled consent for a coarse
    // destructive verb — nothing to add.
    if !effective_destructive || coarse_destructive {
        return OpGate::Allow;
    }
    if global_bypass_active() {
        return OpGate::Allow;
    }

    let key = format!("{resource_type}.{}", op.as_str());
    let outcome = consent_store().check(&key, || {
        let tool_name = format!("wylde_{}", op.as_str());
        let desc = format!("{} on the {resource_type} resource", op.as_str());
        format_prompt(&key, &tool_name, &desc, true)
    });

    match outcome {
        GateOutcome::Allow => OpGate::Allow,
        GateOutcome::Pending { prompt } => {
            let pending_id = record_pending(&key, prompt.clone(), "deny");
            let mut err = IpcError::new(
                "consent_required",
                format!(
                    "verb dispatch of {:?} ({}) blocked: no stored consent \
                     decision for this destructive resource op. GUI: surface \
                     the prompt and call `consent.respond`.",
                    key,
                    op.as_str()
                ),
            );
            err.details = Some(json!({
                "id": pending_id,
                "tool_id": key,
                "resource_type": resource_type,
                "op": op.as_str(),
                "destructive": true,
                "prompt": prompt,
                "default_action": "deny",
            }));
            OpGate::Block {
                error: err,
                reason: ToolErrorReason::ConsentRequired,
            }
        }
        GateOutcome::Deny { reason } => {
            let mut err = IpcError::new("consent_denied", reason);
            err.details = Some(json!({
                "tool_id": key,
                "resource_type": resource_type,
                "op": op.as_str(),
            }));
            OpGate::Block {
                error: err,
                reason: ToolErrorReason::ConsentDenied,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::consent;

    #[test]
    fn read_only_op_always_allows() {
        let g = op_consent_gate("memory", ResourceOp::Get, false, false);
        assert!(matches!(g, OpGate::Allow));
    }

    #[test]
    fn coarse_destructive_skips_fine_gate() {
        // Verb entry was already destructive → runner prompted → skip.
        let g = op_consent_gate("memory", ResourceOp::Delete, true, true);
        assert!(matches!(g, OpGate::Allow));
    }

    #[tokio::test]
    async fn bypass_allows_destructive_op() {
        let _guard = consent::bypass_scope(true).await;
        let g = op_consent_gate("memory", ResourceOp::Delete, true, false);
        assert!(matches!(g, OpGate::Allow));
    }
}
