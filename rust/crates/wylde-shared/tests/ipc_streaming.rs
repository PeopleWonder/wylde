//! End-to-end IPC streaming tests: register a streaming action, open a
//! `send_action_stream` from a client, assert chunk ordering, heartbeats,
//! cancellation semantics, mid-stream error surfacing, and that the
//! coexisting unary `send_action` path still works on the same server.
//!
//! Windows-only — every test compiles to a no-op on non-Windows.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use wylde_shared::ipc;

/// Drop the heartbeat from 25s to 1s for the duration of the test process
/// — otherwise the heartbeat / cancellation assertions would all have to
/// wait the full default cadence. Idempotent (env vars are process-global).
fn shorten_heartbeat() {
    std::env::set_var("WYLDE_IPC_STREAM_HEARTBEAT_SECS", "1");
}

async fn boot_server(
    svc: &str,
) -> (
    Arc<ipc::PipeServer>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let server = Arc::new(ipc::PipeServer::new(svc));
    let server_clone = Arc::clone(&server);
    let task = tokio::spawn(async move { server_clone.accept_loop().await });
    // Give the accept loop time to bind the first pipe instance.
    tokio::time::sleep(Duration::from_millis(100)).await;
    (server, task)
}

#[tokio::test]
async fn streams_n_chunks_in_order() {
    shorten_heartbeat();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-order-{suffix}");
    let action = format!("test.stream.order.{suffix}");

    ipc::register_streaming_action(
        &action,
        |_payload: serde_json::Value, sender: ipc::StreamSender| async move {
            for i in 0..5 {
                if sender.send(Ok(serde_json::json!({"i": i}))).await.is_err() {
                    return;
                }
            }
        },
    );

    let (server, task) = boot_server(&service).await;

    let mut stream = ipc::send_action_stream(&service, &action, serde_json::Value::Null);
    let mut got = Vec::new();
    while let Some(item) = stream.next().await {
        got.push(item.expect("chunk should be ok"));
    }

    assert_eq!(got.len(), 5);
    for (i, v) in got.iter().enumerate() {
        assert_eq!(v["i"], i as i64);
    }

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn heartbeat_keeps_slow_stream_alive() {
    shorten_heartbeat();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-hb-{suffix}");
    let action = format!("test.stream.hb.{suffix}");

    // Handler sleeps 2.5s before emitting one chunk — heartbeats at 1s
    // cadence keep the read alive. Without heartbeats the chunk read would
    // still succeed (no client-side idle timeout this short by default),
    // so to make this test actually exercise the heartbeat path we
    // measure: at least 1 heartbeat must have flowed before the chunk
    // lands. We can't observe heartbeats from the client side (they're
    // silently consumed), so we check elapsed time + chunk arrival as a
    // proxy: a) we received the chunk, b) it took > heartbeat cadence to
    // arrive, c) no timeout fired.
    ipc::register_streaming_action(
        &action,
        |_payload: serde_json::Value, sender: ipc::StreamSender| async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            let _ = sender.send(Ok(serde_json::json!({"hi": true}))).await;
        },
    );

    let (server, task) = boot_server(&service).await;

    let start = Instant::now();
    let mut stream = ipc::send_action_stream(&service, &action, serde_json::Value::Null);
    let first = stream
        .next()
        .await
        .expect("expected one chunk")
        .expect("chunk ok");
    let elapsed = start.elapsed();
    assert_eq!(first["hi"], true);
    assert!(
        elapsed >= Duration::from_millis(2000),
        "chunk landed too fast — heartbeat path may not have been exercised: {elapsed:?}"
    );
    // Drain the terminal frame.
    let next = stream.next().await;
    assert!(next.is_none(), "stream should end after the single chunk");

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn drop_cancels_server_handler() {
    shorten_heartbeat();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-cancel-{suffix}");
    let action = format!("test.stream.cancel.{suffix}");

    let started = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let chunks_sent = Arc::new(AtomicUsize::new(0));

    let started_h = Arc::clone(&started);
    let cancelled_h = Arc::clone(&cancelled);
    let sent_h = Arc::clone(&chunks_sent);
    ipc::register_streaming_action(
        &action,
        move |_payload: serde_json::Value, sender: ipc::StreamSender| {
            let started_h = Arc::clone(&started_h);
            let cancelled_h = Arc::clone(&cancelled_h);
            let sent_h = Arc::clone(&sent_h);
            async move {
                started_h.store(true, Ordering::SeqCst);
                // Emit chunks every 100ms; observe sender.closed() in
                // parallel to detect cancellation promptly.
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            if sender.send(Ok(serde_json::json!({"keep": "going"}))).await.is_err() {
                                cancelled_h.store(true, Ordering::SeqCst);
                                return;
                            }
                            sent_h.fetch_add(1, Ordering::SeqCst);
                        }
                        _ = sender.closed() => {
                            cancelled_h.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
        },
    );

    let (server, task) = boot_server(&service).await;

    {
        let mut stream = ipc::send_action_stream(&service, &action, serde_json::Value::Null);
        // Pull a couple of chunks so we know the handler is running.
        let _ = stream.next().await.expect("first chunk").expect("ok");
        let _ = stream.next().await.expect("second chunk").expect("ok");
        assert!(started.load(Ordering::SeqCst));
        // Drop the stream — should signal cancellation back to the server.
    }

    // Wait up to 5s for the handler to observe the cancellation.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cancelled.load(Ordering::SeqCst) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        cancelled.load(Ordering::SeqCst),
        "handler did not observe cancellation within 5s — chunks_sent={}",
        chunks_sent.load(Ordering::SeqCst)
    );

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn unary_send_action_still_works_alongside_streaming() {
    shorten_heartbeat();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-bc-{suffix}");
    let unary = format!("test.bc.unary.{suffix}");
    let streaming = format!("test.bc.stream.{suffix}");

    ipc::register_action(&unary, |payload: serde_json::Value| async move {
        ipc::Reply::ok(payload)
    });
    ipc::register_streaming_action(
        &streaming,
        |_p: serde_json::Value, sender: ipc::StreamSender| async move {
            for i in 0..3 {
                if sender.send(Ok(serde_json::json!({"i": i}))).await.is_err() {
                    return;
                }
            }
        },
    );

    let (server, task) = boot_server(&service).await;

    // Unary path: untouched semantics.
    let reply = ipc::send_action(&service, &unary, serde_json::json!({"x": 42})).await;
    assert!(reply.ok, "unary call failed: {:?}", reply.error);
    assert_eq!(reply.data["x"], 42);

    // Streaming path on the same server.
    let mut stream = ipc::send_action_stream(&service, &streaming, serde_json::Value::Null);
    let mut got = Vec::new();
    while let Some(item) = stream.next().await {
        got.push(item.expect("chunk ok"));
    }
    assert_eq!(got.len(), 3);

    // And unary still works after the streaming round-trip too.
    let reply2 = ipc::send_action(&service, &unary, serde_json::json!({"y": 7})).await;
    assert!(reply2.ok);
    assert_eq!(reply2.data["y"], 7);

    ipc::unregister_action(&unary);
    ipc::unregister_action(&streaming);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn mid_stream_error_is_surfaced_without_hang() {
    shorten_heartbeat();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-stream-err-{suffix}");
    let action = format!("test.stream.err.{suffix}");

    ipc::register_streaming_action(
        &action,
        |_p: serde_json::Value, sender: ipc::StreamSender| async move {
            let _ = sender.send(Ok(serde_json::json!({"i": 0}))).await;
            let _ = sender.send(Ok(serde_json::json!({"i": 1}))).await;
            let _ = sender
                .send(Err(ipc::IpcError::new("kapow", "exploded mid stream")))
                .await;
            // Anything past the error should never reach the client.
            let _ = sender.send(Ok(serde_json::json!({"i": 999}))).await;
        },
    );

    let (server, task) = boot_server(&service).await;

    let mut stream = ipc::send_action_stream(&service, &action, serde_json::Value::Null);

    let chunk0 = stream.next().await.expect("c0").expect("ok");
    assert_eq!(chunk0["i"], 0);
    let chunk1 = stream.next().await.expect("c1").expect("ok");
    assert_eq!(chunk1["i"], 1);

    let err = stream
        .next()
        .await
        .expect("error item")
        .expect_err("should be err");
    assert_eq!(err.code, "kapow");

    let after = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream did not terminate after error");
    assert!(after.is_none(), "stream must end after the error frame");

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}
