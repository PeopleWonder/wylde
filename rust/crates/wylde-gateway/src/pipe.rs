//! Named-pipe transport — second front door, same operations.
//!
//! Rust port of `Gateway/pipe.py`. The pipe server lives in
//! `wylde_shared::ipc::PipeServer`; here we just register the action
//! handlers each component's HTTP routes already dispatch through, so
//! HTTP and pipe behaviour stay byte-equivalent.
//!
//! ## Wave-2e action surface
//!
//! Wave 1 registered `gateway.ping` plus six `not_implemented` stubs.
//! Wave 2e replaces the egress + extensions + tools stubs with live
//! handlers — see the table below.
//!
//! | Action                 | Handler                                  | Notes                                                                  |
//! |------------------------|------------------------------------------|------------------------------------------------------------------------|
//! | `gateway.ping`         | [`handle_ping`]                          | Diagnostic — was live in wave 1.                                       |
//! | `egress.kill_switch`   | [`handle_kill_switch`]                   | Wave 2e: live. Toggles + reads the egress kill flag.                   |
//! | `egress.destinations`  | [`handle_destinations`]                  | Wave 2e: live. Per-component allowlist + kill state.                   |
//! | `egress.forward`       | [`handle_forward`]                       | Wave 2e: live. Unary outbound call through the allowlist.              |
//! | `extensions.dispatch`  | [`handle_extensions_dispatch`]           | Live. Dispatches through the `wylde-extension-bridge` pipe.            |
//! | `tools.list`           | [`handle_tools_list`]                    | Wave 2e: live. Reshapes harness `tools.list` to alias-keyed dict.      |
//! | `tools.get`            | [`handle_tools_get`]                     | Wave 2e: live. Defers to harness `tools.get`, falls back to list scan. |

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use wylde_shared::ipc::{register_action_with_meta, unregister_action, IpcError, Reply};

use crate::egress::{self, client::EgressError};

pub const SERVICE_NAME: &str = "wylde-gateway";

/// Action names registered by wave 2e. Listed here so [`uninstall`]
/// (test-only) can drop them by name without re-typing the list.
const ACTIONS: [&str; 7] = [
    "gateway.ping",
    "egress.kill_switch",
    "egress.destinations",
    "egress.forward",
    "extensions.dispatch",
    "tools.list",
    "tools.get",
];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register every wave-2e action on the process-wide pipe registry.
/// Idempotent — repeat calls are no-ops.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    register_action_with_meta(
        "gateway.ping",
        |p: Value| async move { handle_ping(p).await },
        "Diagnostic ping. Returns {ok: true, pong: <echo>?}",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "egress.kill_switch",
        |p: Value| async move { handle_kill_switch(p).await },
        "Toggle / read the egress kill switch. Payload: {enabled?: bool}",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "egress.destinations",
        |p: Value| async move { handle_destinations(p).await },
        "List per-component egress destinations.",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "egress.forward",
        |p: Value| async move { handle_forward(p).await },
        "Unary outbound HTTP call through the egress allowlist.",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "extensions.dispatch",
        |p: Value| async move { handle_extensions_dispatch(p).await },
        "Dispatch a browser-extension request through the wylde-extension-bridge pipe.",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "tools.list",
        |p: Value| async move { handle_tools_list(p).await },
        "Return every registered tool's manifest summary (alias-keyed).",
        "wylde_gateway::pipe",
    );
    register_action_with_meta(
        "tools.get",
        |p: Value| async move { handle_tools_get(p).await },
        "Return the full manifest for a single tool by id or alias.",
        "wylde_gateway::pipe",
    );
    tracing::info!("pipe: registered {} Gateway actions", ACTIONS.len());
}

/// Test-only: drop every action handler. Mirrors the device-gate pipe
/// teardown so test ordering doesn't bleed state.
pub fn uninstall() {
    for name in ACTIONS {
        unregister_action(name);
    }
    INSTALLED.store(false, Ordering::SeqCst);
}

// ── gateway.ping ───────────────────────────────────────────────────────

async fn handle_ping(payload: Value) -> Reply {
    let echo = match &payload {
        Value::Object(m) => m.get("echo").cloned(),
        _ => None,
    };
    let mut data = serde_json::Map::new();
    data.insert("ok".into(), Value::Bool(true));
    data.insert("service".into(), Value::String(SERVICE_NAME.into()));
    if let Some(e) = echo {
        data.insert("pong".into(), e);
    }
    Reply::ok(Value::Object(data))
}

// ── egress.kill_switch ─────────────────────────────────────────────────

async fn handle_kill_switch(payload: Value) -> Reply {
    if let Value::Object(m) = &payload {
        if let Some(Value::Bool(b)) = m.get("enabled") {
            egress::set_blocked(*b);
        } else if let Some(other) = m.get("enabled") {
            // Loose-coerce: 1/0, "true"/"false" — but reject types that
            // can't be normalized so callers get a clear error.
            match other {
                Value::Number(n) => {
                    let truthy = n.as_i64().map(|v| v != 0).unwrap_or(false);
                    egress::set_blocked(truthy);
                }
                Value::String(s) => {
                    let truthy =
                        matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
                    egress::set_blocked(truthy);
                }
                Value::Null => {}
                _ => {
                    return Reply::err(IpcError::new("bad_request", "enabled must be a boolean"));
                }
            }
        }
    }
    Reply::ok(json!({ "engaged": egress::is_blocked() }))
}

// ── egress.destinations ────────────────────────────────────────────────

async fn handle_destinations(_payload: Value) -> Reply {
    Reply::ok(json!({
        "destinations": egress::list_destinations(),
        "kill_switch_engaged": egress::is_blocked(),
    }))
}

// ── egress.forward ─────────────────────────────────────────────────────

async fn handle_forward(payload: Value) -> Reply {
    let obj = match payload {
        Value::Object(m) => m,
        _ => {
            return Reply::err(IpcError::new("bad_request", "payload must be a map"));
        }
    };
    let caller = obj
        .get("caller")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let dest_key = obj
        .get("dest")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_owned();
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("/")
        .to_owned();
    let body = obj.get("body").cloned();
    let headers = obj.get("headers").and_then(|v| match v {
        Value::Object(m) => Some(
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect::<std::collections::HashMap<_, _>>(),
        ),
        _ => None,
    });
    let timeout_secs = obj
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(30.0)
        .max(0.001);

    let result = egress::client::forward(
        &caller,
        &dest_key,
        &method,
        &path,
        body.as_ref(),
        headers.as_ref(),
        Duration::from_secs_f64(timeout_secs),
    )
    .await;

    match result {
        Ok(r) => Reply::ok(json!({
            "status": r.status,
            "headers": r.headers,
            "body": r.body,
            "duration_ms": (r.duration_ms * 1000.0).round() / 1000.0,
        })),
        Err(e) => egress_error_to_reply(e),
    }
}

fn egress_error_to_reply(e: EgressError) -> Reply {
    match e {
        EgressError::Blocked => Reply::err(IpcError::new(
            "egress_blocked",
            "egress kill switch is engaged",
        )),
        EgressError::Denied(msg) => Reply::err(IpcError::new("egress_denied", msg)),
        EgressError::Policy(msg) => Reply::err(IpcError::new("egress_denied", msg)),
        EgressError::Ssrf(msg) => Reply::err(IpcError::new("egress_denied", msg)),
        EgressError::Upstream(msg) => Reply::err(IpcError::new("egress_upstream_error", msg)),
    }
}

// ── extensions.dispatch ────────────────────────────────────────────────

/// Dispatch a browser-extension call through the `wylde-extension-bridge`
/// pipe. Mirrors the HTTP route in [`crate::routes::extensions`] — both
/// go through [`crate::routes::extensions::dispatch_through_bridge`], so
/// the pipe and HTTP surfaces fold bridge errors the same way.
async fn handle_extensions_dispatch(payload: Value) -> Reply {
    let obj = match &payload {
        Value::Object(m) => m,
        _ => {
            return Reply::err(IpcError::new("bad_request", "payload must be a map"));
        }
    };
    let extension = obj
        .get("extension")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    match crate::routes::extensions::dispatch_through_bridge(&extension, &endpoint, params).await {
        Ok(data) => Reply::ok(data),
        Err(f) => Reply::err(IpcError::new(f.code, f.message)),
    }
}

// ── tools.list ─────────────────────────────────────────────────────────

async fn handle_tools_list(_payload: Value) -> Reply {
    match crate::proxy_core::pipe_action("wylde-harness", "tools.list", json!({})).await {
        Ok(data) => {
            let canonical = extract_canonical_list(&data);
            let alias_keyed = reshape_to_alias_keyed(&canonical);
            Reply::ok(json!({
                "count": alias_keyed.len(),
                "tools": alias_keyed,
            }))
        }
        Err((_, body)) => {
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("upstream_error")
                .to_owned();
            let msg = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Reply::err(IpcError::new(code, msg))
        }
    }
}

// ── tools.get ──────────────────────────────────────────────────────────

async fn handle_tools_get(payload: Value) -> Reply {
    let tool_id = match &payload {
        Value::Object(m) => m
            .get("tool_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    };
    if tool_id.is_empty() {
        return Reply::err(IpcError::new("bad_request", "tool_id is required"));
    }
    // Try harness `tools.get` first; the `not_implemented` /
    // `unknown_action` arm below falls back to `tools.list`, so this is
    // a deliberate optional-verb probe.
    // wylde-check: optional-verb
    match crate::proxy_core::pipe_action(
        "wylde-harness",
        "tools.get",
        json!({ "tool_id": tool_id }),
    )
    .await
    {
        Ok(data) => Reply::ok(data),
        Err((_, body)) => {
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if code == "tool_not_found" {
                return Reply::err(IpcError::new("tool_not_found", tool_id));
            }
            if code == "not_implemented" || code == "unknown_action" {
                return fallback_tools_get_via_list(&tool_id).await;
            }
            let msg = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Reply::err(IpcError::new(code.to_owned(), msg))
        }
    }
}

async fn fallback_tools_get_via_list(tool_id: &str) -> Reply {
    match crate::proxy_core::pipe_action("wylde-harness", "tools.list", json!({})).await {
        Ok(data) => {
            let canonical = extract_canonical_list(&data);
            let alias_keyed = reshape_to_alias_keyed(&canonical);
            match alias_keyed.get(tool_id) {
                Some(v) => Reply::ok(v.clone()),
                None => Reply::err(IpcError::new("tool_not_found", tool_id)),
            }
        }
        Err((_, body)) => {
            let code = body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("upstream_error")
                .to_owned();
            let msg = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Reply::err(IpcError::new(code, msg))
        }
    }
}

// ── Shared reshape helpers ─────────────────────────────────────────────
//
// Mirror `routes::tool_registry`'s alias derivation — same Python rule,
// `_alias_keys_for(entry)` — so HTTP and pipe surfaces produce
// byte-equivalent shapes.

fn extract_canonical_list(reply: &Value) -> Vec<Value> {
    let data = reply.get("data").unwrap_or(reply);
    if let Some(tools) = data.get("tools") {
        return match tools {
            Value::Array(a) => a.clone(),
            Value::Object(m) => m.values().cloned().collect(),
            _ => Vec::new(),
        };
    }
    match data {
        Value::Array(a) => a.clone(),
        Value::Object(m) => {
            if m.values().all(|v| v.is_object()) {
                m.values().cloned().collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn reshape_to_alias_keyed(entries: &[Value]) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for entry in entries {
        for key in alias_keys_for(entry) {
            out.insert(key, entry.clone());
        }
    }
    out
}

fn alias_keys_for(entry: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(4);
    if let Some(id) = entry.get("id").and_then(Value::as_str) {
        push_unique(&mut out, id.to_owned());
        push_unique(&mut out, id.replace('_', "."));
    }
    if let Some(name) = entry.get("name").and_then(Value::as_str) {
        push_unique(&mut out, name.to_owned());
        push_unique(&mut out, name.replace('.', "_"));
    }
    out
}

fn push_unique(out: &mut Vec<String>, key: String) {
    if !key.is_empty() && !out.contains(&key) {
        out.push(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use wylde_shared::ipc::list_actions;

    static INSTALL_LOCK: StdMutex<()> = StdMutex::new(());
    use crate::egress::kill_switch::EGRESS_TEST_LOCK;

    #[tokio::test]
    async fn install_registers_all_actions() {
        let _g = INSTALL_LOCK.lock().expect("install lock");
        uninstall();
        install();
        let names = list_actions();
        for a in ACTIONS {
            assert!(names.contains(&a.to_string()), "missing: {a}");
        }
        uninstall();
    }

    #[tokio::test]
    async fn ping_echoes_payload() {
        let reply = handle_ping(json!({"echo": "hi"})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["pong"], "hi");
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = INSTALL_LOCK.lock().expect("install lock");
        uninstall();
        install();
        install();
        let count = list_actions()
            .into_iter()
            .filter(|n| ACTIONS.contains(&n.as_str()))
            .count();
        assert_eq!(count, ACTIONS.len());
        uninstall();
    }

    #[tokio::test]
    async fn kill_switch_toggles_state() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        egress::set_blocked(false);
        let r1 = handle_kill_switch(json!({"enabled": true})).await;
        assert!(r1.ok);
        assert_eq!(r1.data["engaged"], true);
        let r2 = handle_kill_switch(json!({"enabled": false})).await;
        assert_eq!(r2.data["engaged"], false);
    }

    #[tokio::test]
    async fn kill_switch_read_only_when_no_enabled_field() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        egress::set_blocked(true);
        let r = handle_kill_switch(json!({})).await;
        assert!(r.ok);
        assert_eq!(r.data["engaged"], true);
        egress::set_blocked(false);
    }

    #[tokio::test]
    async fn destinations_returns_kill_state_and_dict() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        egress::destinations::reset_for_test();
        egress::set_blocked(false);
        let r = handle_destinations(Value::Null).await;
        assert!(r.ok);
        assert!(r.data["destinations"].is_object());
        assert_eq!(r.data["kill_switch_engaged"], false);
    }

    #[tokio::test]
    async fn forward_rejects_non_object_payload() {
        let r = handle_forward(Value::String("nope".into())).await;
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("bad_request")
        );
    }

    #[tokio::test]
    async fn forward_blocked_when_kill_switch_engaged() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        egress::set_blocked(true);
        let r = handle_forward(json!({
            "caller": "X",
            "dest": "y",
            "method": "GET",
            "path": "/",
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("egress_blocked")
        );
        egress::set_blocked(false);
    }

    #[tokio::test]
    async fn extensions_dispatch_unknown_extension_is_error() {
        // No bridge service is part of the unit-test fixture, so an
        // unknown extension always errors: a 503-equivalent
        // `extension_bridge_unavailable` when the pipe is down, or
        // `extension_not_found` if a live bridge answers.
        let r = handle_extensions_dispatch(json!({
            "extension": "no_such_ext",
            "endpoint": "x",
        }))
        .await;
        assert!(!r.ok);
        let code = r.error.as_ref().map(|e| e.code.as_str()).unwrap_or("");
        assert!(
            code == "extension_bridge_unavailable" || code == "extension_not_found",
            "unexpected code: {code}"
        );
    }

    #[tokio::test]
    async fn extensions_dispatch_rejects_non_object_payload() {
        let r = handle_extensions_dispatch(Value::String("nope".into())).await;
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("bad_request")
        );
    }

    #[test]
    fn alias_keys_match_python_derivation() {
        let entry = json!({
            "id": "memory_long_term_save",
            "name": "memory.long_term.save",
        });
        let keys = alias_keys_for(&entry);
        assert!(keys.contains(&"memory_long_term_save".to_owned()));
        assert!(keys.contains(&"memory.long_term.save".to_owned()));
    }

    #[test]
    fn reshape_dedupes_overlapping_aliases() {
        let entries = vec![json!({"id": "rag_ask", "name": "rag.ask"})];
        let dict = reshape_to_alias_keyed(&entries);
        // `id` and the dotted `id_swap_to_dotted` collide with `name` —
        // dedup means we still have just 2 unique keys.
        assert_eq!(dict.len(), 2);
        assert!(dict.contains_key("rag_ask"));
        assert!(dict.contains_key("rag.ask"));
    }
}
