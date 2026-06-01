//! `caption.generate` / `caption.generate_batch` / `caption.generate_video`
//! — Florence-2 captioning over the sibling Python worker pipe.
//!
//! Each handler forwards the payload to the worker action of the same
//! name and applies [`flatten_tool_error`] so the in-`data` `{"error":
//! "..."}` envelope returned by the Python tools surfaces as a proper
//! `worker_failed` IPC error.

use std::time::Duration;

use serde_json::Value;
use wylde_shared::ipc::Reply;

use crate::actions::error::worker_unreachable;
use crate::config::Config;
use crate::worker_client::{call_worker, flatten_tool_error};

async fn forward(action: &str, payload: Value, timeout: Duration) -> Reply {
    let reply = match tokio::time::timeout(timeout, call_worker(action, payload)).await {
        Ok(r) => r,
        Err(_) => {
            return Reply::err(worker_unreachable(format!(
                "{action}: worker pipe call timed out after {}s",
                timeout.as_secs()
            )));
        }
    };
    if !reply.ok {
        // Pipe-level error (e.g. worker pipe not bound). The error
        // envelope already carries the upstream code/message — pass it
        // through unchanged so dashboards can see the raw failure mode.
        return reply;
    }
    flatten_tool_error(reply)
}

pub async fn handle_generate(payload: Value) -> Reply {
    let cfg = Config::get();
    forward(
        "caption.generate",
        payload,
        Duration::from_secs(cfg.generate_timeout_s),
    )
    .await
}

pub async fn handle_generate_batch(payload: Value) -> Reply {
    let cfg = Config::get();
    forward(
        "caption.generate_batch",
        payload,
        Duration::from_secs(cfg.batch_timeout_s),
    )
    .await
}

pub async fn handle_generate_video(payload: Value) -> Reply {
    let cfg = Config::get();
    forward(
        "caption.generate_video",
        payload,
        Duration::from_secs(cfg.video_timeout_s),
    )
    .await
}
