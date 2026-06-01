//! End-to-end IPC integration test: spawn a server, connect a client,
//! exchange one action round-trip, assert the Reply parses cleanly.
//!
//! Windows-only. Non-Windows builds compile this file but every test is
//! a no-op.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use wylde_shared::ipc;

#[tokio::test]
async fn full_action_roundtrip_over_pipe() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-int-{suffix}");
    let action = format!("test.echo.{suffix}");

    // Register an echo action.
    let action_for_handler = action.clone();
    ipc::register_action(&action, move |payload: serde_json::Value| {
        let _ = &action_for_handler;
        async move { ipc::Reply::ok(payload) }
    });

    // Bring up the server.
    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });

    // Give the accept loop a moment to bind the first pipe instance.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Fire one client request.
    let reply = ipc::send_action(
        &service,
        &action,
        serde_json::json!({"hello": "world", "n": 7}),
    )
    .await;

    assert!(
        reply.ok,
        "expected ok reply, got: {:?} (error={:?})",
        reply.data, reply.error
    );
    assert_eq!(reply.data["hello"], "world");
    assert_eq!(reply.data["n"], 7);

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn ping_method_works_in_band() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-int-ping-{suffix}");

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let reply = ipc::send(
        &service,
        "/__ping__",
        serde_json::Value::Null,
        Duration::from_secs(5),
    )
    .await;

    assert!(
        reply.ok,
        "ping should succeed, got error: {:?}",
        reply.error
    );
    assert_eq!(reply.data["pong"], true);
    assert_eq!(reply.data["ver"], 1);

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn unknown_action_returns_no_action_error() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-int-noact-{suffix}");

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let reply = ipc::send_action(
        &service,
        "definitely.does.not.exist",
        serde_json::Value::Null,
    )
    .await;

    assert!(!reply.ok);
    let err = reply.error.expect("error body");
    assert_eq!(err.code, "no_action");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn multiple_concurrent_clients_handled() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ipc-int-conc-{suffix}");
    let action = format!("test.conc.{suffix}");

    ipc::register_action(&action, |payload: serde_json::Value| async move {
        ipc::Reply::ok(payload)
    });

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 4 concurrent clients each fire one request.
    let mut handles = Vec::new();
    for i in 0..4 {
        let svc = service.clone();
        let act = action.clone();
        handles.push(tokio::spawn(async move {
            ipc::send_action(&svc, &act, serde_json::json!({"i": i})).await
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let reply = h.await.expect("join");
        assert!(reply.ok, "client {i} got error: {:?}", reply.error);
        assert_eq!(reply.data["i"], i as i64);
    }

    ipc::unregister_action(&action);
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}
