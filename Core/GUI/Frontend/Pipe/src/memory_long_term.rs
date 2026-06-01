//! `memory.long_term.*` in-process dispatch.  Mirror of
//! `Core/GUI/src-tauri/src/pipe/memory_long_term.rs`.
//!
//! `memory.long_term.search` is intentionally absent — it goes over the
//! wire so the embedder strangler-fig fallback can catch it; see the
//! 2026-05-28 cleanup-slice memory for context.

use serde_json::Value;
use wylde_harness::HarnessApi;
use wylde_shared::ipc::Reply;

pub async fn dispatch<A: HarnessApi + ?Sized>(
    api: &A,
    verb: &str,
    payload: Value,
) -> Option<Reply> {
    match verb {
        "memory.long_term.list" => Some(api.memory_long_term_list(payload).await),
        "memory.long_term.save" => Some(api.memory_long_term_save(payload).await),
        "memory.long_term.update" => Some(api.memory_long_term_update(payload).await),
        "memory.long_term.delete" => Some(api.memory_long_term_delete(payload).await),
        "memory.long_term.history" => Some(api.memory_long_term_history(payload).await),
        _ => None,
    }
}
