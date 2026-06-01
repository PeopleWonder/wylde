//! HTTP route surface.
//!
//! Rust port of `Gateway/routes/`. Wave 1 brought `/health` online; wave
//! 2a added the chat-adjacent surface (`chat.run_turn`, conversations
//! CRUD, prompts CRUD); wave 2b added the memory-adjacent surface
//! (memory CRUD across the three layers, workspaces CRUD + persona,
//! and the `/api/rag` MCP proxy); wave 2c added the model-adjacent
//! surface (`/api/models` — Ollama proxy with streaming `/pull`);
//! wave 2d adds the peripheral surface (`/api/voice`, `/api/devices`,
//! `/api/push`, `/api/link`, `/api/images`, `/api/settings`). The
//! remaining egress / secrets / MCP / tool_registry / extensions
//! routes are queued for wave 2e+ — see `docs/r3_gateway_deferred.md`.
//! The training row was removed from the queue in wave 2c: Python's
//! `routes/training.py` was deleted in the Phase-9 audit (chat-driven
//! trainer flow + direct GUI→pipe), so the Rust port has no training
//! surface to mirror.

mod common;

pub mod chat;
pub mod conversations;
pub mod dev;
pub mod devices;
pub mod egress;
pub mod extensions;
pub mod health;
pub mod images;
pub mod link;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod prompts;
pub mod push;
pub mod rag;
pub mod settings;
pub mod tool_registry;
pub mod voice;
pub mod workspaces;

use axum::Router;

/// Merge every wired route module onto `router`. Mirrors
/// `Gateway/routes/__init__.py::include_all`.
pub fn include_all(router: Router) -> Router {
    router
        .merge(health::router())
        .merge(chat::router())
        .merge(conversations::router())
        .merge(prompts::router())
        .merge(memory::router())
        .merge(workspaces::router())
        .merge(rag::router())
        .merge(models::router())
        .merge(voice::router())
        .merge(devices::router())
        .merge(push::router())
        .merge(link::router())
        .merge(images::router())
        .merge(settings::router())
        .merge(egress::router())
        .merge(extensions::router())
        .merge(dev::router())
        .merge(tool_registry::router())
        .merge(mcp::router())
}
