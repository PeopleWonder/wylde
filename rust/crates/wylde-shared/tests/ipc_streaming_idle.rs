//! Streaming idle-timeout test, isolated in its own binary so the
//! env-var mutation (huge heartbeat + tiny idle) cannot race with the
//! other `ipc_streaming.rs` tests. Each `tests/*.rs` file builds its own
//! integration test executable, so these env vars are scoped to this
//! process only.

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use wylde_shared::ipc;

#[tokio::test]
async fn idle_timeout_fires_when_no_heartbeat_and_no_chunks() {
    // Disable heartbeats on the server (set huge cadence) and set a tiny
    // idle-read timeout on the client. The handler never emits anything,
    // so the client's read_timeout should fire.
    std::env::set_var("WYLDE_IPC_STREAM_HEARTBEAT_SECS", "3600");
    std::env::set_var("WYLDE_IPC_IDLE_TIMEOUT", "1");

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-idle-{suffix}");
    let action = format!("test.stream.idle.{suffix}");

    ipc::register_streaming_action(
        &action,
        |_p: serde_json::Value, sender: ipc::StreamSender| async move {
            // Explicitly hold the sender across the sleep — naming it
            // `_sender` would make `async move` drop it before the
            // first poll (unused capture).
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(sender);
        },
    );

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    let mut stream = ipc::send_action_stream(&service, &action, serde_json::Value::Null);
    let item = stream
        .next()
        .await
        .expect("expected a read_timeout error item, got channel close");
    let err = item.expect_err("must be error");
    assert_eq!(err.code, "read_timeout", "got: {err:?}");
    assert!(start.elapsed() < Duration::from_secs(5));

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}
