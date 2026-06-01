//! `caption.health` and `caption.list_backends` — quick probes.
//!
//! `caption.health` forwards to the worker (or short-circuits with a
//! local error if the worker pipe is down — the lifecycle daemon should
//! have started it before us, but a stale-trainer scenario can still
//! race). `caption.list_backends` is a pure static reply — it does NOT
//! reach the worker, so it stays cheap even if the worker is cold.

use serde_json::json;
use serde_json::Value;
use std::time::Duration;
use wylde_shared::ipc::Reply;

use crate::actions::error::worker_unreachable;
use crate::config::Config;
use crate::worker_client::call_worker;

pub async fn handle_health(_payload: Value) -> Reply {
    let cfg = Config::get();
    let to = Duration::from_secs(cfg.health_timeout_s);

    let reply = match tokio::time::timeout(to, call_worker("caption.health", json!({}))).await {
        Ok(r) => r,
        Err(_) => {
            return Reply::err(worker_unreachable(format!(
                "caption.health: worker pipe call timed out after {}s",
                cfg.health_timeout_s
            )));
        }
    };

    if !reply.ok {
        // Pipe-level error (worker not bound, transport error, etc.) —
        // surface as worker_unreachable so dashboards distinguish from
        // a worker-internal failure.
        let detail = reply
            .error
            .as_ref()
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| "no error envelope".to_owned());
        return Reply::err(worker_unreachable(format!(
            "caption.health: worker pipe call failed ({detail})"
        )));
    }

    // The worker tool returns a result dict; surface as-is plus a
    // worker_pipe annotation so callers can see this hit the worker.
    let mut data = reply.data;
    if let Some(map) = data.as_object_mut() {
        map.insert(
            "worker_pipe".to_owned(),
            json!(crate::worker_client::WORKER_SERVICE),
        );
    }
    Reply::ok(data)
}

pub async fn handle_list_backends(_payload: Value) -> Reply {
    // Static set defined in `Trainer/Caption/captioner.py::build_captioner`.
    // Cheap reply that does NOT touch the worker, so dashboards probing
    // the trainer at startup don't pay for the worker pipe boot.
    Reply::ok(json!({
        "backends": ["florence", "qwen", "joycaption"],
        "default": Config::get().backend,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_backends_returns_the_canonical_three() {
        let r = handle_list_backends(json!({})).await;
        assert!(r.ok);
        let backends = r.data["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 3);
        assert!(backends.iter().any(|b| b == "florence"));
        assert!(backends.iter().any(|b| b == "qwen"));
        assert!(backends.iter().any(|b| b == "joycaption"));
    }
}
