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
) -> DispatchOutcome {
    let started = Instant::now();

    let Some(entry) = registry.lookup(tool_name) else {
        let err = IpcError::new(
            "not_found",
            format!(
                "unknown internal tool {tool_name:?}; not in the harness registry"
            ),
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

    if let Some(block) = check_consent_gate(&entry) {
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
fn check_consent_gate(entry: &ToolEntry) -> Option<DispatchError> {
    if global_bypass_active() {
        return None;
    }
    let outcome = consent_store().check(&entry.id, || {
        format_prompt(&entry.id, &entry.name, &entry.description, entry.destructive)
    });
    match outcome {
        GateOutcome::Allow => None,
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
            Some(DispatchError::with_reason(err, ToolErrorReason::ConsentRequired))
        }
        GateOutcome::Deny { reason } => {
            let mut err = IpcError::new("consent_denied", reason);
            err.details = Some(json!({
                "tool_id": entry.id,
                "tool_name": entry.name,
            }));
            Some(DispatchError::with_reason(err, ToolErrorReason::ConsentDenied))
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
    started
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
mod tests {
    use super::*;
    use crate::tooling::consent::{self, Decision};
    use crate::tooling::registry::{entry_active, entry_deferred};
    use serde_json::json;

    /// Acquire the consent serial guard and set bypass=true. Drop the
    /// returned guard at the end of the test to release. The two
    /// dispatch-test families (bypass-needed and gate-needed) MUST
    /// share the same guard so a gate test never overlaps with a
    /// bypass-needed test mid-flight.
    async fn bypass_scope() -> tokio::sync::MutexGuard<'static, ()> {
        let g = consent::serial_test_guard().await;
        consent::set_bypass_for_tests(true);
        g
    }

    fn make_active_read_only_entry() -> ToolEntry {
        entry_active(
            "read_file",
            "fs.read_file",
            "fs",
            "read a file",
            vec![],
            false,
            |args, _| async move {
                Ok(json!({"echo": args}))
            },
        )
    }

    fn make_active_destructive_entry() -> ToolEntry {
        entry_active(
            "write_file",
            "fs.write_file",
            "fs",
            "write a file",
            vec![],
            true,
            |args, _| async move {
                Ok(json!({"wrote": args}))
            },
        )
    }

    fn make_deferred_entry() -> ToolEntry {
        entry_deferred(
            "memory_search",
            "memory.search",
            "memory",
            "memory search",
            vec![],
            false,
            "7",
            "lands with memory port",
        )
    }

    #[tokio::test]
    async fn dispatch_returns_not_found_for_unknown_tool() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![]);
        let outcome = dispatch_tool(&reg, cfg, "no.such.tool", TIER_TOOL_USE, json!({})).await;
        let err = outcome.result.expect_err("should fail");
        assert_eq!(err.error.code, "not_found");
        assert_eq!(err.reason, Some(ToolErrorReason::ToolCallTextUnrecognised));
    }

    #[tokio::test]
    async fn dispatch_invokes_active_handler() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({"path": "x"})).await;
        let ok = outcome.result.expect("active handler succeeds");
        assert_eq!(ok["echo"]["path"], "x");
        assert_eq!(outcome.canonical_id, "read_file");
    }

    #[tokio::test]
    async fn dispatch_returns_deferred_error_for_deferred_entry() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_deferred_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "memory_search", TIER_TOOL_USE, json!({})).await;
        let err = outcome.result.expect_err("should fail");
        assert_eq!(err.error.code, "phase_7_deferred");
        assert!(err.error.message.contains("Phase 7"));
    }

    #[tokio::test]
    async fn dispatch_blocks_read_only_tier_for_every_tool() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_READ_ONLY, json!({})).await;
        let err = outcome.result.expect_err("should block");
        assert_eq!(err.reason, Some(ToolErrorReason::TierReadOnly));
        assert_eq!(err.error.code, "tier_read_only");
    }

    #[tokio::test]
    async fn dispatch_blocks_destructive_tool_on_tool_use_tier() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.write_file", TIER_TOOL_USE, json!({})).await;
        let err = outcome.result.expect_err("should block");
        assert_eq!(err.reason, Some(ToolErrorReason::TierReadOnly));
        assert_eq!(err.error.code, "tier_tool_use_destructive_blocked");
    }

    #[tokio::test]
    async fn dispatch_allows_destructive_tool_on_destructive_tier() {
        let _g = bypass_scope().await;
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.write_file", TIER_DESTRUCTIVE, json!({"a": 1})).await;
        let ok = outcome.result.expect("destructive tier permits");
        assert_eq!(ok["wrote"]["a"], 1);
    }

    // ── Phase 12.2 consent-gate integration tests ────────────────────
    //
    // These run under a shared serial guard with bypass=off so the
    // global consent store is in a known shape during the dispatch.
    // The store's persistence path is redirected to a per-test
    // tempdir so the host's real `data/preferences/consent.json` is
    // never touched.

    async fn gate_test_scope<F, Fut>(test_body: F)
    where
        F: FnOnce(tempfile::TempDir) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _guard = consent::serial_test_guard().await;
        let prev = consent::global_bypass_active();
        consent::set_bypass_for_tests(false);
        let td = tempfile::TempDir::new().expect("tempdir");
        let path = td.path().join("consent.json");
        // Inject a fresh store-of-record so the test's writes don't
        // race with the real file at <wylde_root>/data/preferences/.
        consent_store_set_path_for_tests(path);
        consent::store().reset().expect("reset injected store");
        // Phase 12.6: hitting the gate now also records a pending
        // entry in the process-wide registry. Clear it so the
        // previous test's residue doesn't bleed in.
        consent::clear_pending();
        test_body(td).await;
        consent::set_bypass_for_tests(prev);
    }

    fn consent_store_set_path_for_tests(path: std::path::PathBuf) {
        // Tunnel through the consent module's test-only helper. Kept
        // separate so the public API surface stays minimal.
        let s = consent::store();
        consent::store_set_path_for_tests_helper(s, path);
    }

    #[tokio::test]
    async fn dispatch_returns_consent_required_when_no_decision() {
        gate_test_scope(|_td| async {
            let cfg = Config::default_for_tests();
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            let reg = Registry::with_only(vec![make_active_read_only_entry()]);
            let outcome =
                dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({})).await;
            let err = outcome.result.expect_err("gate should block");
            assert_eq!(err.error.code, "consent_required");
            assert_eq!(err.reason, Some(ToolErrorReason::ConsentRequired));
            let details = err.error.details.as_ref().expect("details present");
            assert_eq!(details["tool_id"], "read_file");
            assert!(details["prompt"].as_str().unwrap().contains("fs.read_file"));
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_passes_after_approved_decision() {
        gate_test_scope(|_td| async {
            consent::store()
                .set("read_file", Decision::Approved)
                .expect("set approved");
            let cfg = Config::default_for_tests();
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            let reg = Registry::with_only(vec![make_active_read_only_entry()]);
            let outcome =
                dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({"path": "x"}))
                    .await;
            let ok = outcome.result.expect("approved tool dispatches");
            assert_eq!(ok["echo"]["path"], "x");
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_blocks_with_consent_denied_when_denied() {
        gate_test_scope(|_td| async {
            consent::store()
                .set("read_file", Decision::Denied)
                .expect("set denied");
            let cfg = Config::default_for_tests();
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            let reg = Registry::with_only(vec![make_active_read_only_entry()]);
            let outcome =
                dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({})).await;
            let err = outcome.result.expect_err("denied gate blocks");
            assert_eq!(err.error.code, "consent_denied");
            assert_eq!(err.reason, Some(ToolErrorReason::ConsentDenied));
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_passes_when_global_no_auth_set() {
        gate_test_scope(|_td| async {
            consent::store().set_no_auth(true).expect("set no_auth");
            let cfg = Config::default_for_tests();
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            let reg = Registry::with_only(vec![make_active_read_only_entry()]);
            let outcome =
                dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({})).await;
            let ok = outcome.result.expect("no_auth skips the gate");
            assert_eq!(ok["echo"], json!({}));
        })
        .await;
    }

    #[tokio::test]
    async fn tier_gate_runs_before_consent_gate() {
        // Pinned because the user spec says we never prompt the user
        // for a tool the tier would block anyway — that would be
        // noise they can't act on. A read_only tier with a fresh
        // store (no decision yet) must produce `tier_read_only`, NOT
        // `consent_required`.
        gate_test_scope(|_td| async {
            let cfg = Config::default_for_tests();
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            let reg =
                Registry::with_only(vec![make_active_read_only_entry()]);
            let outcome =
                dispatch_tool(&reg, cfg, "fs.read_file", TIER_READ_ONLY, json!({})).await;
            let err = outcome.result.expect_err("tier blocks first");
            assert_eq!(err.error.code, "tier_read_only");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_gate_uses_tempfile_store_not_real_path() {
        // Smoke test: under gate_test_scope, the store writes to the
        // tempdir we injected. The host's real
        // `data/preferences/consent.json` must be untouched.
        gate_test_scope(|td| async move {
            consent::store()
                .set("any_tool", Decision::Approved)
                .expect("set");
            let written = td.path().join("consent.json");
            assert!(
                written.exists(),
                "expected injected consent.json at {}",
                written.display()
            );
        })
        .await;
    }

    #[test]
    fn catalog_payload_lists_one_row_per_canonical_entry() {
        let reg = Registry::with_only(vec![
            make_active_read_only_entry(),
            make_deferred_entry(),
        ]);
        let cat = catalog_payload(&reg);
        assert_eq!(cat.len(), 2);
        let mut ids: Vec<String> = cat
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["memory_search", "read_file"]);
    }

    #[test]
    fn catalog_payload_marks_deferred_status_with_phase() {
        let reg = Registry::with_only(vec![make_deferred_entry()]);
        let cat = catalog_payload(&reg);
        assert_eq!(cat[0]["status"], "deferred");
        assert_eq!(cat[0]["deferred_phase"], "7");
    }
}
