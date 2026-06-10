//! End-to-end test for the extension resource overlay (tool-registry
//! consolidation Slice 5a, `docs/plans/extension-resource-declaration.md`).
//!
//! Stands up a mock `wylde-extension-bridge` exposing `ext.resources.list`
//! (the Webcrawler `url` resource) and `ext.tools.call`. Pulls the
//! declaration into the harness verb overlay, then drives
//! `wylde_execute("ext:Webcrawler:url", action="fetch", params={url})`
//! through the registered `OpHandler` and asserts the call reaches the
//! mock with the Phase-4 `ext.tools.call` shape and the reply flows back.
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use wylde_harness::config::Config;
use wylde_harness::tooling::resource::resources::extensions;
use wylde_harness::tooling::resource::{
    ResourceOp, ResourceRegistry, ResourceRequest, ToolContext,
};
use wylde_shared::ipc;

async fn registry_guard() -> MutexGuard<'static, ()> {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    LOCK.lock().await
}

/// The wire shape the real bridge emits for the Webcrawler `url` resource.
fn webcrawler_resources_reply() -> Value {
    json!({
        "resources": [{
            "extension": "Webcrawler",
            "resource_type": "ext:Webcrawler:url",
            "bare_resource_type": "url",
            "display_name": "Web URL",
            "description": "fetch/scrape/extract",
            "scope": "global",
            "schema_version": 1,
            "claimed_tools": ["fetch", "scrape", "extract"],
            "operations": {
                "execute": {
                    "description": "web actions",
                    "destructive": false,
                    "tier": "read",
                    "mcp_tool": "",
                    "actions": [
                        {"name": "fetch", "mcp_tool": "fetch", "destructive": false},
                        {"name": "scrape", "mcp_tool": "scrape", "destructive": false},
                        {"name": "extract", "mcp_tool": "extract", "destructive": false}
                    ]
                }
            }
        }]
    })
}

#[tokio::test]
async fn wylde_execute_reaches_webcrawler_through_the_overlay() {
    let _guard = registry_guard().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ext-bridge-mock-res-{suffix}");

    // Mock bridge: ext.resources.list returns the declaration; ext.tools.call
    // captures the payload and returns a fake fetch result.
    ipc::register_action("ext.resources.list", |_payload: Value| async move {
        ipc::Reply::ok(webcrawler_resources_reply())
    });

    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen_for_handler = Arc::clone(&seen);
    ipc::register_action("ext.tools.call", move |payload: Value| {
        let seen = Arc::clone(&seen_for_handler);
        async move {
            *seen.lock().unwrap() = Some(payload.clone());
            ipc::Reply::ok(json!({"status": "ok", "body": "<html>hi</html>"}))
        }
    });

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let server_task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut cfg = Config::default_for_tests();
    cfg.extension_bridge_service = service.clone();
    let cfg: &'static Config = Box::leak(Box::new(cfg));

    // Pull declarations + register into a fresh overlay (the same calls the
    // live sync task makes on an `ext.events` spawn).
    let specs = extensions::pull_specs(cfg, None)
        .await
        .expect("ext.resources.list succeeds");
    assert_eq!(specs.len(), 1, "one resource declared");

    let reg = ResourceRegistry::empty();
    let registered = extensions::register_from_specs(&reg, &specs);
    assert_eq!(registered, vec!["ext:Webcrawler:url".to_string()]);

    // Resolve the execute handler and drive a fetch through it.
    let def = reg
        .lookup("ext:Webcrawler:url")
        .expect("resource registered");
    let handler = def
        .operations
        .get(&ResourceOp::Execute)
        .cloned()
        .expect("execute op registered");

    let req = ResourceRequest {
        action: Some("fetch".into()),
        params: json!({"url": "https://example.com", "format": "text"}),
        ..Default::default()
    };
    let ctx = ToolContext::for_op("ext:Webcrawler:url", ResourceOp::Execute, None);
    let result = handler
        .call(req, cfg, ctx)
        .await
        .expect("dispatch succeeds");

    // The mock's reply flows back verbatim.
    assert_eq!(result["status"], "ok");
    assert_eq!(result["body"], "<html>hi</html>");

    // The OpHandler issued the Phase-4 ext.tools.call contract: the `fetch`
    // action mapped to the `fetch` MCP tool, params forwarded as arguments.
    let captured = seen
        .lock()
        .unwrap()
        .clone()
        .expect("ext.tools.call invoked");
    assert_eq!(captured["extension"], "Webcrawler");
    assert_eq!(captured["tool"], "fetch");
    assert_eq!(captured["arguments"]["url"], "https://example.com");
    assert_eq!(captured["arguments"]["format"], "text");

    ipc::unregister_action("ext.resources.list");
    ipc::unregister_action("ext.tools.call");
    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn unknown_action_returns_clean_envelope_without_calling_bridge() {
    let _guard = registry_guard().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("ext-bridge-mock-res-unknown-{suffix}");

    // ext.tools.call must NOT be hit for an unknown action.
    let called: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let called_h = Arc::clone(&called);
    ipc::register_action("ext.tools.call", move |_payload: Value| {
        let called = Arc::clone(&called_h);
        async move {
            *called.lock().unwrap() = true;
            ipc::Reply::ok(json!({}))
        }
    });

    let mut cfg = Config::default_for_tests();
    cfg.extension_bridge_service = service.clone();
    let cfg: &'static Config = Box::leak(Box::new(cfg));

    // Build the overlay directly from a hand-made spec (no bridge pull needed).
    let specs: Vec<extensions::ExtResourceSpec> =
        serde_json::from_value(webcrawler_resources_reply()["resources"].clone()).unwrap();
    let reg = ResourceRegistry::empty();
    extensions::register_from_specs(&reg, &specs);

    let def = reg.lookup("ext:Webcrawler:url").unwrap();
    let handler = def.operations.get(&ResourceOp::Execute).cloned().unwrap();
    let req = ResourceRequest {
        action: Some("teleport".into()),
        params: json!({}),
        ..Default::default()
    };
    let ctx = ToolContext::for_op("ext:Webcrawler:url", ResourceOp::Execute, None);
    let out = handler.call(req, cfg, ctx).await.unwrap();

    assert_eq!(out["status"], "error");
    let known: Vec<&str> = out["known_actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(known.contains(&"fetch"));
    assert!(
        !*called.lock().unwrap(),
        "bridge must not be called for an unknown action"
    );

    ipc::unregister_action("ext.tools.call");
}
