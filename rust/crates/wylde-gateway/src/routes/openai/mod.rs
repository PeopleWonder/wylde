//! `/v1` — OpenAI-compatible API (#88, #343).
//!
//! One endpoint any OpenAI client (Continue first) can point at, backed by
//! `wylde-ollama` so every inference call is brokered through the VRAM
//! broker, and gated by a Wylde device token like `/mcp`.
//!
//! | Route | Module |
//! |---|---|
//! | `GET /v1/models`, `GET /v1/models/{id}` | [`models`] |
//! | `POST /v1/embeddings` | [`embeddings`] |
//! | `POST /v1/chat/completions`, `POST /v1/completions` | PR 3 (#344) |
//!
//! Shared pieces: [`gate`] (auth + the `/v1` rate limit), [`errors`]
//! (OpenAI error bodies), [`sse`] (OpenAI streaming frames), [`backend`]
//! (the pipe calls, behind a trait for tests) and [`aliases`] (short model
//! names).

pub mod aliases;
pub mod backend;
pub mod embeddings;
pub mod errors;
pub mod gate;
pub mod models;
pub mod sse;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;

use crate::middleware::rate_limit::{openai_limiter, RateLimiter};
use aliases::Aliases;
use backend::Backend;

/// Handler state shared by every `/v1` route.
#[derive(Clone)]
pub struct OpenAiState {
    pub backend: Arc<dyn Backend>,
    pub aliases: Arc<Aliases>,
}

/// The production `/v1` router: live pipes, env aliases, the process-wide
/// `/v1` limiter.
pub fn router() -> Router {
    router_with(backend::pipe(), Aliases::from_env(), openai_limiter())
}

/// Build the `/v1` router over an explicit backend, alias map and limiter.
pub fn router_with(backend: Arc<dyn Backend>, aliases: Aliases, limiter: RateLimiter) -> Router {
    let state = OpenAiState {
        backend,
        aliases: Arc::new(aliases),
    };
    Router::new()
        .route("/v1/models", get(models::list))
        // `{*id}`: model ids contain `/` (`hf.co/unsloth/Repo:Q4`).
        .route("/v1/models/{*id}", get(models::get))
        .route("/v1/embeddings", post(embeddings::create))
        .route_layer(from_fn_with_state(limiter, gate::openai_gate))
        .with_state(state)
}

#[cfg(test)]
mod tests;
