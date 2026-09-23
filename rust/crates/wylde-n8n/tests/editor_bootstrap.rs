//! Integration: `n8n.editor_bootstrap` returns the managed shared-auth
//! payload when an editor context is installed.
//!
//! Lives in its own test binary so the process-wide editor-context
//! `OnceLock` starts empty and this test owns it (the in-crate unit test
//! exercises the *unmanaged* path in a separate binary).

use serde_json::json;
use wylde_n8n::editor::{self, EditorContext};
use wylde_n8n::secret::N8nIdentity;
use wylde_shared::ipc::dispatch_action;

/// A per-run random test secret, so no credential is hard-coded in the test.
fn test_secret() -> String {
    wylde_shared::rng::byte_array::<8>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn editor_bootstrap_returns_managed_payload_with_login_script() {
    // Install a managed context with a per-run random identity.
    let key = test_secret();
    let pw = test_secret();
    let identity: N8nIdentity = serde_json::from_value(json!({
        "encryption_key": key,
        "owner_email": "wylde-owner@wylde.local",
        "owner_password": pw,
    }))
    .unwrap();
    editor::set_context(EditorContext {
        base_url: "http://127.0.0.1:5678".into(),
        identity,
    });

    // Register the action surface and dispatch through the real registry.
    wylde_n8n::service::install();
    let reply = dispatch_action(json!({
        "action": "n8n.editor_bootstrap",
        "payload": {},
    }))
    .await;

    assert!(reply.ok, "editor_bootstrap must succeed");
    let data = reply.data;
    assert_eq!(data["managed"], true, "managed mode payload");
    assert_eq!(data["url"], "http://127.0.0.1:5678");
    let js = data["init_js"]
        .as_str()
        .expect("init_js present in managed mode");
    // The script carries the Wylde-owned credentials + browser-id pin so
    // the embedded editor authenticates with no login screen.
    assert!(js.contains("wylde-owner@wylde.local"));
    assert!(js.contains(&pw));
    assert!(js.contains("n8n-browserId"));
    assert!(js.contains("/rest/login"));
}
