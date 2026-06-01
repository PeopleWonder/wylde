//! `ollama.*` — active model-callable tools (Phase 8).
//!
//! Four thin wrappers over `wylde-ollama` IPC actions:
//!
//! * `list_loaded_models` → `ollama.list_loaded` — return models
//!   currently resident in VRAM.
//! * `preload_model` → `ollama.preload` — load a model into VRAM
//!   without generating tokens.
//! * `evict_model` → `ollama.eject` — release a specific model from VRAM.
//! * `auto_evict_lru` → `ollama.list_loaded` + `ollama.eject` —
//!   read the resident set, sort by `expires_at`, evict until VRAM
//!   drops under the threshold.
//!
//! The Python predecessors live at `Core/harness/tooling/tools/ollama/
//! <id>/`. Each one shells out to `_ollama_lib` for HTTP/IPC; the Rust
//! port skips the HTTP fallback entirely — Phase 1 made the pipe the
//! canonical transport.

use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, param, param_default, Registry};

/// `VRAM_EVICT_THRESHOLD_MB` matches the Python tool's default.
const DEFAULT_VRAM_EVICT_THRESHOLD_MB: u64 = 20_000;

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "list_loaded_models",
        "ollama.list_loaded_models",
        "ollama",
        "List models currently held in memory by Ollama. Returns each \
         entry's name, size, VRAM footprint, and expires_at timestamp.",
        vec![],
        false,
        |args, _| async move { run_list_loaded(args).await },
    ));

    reg.insert(entry_active(
        "preload_model",
        "ollama.preload_model",
        "ollama",
        "Load a model into VRAM without generating tokens. The model \
         stays resident for `keep_alive` (default 24h).",
        vec![
            param("model", "string", true, "Ollama model tag (e.g. 'qwen2.5:7b')"),
            param_default(
                "keep_alive",
                "string",
                "Resident TTL — Ollama-style duration string or integer seconds",
                json!("24h"),
            ),
        ],
        true,
        |args, _| async move { run_preload(args).await },
    ));

    reg.insert(entry_active(
        "evict_model",
        "ollama.evict_model",
        "ollama",
        "Evict a specific model from VRAM (keep_alive=0).",
        vec![param("model", "string", true, "Ollama model tag to evict")],
        true,
        |args, _| async move { run_evict(args).await },
    ));

    reg.insert(entry_active(
        "auto_evict_lru",
        "ollama.auto_evict_lru",
        "ollama",
        "Sweep loaded models, evict LRU (soonest-to-expire) entries \
         until total VRAM drops below `threshold_mb`.",
        vec![
            param_default(
                "threshold_mb",
                "number",
                "VRAM threshold in MiB",
                json!(DEFAULT_VRAM_EVICT_THRESHOLD_MB),
            ),
            param_default(
                "dry_run",
                "boolean",
                "Compute the eviction plan without actually evicting",
                json!(false),
            ),
        ],
        true,
        |args, _| async move { run_auto_evict_lru(args).await },
    ));
}

// ── Handlers ─────────────────────────────────────────────────────────

async fn run_list_loaded(_args: Value) -> Result<Value, IpcError> {
    let reply = wylde_shared::ipc::send_action(
        "wylde-ollama",
        "ollama.list_loaded",
        json!({}),
    )
    .await;
    if !reply.ok {
        return Ok(unreachable_envelope(&reply));
    }
    let models = reply
        .data
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let out: Vec<Value> = models
        .into_iter()
        .map(|m| {
            json!({
                "name": m.get("name").cloned().unwrap_or(Value::Null),
                "size": m.get("size").cloned().unwrap_or(Value::Null),
                "size_vram": m.get("size_vram").cloned().unwrap_or(Value::Null),
                "expires_at": m.get("expires_at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let count = out.len();
    Ok(json!({
        "status": "success",
        "models": out,
        "count": count,
    }))
}

async fn run_preload(args: Value) -> Result<Value, IpcError> {
    let Some(model) = require_model(&args) else {
        return Ok(json!({"status": "error", "error": "'model' is required"}));
    };
    let keep_alive = args
        .get("keep_alive")
        .cloned()
        .unwrap_or_else(|| Value::String("24h".to_owned()));
    let reply = wylde_shared::ipc::send_action(
        "wylde-ollama",
        "ollama.preload",
        json!({"model": &model, "keep_alive": keep_alive.clone()}),
    )
    .await;
    if !reply.ok {
        return Ok(unreachable_envelope(&reply));
    }
    Ok(json!({
        "status": "success",
        "model": model,
        "keep_alive": keep_alive,
        "loaded": true,
    }))
}

async fn run_evict(args: Value) -> Result<Value, IpcError> {
    let Some(model) = require_model(&args) else {
        return Ok(json!({"status": "error", "error": "'model' is required"}));
    };
    let reply = wylde_shared::ipc::send_action(
        "wylde-ollama",
        "ollama.eject",
        json!({"model": &model}),
    )
    .await;
    if !reply.ok {
        return Ok(unreachable_envelope(&reply));
    }
    Ok(json!({
        "status": "success",
        "model": model,
        "evicted": true,
    }))
}

async fn run_auto_evict_lru(args: Value) -> Result<Value, IpcError> {
    let threshold_mb = args
        .get("threshold_mb")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_VRAM_EVICT_THRESHOLD_MB);
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let reply = wylde_shared::ipc::send_action(
        "wylde-ollama",
        "ollama.list_loaded",
        json!({}),
    )
    .await;
    if !reply.ok {
        return Ok(unreachable_envelope(&reply));
    }
    let Some(models) = reply.data.get("models").and_then(Value::as_array).cloned() else {
        return Ok(json!({
            "status": "success",
            "message": "no models loaded",
            "evicted": [],
            "vram_mb": 0,
        }));
    };
    if models.is_empty() {
        return Ok(json!({
            "status": "success",
            "message": "no models loaded",
            "evicted": [],
            "vram_mb": 0,
        }));
    }

    let mut sortable = models.clone();
    sortable.sort_by(|a, b| {
        let ea = a
            .get("expires_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let eb = b
            .get("expires_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        ea.cmp(eb)
    });

    let mut total_vram_mb: f64 = models
        .iter()
        .map(|m| m.get("size_vram").and_then(Value::as_u64).unwrap_or(0) as f64)
        .sum::<f64>()
        / (1024.0 * 1024.0);

    if total_vram_mb <= threshold_mb as f64 {
        return Ok(json!({
            "status": "success",
            "message": format!(
                "VRAM {:.0} MiB below threshold {} MiB — nothing to evict",
                total_vram_mb, threshold_mb
            ),
            "evicted": [],
            "vram_mb": total_vram_mb.round() as i64,
        }));
    }

    let mut evicted: Vec<Value> = Vec::new();
    for m in sortable {
        if total_vram_mb <= threshold_mb as f64 {
            break;
        }
        let Some(name) = m.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let vram_mb = m.get("size_vram").and_then(Value::as_u64).unwrap_or(0) as f64
            / (1024.0 * 1024.0);
        if dry_run {
            total_vram_mb -= vram_mb;
            evicted.push(json!({
                "model": name,
                "vram_freed_mb": vram_mb.round() as i64,
                "dry_run": true,
            }));
            continue;
        }
        let eject_reply = wylde_shared::ipc::send_action(
            "wylde-ollama",
            "ollama.eject",
            json!({"model": name}),
        )
        .await;
        if eject_reply.ok {
            total_vram_mb -= vram_mb;
            evicted.push(json!({
                "model": name,
                "vram_freed_mb": vram_mb.round() as i64,
            }));
        } else {
            evicted.push(json!({
                "model": name,
                "vram_freed_mb": 0,
                "error": eject_reply
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "unknown error".to_owned()),
            }));
        }
    }

    Ok(json!({
        "status": "success",
        "evicted": evicted,
        "vram_after_mb": total_vram_mb.round() as i64,
        "threshold_mb": threshold_mb,
    }))
}

fn require_model(args: &Value) -> Option<String> {
    args.get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Match Python's "ollama unreachable" envelope so callers (and the
/// salvage parser) see a stable shape when wylde-ollama isn't up.
fn unreachable_envelope(reply: &wylde_shared::ipc::Reply) -> Value {
    let detail = reply
        .error
        .as_ref()
        .map(|e| format!("{}: {}", e.code, e.message))
        .unwrap_or_else(|| "ollama pipe unreachable".to_owned());
    json!({
        "status": "error",
        "error": format!("ollama unreachable: {detail}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tooling::registry::HandlerKind;

    #[tokio::test]
    async fn register_promotes_all_four_tools_to_active() {
        let mut reg = Registry::empty();
        register(&mut reg);
        for id in [
            "list_loaded_models",
            "preload_model",
            "evict_model",
            "auto_evict_lru",
        ] {
            let entry = reg.lookup(id).unwrap_or_else(|| panic!("missing {id}"));
            assert!(matches!(entry.kind, HandlerKind::Active(_)), "{id} not active");
        }
    }

    #[tokio::test]
    async fn dotted_aliases_resolve_correctly() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert_eq!(
            reg.lookup("ollama.list_loaded_models").unwrap().id,
            "list_loaded_models"
        );
        assert_eq!(
            reg.lookup("ollama.preload_model").unwrap().id,
            "preload_model"
        );
        assert_eq!(
            reg.lookup("ollama.evict_model").unwrap().id,
            "evict_model"
        );
        assert_eq!(
            reg.lookup("ollama.auto_evict_lru").unwrap().id,
            "auto_evict_lru"
        );
    }

    #[tokio::test]
    async fn destructive_classification_matches_python() {
        let mut reg = Registry::empty();
        register(&mut reg);
        // list_loaded_models is read-only.
        assert!(!reg.lookup("list_loaded_models").unwrap().destructive);
        // preload/evict/auto-evict mutate VRAM state.
        assert!(reg.lookup("preload_model").unwrap().destructive);
        assert!(reg.lookup("evict_model").unwrap().destructive);
        assert!(reg.lookup("auto_evict_lru").unwrap().destructive);
    }

    #[tokio::test]
    async fn preload_requires_model() {
        // No wylde-ollama running — preload should short-circuit before
        // the IPC call with the "missing model" envelope.
        let r = run_preload(json!({})).await.unwrap();
        assert_eq!(r["status"], "error");
        assert!(r["error"].as_str().unwrap().contains("model"));
    }

    #[tokio::test]
    async fn evict_requires_model() {
        let r = run_evict(json!({})).await.unwrap();
        assert_eq!(r["status"], "error");
        assert!(r["error"].as_str().unwrap().contains("model"));
    }

    #[tokio::test]
    async fn evict_rejects_empty_string() {
        let r = run_evict(json!({"model": "   "})).await.unwrap();
        assert_eq!(r["status"], "error");
    }

    #[tokio::test]
    async fn auto_evict_handles_unreachable_gracefully() {
        // No wylde-ollama running — the list_loaded IPC call returns an
        // error reply, which should turn into our "ollama unreachable"
        // envelope rather than a Rust-side panic.
        let r = run_auto_evict_lru(json!({"threshold_mb": 0, "dry_run": true}))
            .await
            .unwrap();
        // Either "no models loaded" (probe returned empty list) or an
        // error envelope — both are non-panicking and informative.
        assert!(r["status"].as_str().is_some());
    }
}
