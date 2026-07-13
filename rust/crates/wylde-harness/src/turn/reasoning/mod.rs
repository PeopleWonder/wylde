//! Agentic reasoning layer — gate + phase orchestration (implementation
//! plan §1, harness submodule NOT a crate, per
//! `dispatch_no_new_service_crates_for_harness`).
//!
//! Slice S1 ships the *configuration* surface only:
//!
//! * [`config`] — [`ReasoningConfig`] / [`ModelSlots`] / [`ReasonMode`] /
//!   [`Depth`], persisted at `<data_dir>/settings/reasoning.json`
//!   (RoutingConfig pattern), written through the
//!   `settings.reasoning.{get,set}` facade verbs.
//! * [`fit`] — the pure VRAM fit picker behind the `reasoning.fit_check`
//!   verb ([`handle_fit_check`]).
//! * [`resolve_depth`] — the payload → config → Fast resolution chain,
//!   consumed by `chat.start_turn` / `chat.run_turn` (parsed and logged in
//!   S1; the S3 plan phase is the first consumer that *acts* on it).
//! * [`constrained`] — grammar-constrained decoding plumbing (2026-07-13):
//!   [`constrained::plan_format`] (the `constrained_plan`-gated PlanDag
//!   schema) + [`constrained::ollama_chat_maybe_constrained`] (the
//!   fail-soft `format`-carrying chat call). PLAN wiring lands with S3;
//!   the post-turn memory extractor and the conversation auto-summariser
//!   already call the wrapper live (2026-07-13, policy table in
//!   [`constrained`]'s module docs).
//!
//! **Identity guarantee:** with `ReasoningConfig.enabled == false` (the
//! default) or `depth == Fast`, nothing in this module touches the turn —
//! [`deep_gate_open`] is the single gate expression, and in S1 no caller
//! changes behaviour on its result. The fast path stays byte-identical to
//! trunk; plain vector RAG + plain ReAct.

pub mod config;
pub mod constrained;
pub mod fit;

use std::collections::HashMap;

use serde_json::{json, Value};
use wylde_shared::ipc::{self, Reply};

pub use config::{Depth, ModelSlots, ReasonMode, ReasoningConfig, ReflectGate};
pub use fit::{fit, SlotFit};

/// Resolve the turn's reasoning depth: payload `"depth"` → config
/// `default_depth` → `Fast`. Mirrors `resolve_model`'s payload-then-config
/// fallback. Malformed payload values fall through (tolerant, never fail a
/// turn on a bad flag).
pub fn resolve_depth(payload: &Value) -> Depth {
    payload
        .get("depth")
        .and_then(Value::as_str)
        .and_then(Depth::parse)
        .unwrap_or_else(|| ReasoningConfig::current().default_depth)
}

/// The one gate expression (plan §2): a turn enters PLAN only when the
/// resolved depth is `Deep` **and** the master toggle is on. `false` ⇒
/// today's exact path — plain vector RAG, plain ReAct, byte-identical.
pub fn deep_gate_open(depth: Depth) -> bool {
    depth == Depth::Deep && ReasoningConfig::current().enabled
}

/// Multiplier applied to on-disk model size to estimate resident bytes —
/// the same convention as `wylde-ollama`'s estimate path
/// (`WYLDE_OLLAMA_VRAM_ESTIMATE_MULT`, default 1.2).
fn vram_estimate_mult() -> f64 {
    std::env::var("WYLDE_OLLAMA_VRAM_ESTIMATE_MULT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|m| *m > 0.0)
        .unwrap_or(1.2)
}

/// The broker service the budget probe asks. Same default as
/// `wylde-ollama`'s `broker_service`.
fn broker_service() -> String {
    std::env::var("WYLDE_HARNESS_BROKER_SERVICE").unwrap_or_else(|_| "wylde-vram-broker".to_owned())
}

/// `reasoning.fit_check {slots?, mode?}` — price the (given or configured)
/// slot set against the live VRAM budget. Reply: the serialized
/// [`SlotFit`]. **Fail-soft end to end**: an unreachable Ollama prices
/// every model unknown, an unreachable broker reports budget 0 — both
/// degrade to warnings in the verdict, never an error reply. Advisory
/// only; nothing gates on it (readiness-chip pattern).
pub async fn handle_fit_check(payload: Value) -> Reply {
    let cfg = ReasoningConfig::current();
    // Optional overrides: price a combo the user is *considering* without
    // persisting it first.
    let slots = payload
        .get("slots")
        .map(|v| {
            serde_json::from_value::<ModelSlots>(v.clone()).unwrap_or_else(|_| cfg.slots.clone())
        })
        .unwrap_or_else(|| cfg.slots.clone());
    let mode = payload
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "split" => Some(ReasonMode::Split),
            "single" => Some(ReasonMode::Single),
            _ => None,
        })
        .unwrap_or(cfg.mode);

    let sizes = probe_model_sizes().await;
    let budget = probe_vram_budget().await;
    let verdict = fit::fit(&slots, mode, budget, &sizes);
    Reply::ok(serde_json::to_value(&verdict).unwrap_or_else(|_| json!({})))
}

/// Estimated resident bytes per pulled model tag: `ollama.list_models`
/// (`/api/tags` passthrough) `models[].{name,size}` × the estimate mult.
/// Unreachable / malformed ⇒ empty map (every model "unknown", warned).
async fn probe_model_sizes() -> HashMap<String, u64> {
    let harness_cfg = crate::config::Config::get();
    let mult = vram_estimate_mult();
    let reply =
        ipc::send_action(&harness_cfg.ollama_service, "ollama.list_models", json!({})).await;
    if !reply.ok {
        return HashMap::new();
    }
    reply
        .data
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name").and_then(Value::as_str)?;
                    let size = m.get("size").and_then(Value::as_u64).filter(|&n| n > 0)?;
                    Some((name.to_owned(), (size as f64 * mult) as u64))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// GPU budget from the broker's `vram.state` (`gpu.total_bytes`).
/// Unreachable ⇒ 0 (fit reports "budget unknown").
async fn probe_vram_budget() -> u64 {
    let reply = ipc::send_action(&broker_service(), "vram.state", json!({})).await;
    if !reply.ok {
        return 0;
    }
    reply
        .data
        .get("gpu")
        .and_then(|g| g.get("total_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_depth_payload_wins() {
        assert_eq!(resolve_depth(&json!({ "depth": "deep" })), Depth::Deep);
        assert_eq!(resolve_depth(&json!({ "depth": "fast" })), Depth::Fast);
    }

    #[test]
    fn resolve_depth_falls_back_to_fast() {
        // No payload flag + default config (default_depth: Fast) ⇒ Fast.
        // (The config cache seeds from a missing file as default in the
        // test env — the identity default.)
        assert_eq!(resolve_depth(&json!({})), Depth::Fast);
        // Malformed values fall through the chain, never error.
        assert_eq!(resolve_depth(&json!({ "depth": "sideways" })), Depth::Fast);
        assert_eq!(resolve_depth(&json!({ "depth": 3 })), Depth::Fast);
    }

    #[test]
    fn gate_is_closed_by_default() {
        // Default config: enabled == false ⇒ even an explicit Deep turn
        // does not open the gate. THE identity guard.
        assert!(!deep_gate_open(Depth::Deep) || ReasoningConfig::current().enabled);
        assert!(!deep_gate_open(Depth::Fast), "Fast never opens the gate");
    }

    #[test]
    fn estimate_mult_defaults() {
        // Not asserting against the env var (other tests may set it) —
        // just the parse contract on the default path.
        let m = vram_estimate_mult();
        assert!(m > 0.0);
    }
}
