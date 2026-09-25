//! Tool dispatcher — looks up the resolved entry, applies the registry
//! tier gate, invokes the handler. Rust port of
//! `Core/harness/tooling/tool_runner/__init__.py`'s `run_tool`.
//!
//! ## Outcome shape
//!
//! [`DispatchOutcome`] carries either an `Ok` value (handler ran clean),
//! an `Err` IpcError (handler failed, or registry/tier blocked the
//! call), and the resolved canonical id so the caller can record the
//! id the model would have used had the alias not been needed.

use std::time::Instant;

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::config::Config;
use crate::events::ToolErrorReason;
use crate::tooling::consent::{
    format_prompt, global_bypass_active, record_pending, store as consent_store, GateOutcome,
};
use crate::tooling::registry::{HandlerKind, Registry, ToolEntry};
use crate::turn::tool_round::{TIER_DESTRUCTIVE, TIER_READ_ONLY, TIER_TOOL_USE};

/// One dispatch result. Returned by [`dispatch_tool`] so callers can
/// thread both the canonical id and the per-call elapsed time back
/// into the turn loop's [`super::super::turn::tool_round::ToolSummary`]
/// without re-measuring or re-resolving.
pub struct DispatchOutcome {
    pub canonical_id: String,
    pub elapsed_ms: u64,
    pub result: Result<Value, DispatchError>,
}

/// Wrapper around `IpcError` that adds the optional structured
/// `ToolErrorReason` the salvage layer and tool_round map to a wire
/// reason. Active-handler failures carry `None`; tier-block + deferred
/// failures carry the matching `ToolErrorReason`.
#[derive(Debug)]
pub struct DispatchError {
    pub error: IpcError,
    pub reason: Option<ToolErrorReason>,
}

impl DispatchError {
    fn new(error: IpcError) -> Self {
        Self {
            error,
            reason: None,
        }
    }
    fn with_reason(error: IpcError, reason: ToolErrorReason) -> Self {
        Self {
            error,
            reason: Some(reason),
        }
    }
}

/// Dispatch one tool call.
///
/// * `tool_name` is whatever the salvage parser emitted — canonical id,
///   dotted name, or any of the alias forms. The registry's lookup
///   table resolves it.
/// * `device_tier` is the turn's normalised tier string from
///   [`crate::turn::tool_round::normalise_device_tier`].
/// * `args` is the raw `Value` from the model's tool call.
///
/// Returns a [`DispatchOutcome`]; the caller decides how to surface it
/// into the turn loop (via `ToolResult` / `ToolError` events + tool
/// message JSON).
pub async fn dispatch_tool(
    registry: &Registry,
    cfg: &'static Config,
    tool_name: &str,
    device_tier: &str,
    args: Value,
    confirm: bool,
) -> DispatchOutcome {
    let started = Instant::now();

    let Some(entry) = registry.lookup(tool_name) else {
        let err = IpcError::new(
            "not_found",
            format!("unknown internal tool {tool_name:?}; not in the harness registry"),
        );
        return DispatchOutcome {
            canonical_id: tool_name.to_string(),
            elapsed_ms: duration_ms(started),
            result: Err(DispatchError::with_reason(
                err,
                ToolErrorReason::ToolCallTextUnrecognised,
            )),
        };
    };

    let canonical_id = entry.id.clone();

    if let Some(block) = check_registry_tier(device_tier, &entry) {
        return DispatchOutcome {
            canonical_id,
            elapsed_ms: duration_ms(started),
            result: Err(block),
        };
    }

    if let Some(block) = check_consent_gate(&entry, confirm) {
        return DispatchOutcome {
            canonical_id,
            elapsed_ms: duration_ms(started),
            result: Err(block),
        };
    }

    let result = invoke_entry(&entry, args, cfg).await;
    DispatchOutcome {
        canonical_id,
        elapsed_ms: duration_ms(started),
        result: result.map_err(DispatchError::new),
    }
}

/// Phase-12.2 consent gate. Runs after the tier gate so a tool the
/// tier would refuse anyway never produces a consent prompt — that
/// would be noise the user can't act on. Returns `None` on
/// `GateOutcome::Allow` (proceed to handler); otherwise returns the
/// shaped `DispatchError` the turn loop will surface to the model and
/// the GUI.
///
/// `confirm` is a per-call, non-persisted confirmation (the MCP surface
/// sets it from an explicit `confirm: true`). It satisfies an **undecided**
/// gate (`Pending`) for this one dispatch only — it never writes a stored
/// decision, and it deliberately does **not** override a stored
/// `Deny`: a caller can confirm a not-yet-decided tool, but can never
/// override the user's explicit "deny". A stored `Allow` needs no confirm.
fn check_consent_gate(entry: &ToolEntry, confirm: bool) -> Option<DispatchError> {
    if global_bypass_active() {
        return None;
    }
    let outcome = consent_store().check(&entry.id, || {
        format_prompt(
            &entry.id,
            &entry.name,
            &entry.description,
            entry.destructive,
        )
    });
    match outcome {
        GateOutcome::Allow => None,
        // A per-call confirmation clears an undecided gate — but only an
        // undecided one. The `Deny` arm below is intentionally NOT reached
        // by confirm, so an explicit deny always wins.
        GateOutcome::Pending { .. } if confirm => None,
        GateOutcome::Pending { prompt } => {
            // Phase 12.6: also record the prompt in the pending
            // registry so the `consent.stream_pending` subscribers
            // (GUI toasts) see it in real time, and include the
            // generated id in the error details so the GUI can
            // correlate the dispatch error with the toast.
            let default_action = if entry.destructive { "deny" } else { "allow" };
            let pending_id = record_pending(&entry.id, prompt.clone(), default_action);
            let mut err = IpcError::new(
                "consent_required",
                format!(
                    "tool {:?} dispatch blocked: no stored consent decision. \
                     GUI: surface the prompt and call `consent.respond` with \
                     decision=\"approved\" or \"denied\".",
                    entry.name
                ),
            );
            err.details = Some(json!({
                "id": pending_id,
                "tool_id": entry.id,
                "tool_name": entry.name,
                "destructive": entry.destructive,
                "prompt": prompt,
                "default_action": default_action,
            }));
            Some(DispatchError::with_reason(
                err,
                ToolErrorReason::ConsentRequired,
            ))
        }
        GateOutcome::Deny { reason } => {
            let mut err = IpcError::new("consent_denied", reason);
            err.details = Some(json!({
                "tool_id": entry.id,
                "tool_name": entry.name,
            }));
            Some(DispatchError::with_reason(
                err,
                ToolErrorReason::ConsentDenied,
            ))
        }
    }
}

/// Registry-aware tier gate. The base `tool_round::check_tier_gate`
/// blocks every call on `read_only` and otherwise allows; this one
/// additionally consults the entry's `destructive` flag and denies on
/// `tool_use` when the tool is marked destructive.
fn check_registry_tier(device_tier: &str, entry: &ToolEntry) -> Option<DispatchError> {
    let tier = if device_tier.is_empty() {
        TIER_TOOL_USE
    } else {
        device_tier
    };
    match tier {
        TIER_READ_ONLY => Some(DispatchError::with_reason(
            IpcError::new(
                "tier_read_only",
                format!(
                    "tool {:?} blocked: device tier is 'read_only', no tools \
                     may run on this turn",
                    entry.name
                ),
            ),
            ToolErrorReason::TierReadOnly,
        )),
        TIER_DESTRUCTIVE => None,
        // `tool_use` (and any unknown tier) — destructive tools blocked.
        _ => {
            if entry.destructive {
                Some(DispatchError::with_reason(
                    IpcError::new(
                        "tier_tool_use_destructive_blocked",
                        format!(
                            "tool {:?} blocked: device tier is 'tool_use' \
                             but this tool is destructive; needs \
                             'destructive_tool_access' tier",
                            entry.name
                        ),
                    ),
                    ToolErrorReason::TierReadOnly,
                ))
            } else {
                None
            }
        }
    }
}

async fn invoke_entry(
    entry: &ToolEntry,
    args: Value,
    cfg: &'static Config,
) -> Result<Value, IpcError> {
    match &entry.kind {
        HandlerKind::Active(handler) => handler.call(args, cfg).await,
        HandlerKind::Deferred { phase, reason } => Err(IpcError::new(
            format!("phase_{phase}_deferred"),
            format!(
                "tool {:?} is registered but not yet implemented in Rust ({reason}). \
                 Tracking under Phase {phase} of the migration.",
                entry.name
            ),
        )),
    }
}

fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Build the catalog payload that `tools.list` returns. One row per
/// canonical entry; aliases are not duplicated.
pub fn catalog_payload(registry: &Registry) -> Vec<Value> {
    registry
        .canonical_entries()
        .into_iter()
        .map(|e| {
            let status = match &e.kind {
                HandlerKind::Active(_) => "active",
                HandlerKind::Deferred { .. } => "deferred",
            };
            let deferred_phase = match &e.kind {
                HandlerKind::Deferred { phase, .. } => Some(*phase),
                HandlerKind::Active(_) => None,
            };
            json!({
                "id": e.id,
                "name": e.name,
                "group": e.group,
                "description": e.description,
                "parameters": e.parameters,
                "destructive": e.destructive,
                "status": status,
                "deferred_phase": deferred_phase,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
