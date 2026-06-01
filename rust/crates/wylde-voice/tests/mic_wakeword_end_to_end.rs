//! End-to-end + live-mic integration tests (Slice 11.D).
//!
//! Three layers:
//!
//! 1. `voice.mic.stop` dispatches cleanly through the registry when no
//!    capture is running (cheap, always-on).
//! 2. `voice.mic.chunks` surfaces `invalid_request` when no capture
//!    is active (cheap, always-on).
//! 3. `live_mic_chunk_round_trip` (`#[ignore]`) opens the default
//!    input device, drives the `voice.mic.start` → `voice.mic.chunks`
//!    → `voice.mic.stop` pipeline, and asserts the chunk wire shape.
//!
//! Run the ignored test locally with:
//!
//! ```
//! cargo test -p wylde-voice --test mic_wakeword_end_to_end \
//!     live_mic_chunk_round_trip -- --ignored --nocapture
//! ```

use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use wylde_shared::ipc::actions::take_streaming_action;
use wylde_shared::ipc::dispatch_action;

/// Tokio test threads can race on the process-wide action registry —
/// the same pattern wylde-voice's unit tests use to serialise
/// install() / reset_for_tests() against dispatch_action() calls.
async fn registry_guard() -> MutexGuard<'static, ()> {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    LOCK.lock().await
}

#[tokio::test]
async fn mic_stop_dispatches_via_registry_when_idle() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();
    let reply = dispatch_action(json!({
        "action": "voice.mic.stop",
        "payload": null,
    }))
    .await;
    wylde_voice::reset_for_tests();
    assert!(reply.ok, "voice.mic.stop should reply ok even when idle");
    assert_eq!(reply.data["stopped"], false);
    assert_eq!(reply.data["was_running"], false);
}

#[tokio::test]
async fn wakeword_stop_dispatches_via_registry_when_idle() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();
    let reply = dispatch_action(json!({
        "action": "voice.wakeword.stop",
        "payload": null,
    }))
    .await;
    wylde_voice::reset_for_tests();
    assert!(reply.ok);
    assert_eq!(reply.data["stopped"], false);
}

#[tokio::test]
async fn mic_chunks_surfaces_invalid_request_with_no_capture() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let fut = take_streaming_action("voice.mic.chunks", json!({}), tx)
        .expect("streaming action resolves");
    fut.await;
    let chunk = rx.recv().await.expect("at least one chunk");
    let err = chunk.expect_err("missing capture → stream-level error");
    assert_eq!(err.code, "invalid_request");
    assert!(err.message.contains("voice.mic.start"));
    wylde_voice::reset_for_tests();
}

#[tokio::test]
async fn wakeword_events_surfaces_invalid_request_with_no_listener() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let fut = take_streaming_action("voice.wakeword.events", json!({}), tx)
        .expect("streaming action resolves");
    fut.await;
    let chunk = rx.recv().await.expect("at least one chunk");
    let err = chunk.expect_err("missing listener → stream-level error");
    assert_eq!(err.code, "invalid_request");
    assert!(err.message.contains("voice.wakeword.start"));
    wylde_voice::reset_for_tests();
}

#[tokio::test]
async fn wakeword_start_with_unresolvable_models_dir_returns_model_not_loaded() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();
    let reply = dispatch_action(json!({
        "action": "voice.wakeword.start",
        "payload": {
            "model_name": "openwakeword-fake-integration-fixture",
            "models_dir": "C:/no/such/dir/openwakeword-integration-fixture",
        },
    }))
    .await;
    wylde_voice::reset_for_tests();
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "model_not_loaded");
}

/// Live wake-word integration: pull the openWakeWord bundle from the
/// canonical GitHub URLs, scan via the model registry to confirm it
/// landed, then check_wake_word_model + voice.wakeword.start drives a
/// real listener thread. Marked `#[ignore]` because:
///   1. It hits the network (curl GET to raw.githubusercontent.com).
///   2. It needs a working default input device for the listener.
///
/// Run locally with:
/// ```
/// cargo test -p wylde-voice --test mic_wakeword_end_to_end \
///     live_wake_word_pull_and_load -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires network + default input device"]
async fn live_wake_word_pull_and_load() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();

    let td = tempfile::TempDir::new().expect("tempdir");
    std::env::set_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR", td.path());

    // 1. Kick a pull and wait for completion.
    let pull = dispatch_action(json!({
        "action": "voice.pull_wake_word_model",
        "payload": {"model": "openWakeWord/hey-jarvis"},
    }))
    .await;
    assert!(pull.ok, "pull failed: {:?}", pull.error);
    let job_id = pull.data["job_id"].as_str().unwrap().to_owned();

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut last_state = String::new();
    while std::time::Instant::now() < deadline {
        let st = dispatch_action(json!({
            "action": "voice.wake_word_pull_status",
            "payload": {"job_id": job_id},
        }))
        .await;
        assert!(st.ok, "status query failed: {:?}", st.error);
        last_state = st.data["state"].as_str().unwrap_or_default().to_owned();
        if last_state == "done" {
            break;
        }
        if last_state == "failed" {
            panic!("pull failed: {:?}", st.data["error"]);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(last_state, "done", "pull did not complete in 120 s");

    // 2. Bundle should now be visible via check_wake_word_model.
    let check = dispatch_action(json!({
        "action": "voice.check_wake_word_model",
        "payload": {"model": "openWakeWord/hey-jarvis"},
    }))
    .await;
    assert!(check.ok);
    assert_eq!(
        check.data["installed"], true,
        "scanner didn't see the freshly pulled bundle"
    );

    // 3. Start the listener. The pipeline load can fail on a host
    //    without ort runtime DLLs — surface that explicitly so the test
    //    failure tells the operator what's missing.
    let start = dispatch_action(json!({
        "action": "voice.wakeword.start",
        "payload": {
            "model_name": "openWakeWord/hey-jarvis",
            "models_dir": td.path(),
        },
    }))
    .await;
    if !start.ok {
        let err = start.error.unwrap();
        panic!(
            "wakeword.start failed: [{}] {} \
             — check ORT_DYLIB_PATH and that onnxruntime.dll is in place",
            err.code, err.message
        );
    }

    // 4. Stop cleanly.
    let stop = dispatch_action(json!({
        "action": "voice.wakeword.stop",
        "payload": null,
    }))
    .await;
    wylde_voice::reset_for_tests();
    std::env::remove_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR");
    assert!(stop.ok);
    assert_eq!(stop.data["stopped"], true);
}

/// Drive the full mic.start → mic.chunks → mic.stop loop against the
/// real default input device. Marked `#[ignore]` because the test
/// requires a working mic.
#[tokio::test]
#[ignore = "requires a working default input device"]
async fn live_mic_chunk_round_trip() {
    let _g = registry_guard().await;
    wylde_voice::reset_for_tests();
    wylde_voice::service::install();

    let start = dispatch_action(json!({
        "action": "voice.mic.start",
        "payload": { "chunk_samples": 800 },
    }))
    .await;
    assert!(start.ok, "voice.mic.start failed: {:?}", start.error);
    assert_eq!(start.data["already_running"], false);
    assert_eq!(start.data["chunk_samples"], 800);
    assert_eq!(start.data["sample_rate"], 16_000);

    // Subscribe to chunks. Read a handful of frames and confirm their
    // shape; cap the wait so a stuck driver doesn't deadlock CI.
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let fut = take_streaming_action("voice.mic.chunks", json!({}), tx)
        .expect("streaming action resolves");
    let handle = tokio::spawn(fut);

    let mut received: Vec<serde_json::Value> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && received.len() < 3 {
        match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Ok(v))) => received.push(v),
            Ok(Some(Err(e))) => panic!("stream error: {e:?}"),
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    // Stop the capture — this should also drain the streaming task.
    let stop = dispatch_action(json!({
        "action": "voice.mic.stop",
        "payload": null,
    }))
    .await;
    handle.abort();
    wylde_voice::reset_for_tests();

    assert!(stop.ok, "voice.mic.stop failed: {:?}", stop.error);
    assert_eq!(stop.data["stopped"], true);

    assert!(
        !received.is_empty(),
        "expected ≥1 chunk in 3 s, got {}",
        received.len()
    );
    let first = &received[0];
    assert_eq!(first["type"], "chunk");
    assert_eq!(first["sample_rate"], 16_000);
    assert_eq!(first["format"], "pcm_s16le");
    let b64 = first["audio_b64"].as_str().expect("audio_b64 is a string");
    assert!(!b64.is_empty());
}
