//! Wylde Gateway — Rust port of the `Gateway/` Python package.
//!
//! Gateway is Wylde's trust-boundary HTTP service: it terminates outbound
//! internet traffic from sibling services, fronts mobile clients arriving
//! through the WyldeLink VPN, and exposes a parallel named-pipe transport
//! for in-process callers. Auth, rate limiting, audit logging, and the
//! egress allowlist all live here.
//!
//! ## R3 wave 1 + 2a + 2b + 2c + 2d surface
//!
//! Wave 1 shipped the minimum-viable port: the entry-point lifecycle,
//! `/health`, request-trace + audit-log middleware, a device-gate IPC
//! wrapper, and the named-pipe action shell. Wave 2a added the
//! chat-adjacent surface — `POST /api/chat/run_turn`, conversations
//! CRUD, prompts CRUD — all backed by harness-pipe actions. Wave 2b
//! added the memory-adjacent surface — `/api/memory` (long-term /
//! workspace / short-term layers + reflection), `/api/workspaces`
//! (lifecycle + persona + MRU), and the `/api/rag` MCP proxy. Wave 2c
//! added the model-adjacent surface — `/api/models` (list / running /
//! pull / generate / delete) — proxying the local Ollama daemon at
//! `127.0.0.1:11434` — and brought online [`proxy_core::http_call`]
//! (localhost-HTTP transport, sibling to the wave-1 pipe transport)
//! plus [`streaming`] (NDJSON→SSE bridge, consumer is
//! `/api/models/pull`). Wave 2d adds the peripheral surface:
//! `/api/voice` (Flask-style pipe proxy to `wylde-voice`); `/api/devices`
//! (action-style on `wylde-device-gate` via `services::device_gate`);
//! `/api/push` (Flask-style pipe proxy to `wylde-vpn`); `/api/link`
//! (HTTP loopback proxy to `127.0.0.1:8020`, with
//! `device_gate.complete_pairing` for `/pair`); `/api/images`
//! (HTTP loopback proxy to `127.0.0.1:8014`, with on-disk library
//! reads); and `/api/settings` (file-backed Ollama defaults). The
//! rest of the Python Gateway (extensions / tool_registry routes,
//! outbound egress core, MCP, token cache, `auth::require_local`,
//! Ollama-proxy chat streaming) is queued in
//! `docs/r3_gateway_deferred.md` for wave 2a.1 / 2e+. The training
//! row was removed from the queue in wave 2c (Python's
//! `routes/training.py` was deleted in the Phase-9 audit), so the
//! Rust port has no training surface to mirror.
//!
//! ## Module map
//!
//! | Python                                  | Rust                              |
//! |-----------------------------------------|-----------------------------------|
//! | `Gateway/run.py`                        | [`run`]                           |
//! | `Gateway/app.py`                        | [`app`]                           |
//! | `Gateway/settings.py`                   | [`settings`]                      |
//! | `Gateway/envelopes.py`                  | [`envelopes`]                     |
//! | `Gateway/proxy_core.py`                 | [`proxy_core`]                    |
//! | `Gateway/streaming.py`                  | [`streaming`]                     |
//! | `Gateway/pipe.py`                       | [`pipe`]                          |
//! | `Gateway/middleware/{trace,audit}`      | [`middleware`]                    |
//! | `Gateway/services/device_gate.py`       | [`services`]                      |
//! | `Gateway/routes/health.py`              | [`routes::health`]                |
//! | `Gateway/routes/chat.py` (run_turn)     | [`routes::chat`]                  |
//! | `Gateway/routes/rag.py`                 | [`routes::rag`]                   |
//! | `Gateway/routes/models.py`              | [`routes::models`]                |
//! | `Gateway/routes/voice.py`               | [`routes::voice`]                 |
//! | `Gateway/routes/devices.py`             | [`routes::devices`]               |
//! | `Gateway/routes/push.py`                | [`routes::push`]                  |
//! | `Gateway/routes/link.py`                | [`routes::link`]                  |
//! | `Gateway/routes/images.py`              | [`routes::images`]                |
//! | `Gateway/routes/settings.py`            | [`routes::settings`]              |
//! | `Core/harness/pipe/_conversations.py`   | [`routes::conversations`]         |
//! | `Core/harness/pipe/_prompts.py`         | [`routes::prompts`]               |
//! | `Core/harness/pipe/_memory.py`          | [`routes::memory`]                |
//! | `Core/harness/pipe/_rag_workspaces.py`  | [`routes::workspaces`]            |
//!
//! Strangler-fig: the Lifecycle daemon picks Python or this Rust binary via
//! `WYLDE_WYLDE_GATEWAY_IMPL`. Both write to the same manifest and audit
//! logs, both honour the same env-var surface, so live cutover is a flag
//! flip — though wave 1 only serves `/health`, so production cutover
//! waits on wave 2+.

pub mod app;
pub mod auth;
pub mod egress;
pub mod envelopes;
pub mod middleware;
pub mod pipe;
pub mod proxy_core;
pub mod routes;
pub mod run;
pub mod secrets;
pub mod services;
pub mod settings;
pub mod streaming;

pub use crate::settings::{get_settings, reset_settings_cache, GatewaySettings};

/// Canonical service identity, matches `Gateway/run.py::SERVICE_NAME`.
pub const SERVICE_NAME: &str = "wylde-gateway";
