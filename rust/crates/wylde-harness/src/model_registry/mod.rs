//! Unified registry of every model the Wylde harness uses. Phase 8 of
//! the Wylde Rust migration. Rust port of
//! `Core/harness/model_registry/`.
//!
//! ## Scope
//!
//! The Python module is partitioned the same way the Rust port is:
//!
//! * **Kind taxonomy + entry shape** ([`types`]) — `Kind`, `ModelEntry`,
//!   `default_chat_visible`. New code touches this first.
//! * **HF cache scanner** ([`hf_scanner`]) — walk `~/.cache/huggingface/
//!   hub/models--*`, parse repo names, sum file sizes.
//! * **Service-manifest reader** ([`service_manifests`]) — pull `models`
//!   declarations from each top-level service for kind overrides +
//!   `required_by`.
//! * **Heuristics** ([`heuristics`]) — fallback `infer_kind` when no
//!   manifest claims a repo.
//! * **Routing** ([`routing`]) — LLM capability slots, profiles, churn
//!   prevention, opt-in HF discovery state. Sub-modules per concern.
//! * **Public API** ([`api`]) — `list_models`, `get_model`, `is_loaded`,
//!   `refresh_cache`. Merges scanner + manifests + Ollama probe +
//!   routing profiles into one view.
//!
//! ## Strangler-fig env var
//!
//! [`impl_for`] reads `WYLDE_HARNESS_MODEL_REGISTRY_IMPL`. Default is
//! `python` until the Phase 8 parity tests cover the wire shape; the
//! Rust side ships behind the flag so we can dogfood it. The wylde-ollama
//! IPC dispatch for the model-side actions (`ollama.list_loaded`,
//! `ollama.eject`, `ollama.preload`) is independent of this flag — Phase
//! 1 already cut Ollama HTTP calls to the pipe, and that path stays live
//! regardless of the registry's impl.
//!
//! The Ollama-talking files from Python's `_routing/benchmarks.py` and
//! `_routing/ollama_watcher.py` are NOT ported here — those merged into
//! `wylde-ollama` during Phase 1 per the master plan.

pub mod api;
pub mod heuristics;
pub mod hf_scanner;
pub mod routing;
pub mod service_manifests;
pub mod types;
pub mod wakeword_scanner;

/// Read `WYLDE_HARNESS_MODEL_REGISTRY_IMPL` once per call. Default
/// `python`; unknown values clamp to `python` so a typo can't route
/// callers through an incomplete Rust port. Mirrors the
/// `crate::memory::impl_for` shape from Phase 7.
pub fn impl_for() -> &'static str {
    let raw = std::env::var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL").unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "rust" => "rust",
        _ => "python",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;

    #[test]
    fn impl_for_defaults_to_python_when_env_unset() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("WYLDE_HARNESS_MODEL_REGISTRY_IMPL");
        std::env::remove_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL");
        assert_eq!(impl_for(), "python");
        if let Some(v) = prior {
            std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", v);
        }
    }

    #[test]
    fn impl_for_clamps_unknown_to_python() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("WYLDE_HARNESS_MODEL_REGISTRY_IMPL");
        std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", "elixir");
        assert_eq!(impl_for(), "python");
        match prior {
            Some(v) => std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", v),
            None => std::env::remove_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL"),
        }
    }

    #[test]
    fn impl_for_recognises_rust() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("WYLDE_HARNESS_MODEL_REGISTRY_IMPL");
        std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", "rust");
        assert_eq!(impl_for(), "rust");
        std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", "RUST");
        assert_eq!(impl_for(), "rust");
        std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", " rust ");
        assert_eq!(impl_for(), "rust");
        match prior {
            Some(v) => std::env::set_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL", v),
            None => std::env::remove_var("WYLDE_HARNESS_MODEL_REGISTRY_IMPL"),
        }
    }
}
