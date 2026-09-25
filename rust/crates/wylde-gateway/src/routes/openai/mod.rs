//! `/v1` — OpenAI-compatible API (#88).
//!
//! One endpoint any OpenAI client (Continue first) can point at, backed by
//! `wylde-ollama` so every inference call is brokered through the VRAM
//! broker, and gated by a Wylde device token like `/mcp`.
//!
//! | Route | Module |
//! |---|---|
//! | `GET /v1/models`, `GET /v1/models/{id}` | [`models`] |
//! | `POST /v1/embeddings` | [`embeddings`] |
//! | `POST /v1/chat/completions` | [`chat`] (+ [`translate`], [`salvage`]) |
//! | `POST /v1/completions` (incl. FIM) | [`completions`] (+ [`fim`], [`lane`]) |
//!
//! Shared pieces: [`gate`] (auth + the `/v1` rate limit), [`errors`]
//! (OpenAI error bodies), [`sse`] (OpenAI streaming frames), [`backend`]
//! (the pipe calls, behind a trait for tests), [`registry`] (the cached
//! model-registry view every route lists and resolves models against) and
//! [`aliases`] (short model names, from the registry).

pub mod aliases;
pub mod backend;
pub mod chat;
pub mod completions;
pub mod embeddings;
pub mod errors;
pub mod fim;
pub mod gate;
pub mod lane;
pub mod models;
pub mod registry;
pub mod salvage;
pub mod sse;
pub mod translate;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;

use crate::middleware::rate_limit::{openai_limiter, RateLimiter};
use aliases::Aliases;
use backend::Backend;
use lane::FimLane;
use registry::RegistryCache;
use salvage::SalvagePolicy;

/// Handler state shared by every `/v1` route.
#[derive(Clone)]
pub struct OpenAiState {
    pub backend: Arc<dyn Backend>,
    /// Deprecated `WYLDE_OPENAI_MODEL_ALIASES` entries: a fallback under the
    /// registry's aliases (see [`registry`]).
    pub env_aliases: Arc<Aliases>,
    /// The cached registry view (catalog + effective aliases).
    pub registry: Arc<RegistryCache>,
    /// Which models get tool-call salvage.
    pub salvage: Arc<SalvagePolicy>,
    /// Latest-request-wins FIM lane, per device.
    pub lane: Arc<FimLane>,
    /// Per-model "supports native `insert`" answers from `ollama.show`.
    pub insert_cache: Arc<Mutex<HashMap<String, bool>>>,
}

/// The production `/v1` router: live pipes, the deprecated env aliases as a
/// fallback, the process-wide `/v1` limiter.
pub fn router() -> Router {
    router_with(backend::pipe(), Aliases::from_env(), openai_limiter())
}

/// Build the `/v1` router over an explicit backend, fallback (env) alias map
/// and limiter. The salvage setting is read from the environment.
pub fn router_with(backend: Arc<dyn Backend>, aliases: Aliases, limiter: RateLimiter) -> Router {
    router_from(
        OpenAiState::new(backend, aliases, SalvagePolicy::from_env()),
        limiter,
    )
}

impl OpenAiState {
    /// Fresh state: empty registry/`insert` caches and FIM lane.
    /// `env_aliases` are the deprecated fallback aliases.
    pub fn new(backend: Arc<dyn Backend>, env_aliases: Aliases, salvage: SalvagePolicy) -> Self {
        Self {
            backend,
            env_aliases: Arc::new(env_aliases),
            registry: Arc::new(RegistryCache::default()),
            salvage: Arc::new(salvage),
            lane: Arc::new(FimLane::default()),
            insert_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Build the `/v1` router over explicit state and limiter.
pub fn router_from(state: OpenAiState, limiter: RateLimiter) -> Router {
    Router::new()
        .route("/v1/models", get(models::list))
        // `{*id}`: model ids contain `/` (`hf.co/unsloth/Repo:Q4`).
        .route("/v1/models/{*id}", get(models::get))
        .route("/v1/embeddings", post(embeddings::create))
        .route("/v1/chat/completions", post(chat::create))
        .route("/v1/completions", post(completions::create))
        .route_layer(from_fn_with_state(limiter, gate::openai_gate))
        .with_state(state)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_chat;
#[cfg(test)]
mod tests_completions;
#[cfg(test)]
mod tests_registry;
