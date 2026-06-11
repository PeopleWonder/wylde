//! Per-model chat request options + the num_ctx-derived slot budget
//! (prompt-engineering improvement plan **B5**).
//!
//! Closes the two halves of the `num_ctx` gap:
//!
//! 1. **Overrides reach the request.** The user's sparse per-model
//!    inference overrides (`settings/ollama_overrides`, written by the
//!    Settings panel) are folded into every chat request's `options`
//!    field via [`chat_options`]. Before this, a `num_ctx` or
//!    `temperature` the user set in Settings was read back by the
//!    Settings verbs and applied to *nothing*.
//!
//! 2. **The eviction budget tracks the model's real window.** The OI-8
//!    token-budget ladder used a single global ceiling
//!    (`DEFAULT_TOKEN_BUDGET` = 100k) regardless of the model. Against a
//!    model loaded at Ollama's default context, the carefully-tiered
//!    eviction could pass a prompt Ollama then truncates **from the
//!    front** — destroying the base instruction and tool catalog first.
//!    [`slot_budget`] derives the gather-slot ceiling from the model's
//!    *effective* `num_ctx` (user override → the model's declared
//!    Modelfile default → Ollama's server default), reserving headroom
//!    for the base prompt, the user message, and the response.
//!
//! `WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET` keeps its historical meaning —
//! when set it IS the slot budget, bypassing the derivation entirely
//! (the explicit deployment knob wins).

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::{Map, Value};
use wylde_shared::ipc;

use crate::config::Config;
use crate::settings::ollama_overrides;
use crate::turn::token_budget;

/// Ollama's server-side default context length when neither the user nor
/// the model's Modelfile declares one. Matches the Ollama default
/// (`OLLAMA_CONTEXT_LENGTH`, 4096 since v0.6); a server configured higher
/// only gains safety margin — we evict a little more than strictly needed,
/// never less.
const OLLAMA_FALLBACK_NUM_CTX: u64 = 4096;

/// Headroom reserved for the model's response when the user hasn't set a
/// `num_predict` override.
const RESPONSE_RESERVE_TOKENS: usize = 1024;

/// The slot budget never derives below this — even a tiny context window
/// keeps room for the never-drop tier to render meaningfully.
const MIN_SLOT_BUDGET: usize = 256;

/// The user's sparse per-model overrides as an Ollama `options` object.
/// Empty when the user has overridden nothing for `model` — the caller
/// then omits `options` and the request is byte-identical to before B5.
pub(crate) fn chat_options(model: &str) -> Map<String, Value> {
    ollama_overrides::get_overrides(ollama_overrides::DEFAULT_PROFILE, model)
}

/// Derive the gather-slot token budget for one turn of `model`.
///
/// `base_prompt` is the rendered base system prompt (instruction + tool
/// catalog) and `user_message` the current ask — both are fixed costs the
/// slots must leave room for. Async because an unoverridden model's
/// declared default is fetched (once, then cached) from the Ollama
/// service.
pub(crate) async fn slot_budget(model: &str, base_prompt: &str, user_message: &str) -> usize {
    // The explicit deployment knob keeps its historical meaning.
    if let Some(b) = env_slot_budget() {
        return b;
    }
    let Some(num_ctx) = effective_num_ctx(model).await else {
        // Service unreachable and no override — we can't know the window;
        // keep the historical ceiling (the chat call is about to fail on
        // the same unreachable service anyway).
        return token_budget::DEFAULT_TOKEN_BUDGET;
    };

    let fixed =
        token_budget::estimate_tokens(base_prompt) + token_budget::estimate_tokens(user_message);
    let reserve = response_reserve(model);
    (num_ctx as usize)
        .saturating_sub(fixed + reserve)
        .max(MIN_SLOT_BUDGET)
}

/// `WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET` when set and positive.
fn env_slot_budget() -> Option<usize> {
    std::env::var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// Response headroom: the user's `num_predict` override when set,
/// otherwise [`RESPONSE_RESERVE_TOKENS`].
fn response_reserve(model: &str) -> usize {
    chat_options(model)
        .get("num_predict")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or(RESPONSE_RESERVE_TOKENS)
}

/// The model's effective `num_ctx`: the user's override when set,
/// otherwise the model's declared Modelfile default (cached after one
/// `ollama.get_model_defaults` round trip), otherwise
/// [`OLLAMA_FALLBACK_NUM_CTX`]. `None` only when the override is absent
/// AND the service was unreachable (transient — not cached, retried next
/// turn).
async fn effective_num_ctx(model: &str) -> Option<u64> {
    if let Some(n) = chat_options(model)
        .get("num_ctx")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        return Some(n);
    }
    declared_num_ctx(model).await
}

/// Successful lookups cached per model tag for the process lifetime —
/// `None` in the map means "the service answered: nothing declared"
/// (resolved to the fallback), so the IPC round trip runs at most once
/// per model. Transport failures are NOT cached.
static DECLARED_NUM_CTX: Lazy<Mutex<HashMap<String, Option<u64>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

async fn declared_num_ctx(model: &str) -> Option<u64> {
    if let Some(cached) = DECLARED_NUM_CTX
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(model)
    {
        return Some(cached.unwrap_or(OLLAMA_FALLBACK_NUM_CTX));
    }

    let cfg = Config::get();
    let reply = ipc::call_action(
        &cfg.ollama_service,
        "ollama.get_model_defaults",
        serde_json::json!({ "model": model }),
    )
    .await;
    match reply {
        Ok(v) => {
            let declared = v.get("num_ctx").and_then(Value::as_u64).filter(|n| *n > 0);
            DECLARED_NUM_CTX
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(model.to_owned(), declared);
            Some(declared.unwrap_or(OLLAMA_FALLBACK_NUM_CTX))
        }
        // Unreachable / unknown model: don't poison the cache; the caller
        // falls back to the historical ceiling for this turn.
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use serde_json::json;

    /// Bind the override store to a fresh temp dir (and pin the budget
    /// env) for one test, mirroring `ollama_overrides`' test harness.
    fn with_temp_store<F: FnOnce()>(f: F) {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let prior_data = std::env::var_os("WYLDE_DATA_DIR");
        let prior_budget = std::env::var_os("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());
        std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
        f();
        match prior_data {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        match prior_budget {
            Some(v) => std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", v),
            None => std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET"),
        }
    }

    #[test]
    fn chat_options_round_trips_the_sparse_override_map() {
        with_temp_store(|| {
            assert!(chat_options("llama3.2:3b").is_empty(), "nothing stored");
            ollama_overrides::set_override(
                ollama_overrides::DEFAULT_PROFILE,
                "llama3.2:3b",
                "num_ctx",
                json!(8192),
            );
            ollama_overrides::set_override(
                ollama_overrides::DEFAULT_PROFILE,
                "llama3.2:3b",
                "temperature",
                json!(0.4),
            );
            let opts = chat_options("llama3.2:3b");
            assert_eq!(opts.get("num_ctx"), Some(&json!(8192)));
            assert_eq!(opts.get("temperature"), Some(&json!(0.4)));
            // Other models are untouched.
            assert!(chat_options("qwen2.5:0.5b").is_empty());
        });
    }

    #[tokio::test]
    async fn slot_budget_derives_from_the_num_ctx_override() {
        // Inline env management (`with_temp_store` can't wrap an await).
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let prior = std::env::var_os("WYLDE_DATA_DIR");
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());
        let prior_budget = std::env::var_os("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
        std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");

        ollama_overrides::set_override(
            ollama_overrides::DEFAULT_PROFILE,
            "m:1",
            "num_ctx",
            json!(8192),
        );
        // base prompt ≈ 1000 tokens (4000 chars), message ≈ 25.
        let base = "x".repeat(4000);
        let msg = "y".repeat(100);
        let budget = slot_budget("m:1", &base, &msg).await;
        // 8192 − 1000 − 25 − 1024 (response reserve) = 6143.
        assert_eq!(budget, 6143);

        // An explicit num_predict override changes the reserve.
        ollama_overrides::set_override(
            ollama_overrides::DEFAULT_PROFILE,
            "m:1",
            "num_predict",
            json!(2048),
        );
        let budget = slot_budget("m:1", &base, &msg).await;
        assert_eq!(budget, 8192 - 1000 - 25 - 2048);

        match prior {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        match prior_budget {
            Some(v) => std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", v),
            None => std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET"),
        }
    }

    #[tokio::test]
    async fn slot_budget_floors_when_fixed_costs_swallow_the_window() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let prior = std::env::var_os("WYLDE_DATA_DIR");
        std::env::set_var("WYLDE_DATA_DIR", tmp.path());
        let prior_budget = std::env::var_os("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
        std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");

        ollama_overrides::set_override(
            ollama_overrides::DEFAULT_PROFILE,
            "tiny:1",
            "num_ctx",
            json!(2048),
        );
        // A base prompt far bigger than the window.
        let base = "x".repeat(40_000);
        let budget = slot_budget("tiny:1", &base, "hi").await;
        assert_eq!(budget, MIN_SLOT_BUDGET);

        match prior {
            Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
            None => std::env::remove_var("WYLDE_DATA_DIR"),
        }
        match prior_budget {
            Some(v) => std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", v),
            None => std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET"),
        }
    }

    #[tokio::test]
    async fn env_budget_bypasses_the_derivation() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET");
        std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", "16000");
        assert_eq!(slot_budget("any:model", "base", "msg").await, 16_000);
        match prior {
            Some(v) => std::env::set_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET", v),
            None => std::env::remove_var("WYLDE_HARNESS_CONTEXT_TOKEN_BUDGET"),
        }
    }
}
