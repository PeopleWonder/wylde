//! Service entrypoint: register the `caption.*` actions on the shared
//! IPC registry. Same shape as `wylde-ollama::service`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use wylde_shared::ipc::{register_action_with_meta, unregister_action};

use crate::actions::{caption, health};

const ALL_ACTIONS: [&str; 5] = [
    "caption.health",
    "caption.list_backends",
    "caption.generate",
    "caption.generate_batch",
    "caption.generate_video",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every `caption.*` action on the process-wide registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    register_action_with_meta(
        "caption.health",
        |payload: Value| async move { health::handle_health(payload).await },
        "Liveness probe + worker state. Reply: \
         {ok: true, backend, model_loaded, device, worker_pid, worker_warm}.",
        "wylde_trainer::actions::health",
    );
    register_action_with_meta(
        "caption.list_backends",
        |payload: Value| async move { health::handle_list_backends(payload).await },
        "List captioner backends compiled into the worker (florence, qwen, joycaption) \
         and the active default. Static reply — does not boot the worker.",
        "wylde_trainer::actions::health",
    );
    register_action_with_meta(
        "caption.generate",
        |payload: Value| async move { caption::handle_generate(payload).await },
        "Caption a single image. Payload: {image_path, detail?, trigger?, backend?, \
         write_txt?, overwrite?}. Reply: {image_path, caption, backend, txt_path?, written?}. \
         Mirrors Trainer/Caption/tools/caption_image.",
        "wylde_trainer::actions::caption",
    );
    register_action_with_meta(
        "caption.generate_batch",
        |payload: Value| async move { caption::handle_generate_batch(payload).await },
        "Caption every image in a folder, writing a .txt next to each. Payload: \
         {folder, detail?, trigger?, backend?, extensions?, recursive?, batch_size?, \
         output_ext?, overwrite?}. Reply: {folder, total, captioned, skipped, errors, \
         errors_list, files_sample}. Mirrors Trainer/Caption/tools/caption_batch.",
        "wylde_trainer::actions::caption",
    );
    register_action_with_meta(
        "caption.generate_video",
        |payload: Value| async move { caption::handle_generate_video(payload).await },
        "Sample frames from a video and caption them. Payload: {video_path, detail?, \
         trigger?, backend?, mode?, frame_count?, target_fps?, interval_s?, aggregate?, \
         write_txt?, overwrite?, write_frames?}. Reply: {video_path, frames_sampled, \
         caption, per_frame_captions, frames_written?}. Mirrors \
         Trainer/Caption/tools/caption_video.",
        "wylde_trainer::actions::caption",
    );

    tracing::info!("wylde-trainer: registered {} actions", ALL_ACTIONS.len());
}

/// Signal stop. The Python worker is supervised by the lifecycle
/// daemon, not by this crate — so nothing process-local to drain.
pub fn stop() {}

/// Test-only: unregister every action and reset the install flag.
pub fn reset_for_tests() {
    for n in ALL_ACTIONS {
        unregister_action(n);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

pub fn all_actions() -> &'static [&'static str] {
    &ALL_ACTIONS
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
    use wylde_shared::ipc::{dispatch_action, list_actions};

    async fn registry_guard() -> MutexGuard<'static, ()> {
        static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn install_registers_all_five_actions() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let actions = list_actions();
        for n in ALL_ACTIONS {
            assert!(actions.contains(&n.to_string()), "missing {n}");
        }
        reset_for_tests();
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        install();
        reset_for_tests();
    }

    #[tokio::test]
    async fn dispatching_unknown_subaction_returns_no_action() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "caption.bogus",
            "payload": null,
        }))
        .await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "no_action");
        reset_for_tests();
    }

    #[tokio::test]
    async fn list_backends_returns_via_dispatch() {
        let _g = registry_guard().await;
        reset_for_tests();
        install();
        let reply = dispatch_action(serde_json::json!({
            "action": "caption.list_backends",
            "payload": null,
        }))
        .await;
        assert!(reply.ok, "list_backends should be a static reply");
        let backends = reply.data["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 3);
        reset_for_tests();
    }
}
