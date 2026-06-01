//! End-to-end dispatch tests for Phase 5.C.
//!
//! Stand up a mock `wylde-extension-bridge` over a fresh pipe with one
//! registered `ext.tools.call` action; flip the harness config to point
//! at it; verify [`wylde_harness::dispatch::call_mcp_extension`] sends
//! the Phase 4 contract payload (`{extension, tool, arguments}`) and
//! that the round-trip surfaces the mock's reply verbatim.
//!
//! Windows-only — IPC uses named pipes. Non-Windows builds compile but
//! every test is a no-op.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use wylde_harness::config::Config;
use wylde_harness::dispatch;
use wylde_shared::ipc;

/// Serialise tests that register the same global action name
/// (`ext.tools.call`). The IPC action registry is process-wide; without
/// this guard two `tokio::test`s register and collide.
async fn registry_guard() -> MutexGuard<'static, ()> {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    LOCK.lock().await
}

#[tokio::test]
async fn call_mcp_extension_sends_phase4_payload_and_returns_reply() {
    let _guard = registry_guard().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ext-bridge-mock-{suffix}");

    // Capture what the mock saw so we can assert on the wire shape.
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen_for_handler = Arc::clone(&seen);
    ipc::register_action("ext.tools.call", move |payload: Value| {
        let seen = Arc::clone(&seen_for_handler);
        async move {
            *seen.lock().unwrap() = Some(payload.clone());
            ipc::Reply::ok(json!({"ok": true, "data": {"scraped": "hello world"}}))
        }
    });

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut cfg = Config::default_for_tests();
    cfg.extension_bridge_service = service.clone();
    cfg.mcp_namespaces = vec!["webcrawler".to_string()];

    let result = dispatch::call_mcp_extension(&cfg, "webcrawler.scrape", json!({"url": "x"}))
        .await
        .expect("MCP call succeeds");
    assert_eq!(result["ok"], true);
    assert_eq!(result["data"]["scraped"], "hello world");

    // Wire-shape parity with the Phase 4 contract.
    let captured = seen.lock().unwrap().clone().expect("handler called");
    assert_eq!(captured["extension"], "webcrawler");
    assert_eq!(captured["tool"], "scrape");
    assert_eq!(captured["arguments"]["url"], "x");

    ipc::unregister_action("ext.tools.call");
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn call_mcp_extension_surfaces_bridge_error_envelope() {
    let _guard = registry_guard().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ext-bridge-mock-err-{suffix}");

    ipc::register_action("ext.tools.call", |_payload: Value| async move {
        ipc::Reply::err(ipc::IpcError::new(
            "extension_not_found",
            "unknown extension",
        ))
    });

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut cfg = Config::default_for_tests();
    cfg.extension_bridge_service = service.clone();
    cfg.mcp_namespaces = vec!["webcrawler".to_string()];

    let err = dispatch::call_mcp_extension(&cfg, "webcrawler.scrape", json!({}))
        .await
        .expect_err("should error");
    assert_eq!(err.code, "extension_not_found");

    ipc::unregister_action("ext.tools.call");
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}
