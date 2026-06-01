//! Thin Rust client for the `wylde-memgraph` service.
//!
//! Rust port of `Core/harness/memory/memgraph.py`. The Python client speaks
//! a hand-rolled msgpack-over-named-pipe protocol with a legacy envelope
//! shape (`{v, method, verb, data}`, no handshake / `id` / `meta`). The
//! Memgraph service's IPC server still accepts those v0 frames via its
//! pre-v1 fallback path, but new code has no reason to reproduce that
//! debt — this client routes everything through `wylde_shared::ipc`,
//! which speaks the modern v1 envelope (handshake + `id` + `meta`).
//!
//! The transport is abstracted behind a callback (`SendFn`) so tests can
//! plug in canned replies without touching a real pipe. The default
//! constructor wires it to [`wylde_shared::ipc::send_with_verb`].
//!
//! ## Known divergences from the Python client
//!
//! 1. `multihop` sends the payload keys the **server** route expects
//!    (`entities`, `expand_hops`) rather than the keys the Python client
//!    sends (`start`, `max_hops`). The Python client's keys don't match
//!    the route's reader — every `memgraph.multihop()` Python call has
//!    been arriving at the server with an empty entity list. This Rust
//!    port aligns with the server, which is the source of truth.
//! 2. `traverse` accepts optional `workspace`, `decay_alpha`, and
//!    `rel_depths` parameters that the Python `traverse()` signature
//!    silently drops on the floor — the server route reads them, the
//!    Python client never sent them. Same alignment rationale.
//!
//! Both divergences are captured in [[wylde_memgraph_python_client_bugs]].

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

/// Default service identifier (`pipe_name("memgraph")` →
/// `\\.\pipe\wylde-memgraph`). Override via `WYLDE_MEMGRAPH_SERVICE` if a
/// non-standard deployment uses a different pipe name — same env knob
/// the harness's `common::memgraph_service_name` reads.
pub const DEFAULT_SERVICE: &str = "memgraph";

/// Default per-call timeout, matching Python's `_DEFAULT_TIMEOUT_S`.
pub const DEFAULT_TIMEOUT_SECS: f64 = 5.0;

/// Transport callback. Returns a [`Reply`] for the given pipe call.
///
/// Real production use wires this to [`wylde_shared::ipc::send_with_verb`].
/// Tests build a closure that captures canned responses.
pub type SendFn = Arc<
    dyn Fn(String, String, Value, Duration) -> BoxFuture<'static, Reply> + Send + Sync + 'static,
>;

/// Memgraph IPC client. Cheap to clone (transport is `Arc<dyn Fn>`).
#[derive(Clone)]
pub struct Client {
    send: SendFn,
    default_timeout: Duration,
    /// Service identifier — exposed for diagnostics and so callers can
    /// build the equivalent `pipe_name` if they need to log it.
    service: String,
}

impl Client {
    /// Build a client wired to the live IPC transport. Calls go to the
    /// pipe `\\.\pipe\wylde-<service>`; the env knob
    /// `WYLDE_MEMGRAPH_SERVICE` overrides the service name.
    pub fn new() -> Self {
        let service = crate::memory::common::memgraph_service_name();
        let service_for_send = service.clone();
        let send: SendFn = Arc::new(move |method, verb, payload, timeout| {
            let svc = service_for_send.clone();
            // Drop the `wylde-` prefix so `send_with_verb` doesn't double
            // it — `pipe_name` re-adds it internally. Same shape Python's
            // `memgraph.py` uses against the legacy pipe name.
            let bare = svc.strip_prefix("wylde-").unwrap_or(&svc).to_owned();
            Box::pin(async move {
                wylde_shared::ipc::send_with_verb(&bare, &method, &verb, payload, timeout).await
            })
        });
        Self {
            send,
            default_timeout: Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS),
            service,
        }
    }

    /// Build a client around an arbitrary transport. Tests use this to
    /// plug in deterministic replies.
    pub fn with_send(send: SendFn) -> Self {
        Self {
            send,
            default_timeout: Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS),
            service: DEFAULT_SERVICE.to_owned(),
        }
    }

    /// Service name this client targets (e.g. `"wylde-memgraph"`).
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Override the per-call default timeout. Returns `self` for fluent
    /// configuration.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    async fn send_call(
        &self,
        method: &str,
        verb: &str,
        payload: Value,
        timeout: Option<Duration>,
    ) -> Reply {
        let timeout = timeout.unwrap_or(self.default_timeout);
        (self.send)(method.to_owned(), verb.to_owned(), payload, timeout).await
    }

    // ── Public route surface (mirrors Python's `memgraph.py`) ──────────

    /// `GET /health` — returns `{"ok": bool}`.
    pub async fn health(&self) -> Reply {
        self.send_call("/health", "GET", Value::Null, Some(Duration::from_secs(2)))
            .await
    }

    /// `POST /ensure_schema` — idempotent index creation.
    pub async fn ensure_schema(&self) -> Reply {
        self.send_call(
            "/ensure_schema",
            "POST",
            Value::Null,
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `POST /upsert` — body shape `{"chunks": [{id, path, symbol,
    /// language, entities}, ...]}`. Chunks pass through as opaque JSON.
    pub async fn upsert(&self, chunks: Vec<Value>) -> Reply {
        self.send_call(
            "/upsert",
            "POST",
            json!({ "chunks": chunks }),
            Some(Duration::from_secs(30)),
        )
        .await
    }

    /// `POST /delete_path` — drop chunks/edges for one source path.
    pub async fn delete_path(&self, path: &str) -> Reply {
        self.send_call(
            "/delete_path",
            "POST",
            json!({ "path": path }),
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `POST /delete_workspace` — drop everything for a workspace.
    pub async fn delete_workspace(&self, workspace_id: &str) -> Reply {
        self.send_call(
            "/delete_workspace",
            "POST",
            json!({ "workspace": workspace_id }),
            Some(Duration::from_secs(30)),
        )
        .await
    }

    /// `POST /traverse` — entity-anchored chunk discovery.
    ///
    /// Accepts the full `TraverseRequest` surface the server reads,
    /// including the `workspace` / `decay_alpha` / `rel_depths` fields
    /// the Python client silently dropped (see module docs).
    pub async fn traverse(&self, req: TraverseRequest) -> Reply {
        self.send_call(
            "/traverse",
            "POST",
            req.to_payload(),
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `POST /relate` — typed Entity→Entity edges. `rel_type` must be
    /// one of [`super::schema::REL_CALLS`] / `REL_IMPORTS` / `REL_INHERITS`
    /// / `REL_CONFIGURES` / `REL_EXPOSES` — validated server-side.
    pub async fn relate(&self, rel_type: &str, pairs: Vec<EntityPair>) -> Reply {
        self.send_call(
            "/relate",
            "POST",
            json!({
                "rel_type": rel_type,
                "pairs": pairs.into_iter().map(EntityPair::into_value).collect::<Vec<_>>(),
            }),
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `POST /unrelate` — drop the given typed edges.
    pub async fn unrelate(&self, rel_type: &str, pairs: Vec<EntityPair>) -> Reply {
        self.send_call(
            "/unrelate",
            "POST",
            json!({
                "rel_type": rel_type,
                "pairs": pairs.into_iter().map(EntityPair::into_value).collect::<Vec<_>>(),
            }),
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `POST /multihop` — multi-hop traversal from seed entities.
    ///
    /// Payload uses the **server-aligned** field names (`entities`,
    /// `expand_hops`) — see module docs for why this diverges from the
    /// Python client's wire shape.
    pub async fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32) -> Reply {
        self.send_call(
            "/multihop",
            "POST",
            json!({
                "entities": entities,
                "expand_hops": expand_hops,
                "limit": limit,
            }),
            Some(Duration::from_secs(15)),
        )
        .await
    }

    /// `POST /upsert_edge` — MERGE-style strengthen-or-create on a
    /// `source -[label]-> target` edge.
    pub async fn upsert_edge(
        &self,
        source: &str,
        label: &str,
        target: &str,
        weight_delta: f64,
    ) -> Reply {
        self.send_call(
            "/upsert_edge",
            "POST",
            json!({
                "source": source,
                "label": label,
                "target": target,
                "weight_delta": weight_delta,
            }),
            Some(Duration::from_secs(10)),
        )
        .await
    }

    /// `GET /stats` — `{ok, entities, chunks, mentions}`.
    pub async fn stats(&self) -> Reply {
        self.send_call("/stats", "GET", Value::Null, Some(Duration::from_secs(5)))
            .await
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Payload for `/traverse`. The server reads `entities` + `max_hops` +
/// `limit` unconditionally; `workspace`, `decay_alpha`, and
/// `rel_depths` are optional, default-supplied by the route.
#[derive(Clone, Debug, Default)]
pub struct TraverseRequest {
    pub entities: Vec<String>,
    pub max_hops: u32,
    pub limit: u32,
    pub workspace: Option<String>,
    pub decay_alpha: Option<f64>,
    /// Optional per-bucket depth budget. Bucket keys are
    /// [`super::schema::BUCKET_CALLS_IMPORTS`] /
    /// [`super::schema::BUCKET_CONFIGURES_EXPOSES`].
    pub rel_depths: Option<Vec<(String, u32)>>,
}

impl TraverseRequest {
    /// Build a minimum-shape request: just an entity list with default
    /// hop / limit budgets (matching Python's `traverse(entities)` call
    /// with default kwargs).
    pub fn for_entities(entities: Vec<String>) -> Self {
        Self {
            entities,
            max_hops: 2,
            limit: 50,
            workspace: None,
            decay_alpha: None,
            rel_depths: None,
        }
    }

    fn to_payload(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("entities".to_owned(), json!(self.entities));
        obj.insert("max_hops".to_owned(), json!(self.max_hops));
        obj.insert("limit".to_owned(), json!(self.limit));
        if let Some(ws) = &self.workspace {
            if !ws.is_empty() {
                obj.insert("workspace".to_owned(), json!(ws));
            }
        }
        if let Some(a) = self.decay_alpha {
            obj.insert("decay_alpha".to_owned(), json!(a));
        }
        if let Some(rd) = &self.rel_depths {
            let mut map = serde_json::Map::new();
            for (bucket, depth) in rd {
                map.insert(bucket.clone(), json!(depth));
            }
            obj.insert("rel_depths".to_owned(), Value::Object(map));
        }
        Value::Object(obj)
    }
}

/// One pair of entity names for `/relate` / `/unrelate`.
#[derive(Clone, Debug)]
pub struct EntityPair {
    pub source: String,
    pub target: String,
}

impl EntityPair {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }

    fn into_value(self) -> Value {
        json!({ "source": self.source, "target": self.target })
    }
}

/// Helper for callers that want raise-on-failure semantics: turns a
/// not-ok [`Reply`] into an [`IpcError`].
pub fn ok_or_err(reply: Reply) -> Result<Value, IpcError> {
    if reply.ok {
        Ok(reply.data)
    } else {
        Err(reply
            .error
            .unwrap_or_else(|| IpcError::new("unknown", "memgraph call failed with no error body")))
    }
}

// ── Test helpers ──────────────────────────────────────────────────────

/// A deterministic mock transport for offline tests. Records every call
/// and replies with a [`Reply`] picked by a closure the test supplies.
#[cfg(test)]
pub mod mock {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use wylde_shared::ipc::Reply;

    /// One recorded call.
    #[derive(Clone, Debug)]
    pub struct RecordedCall {
        pub method: String,
        pub verb: String,
        pub payload: Value,
    }

    /// Shared, cloneable handle into a mock transport's call log + reply
    /// builder. The reply builder is a closure run on each call.
    #[derive(Clone)]
    pub struct MockHandle {
        inner: Arc<Mutex<MockInner>>,
    }

    struct MockInner {
        calls: Vec<RecordedCall>,
        responder: Box<dyn Fn(&RecordedCall) -> Reply + Send + Sync>,
    }

    impl MockHandle {
        pub fn calls(&self) -> Vec<RecordedCall> {
            self.inner.lock().expect("mock log").calls.clone()
        }
    }

    /// Build a mock client whose reply for every call is supplied by
    /// `responder`. The returned [`super::Client`] is interchangeable
    /// with a real one.
    pub fn new_with_responder<F>(responder: F) -> (super::Client, MockHandle)
    where
        F: Fn(&RecordedCall) -> Reply + Send + Sync + 'static,
    {
        let inner = Arc::new(Mutex::new(MockInner {
            calls: Vec::new(),
            responder: Box::new(responder),
        }));
        let handle = MockHandle {
            inner: Arc::clone(&inner),
        };
        let send: super::SendFn = Arc::new(move |method, verb, payload, _timeout| {
            let call = RecordedCall {
                method,
                verb,
                payload,
            };
            let reply = {
                let mut guard = inner.lock().expect("mock log");
                guard.calls.push(call.clone());
                (guard.responder)(&call)
            };
            Box::pin(async move { reply })
        });
        (super::Client::with_send(send), handle)
    }

    /// Convenience: build a mock that replies `Reply::ok(data)` for every
    /// call. Useful when the test only cares about *what* was sent.
    pub fn new_with_static_ok(data: Value) -> (super::Client, MockHandle) {
        new_with_responder(move |_| Reply::ok(data.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memgraph::schema;
    use serde_json::json;

    #[tokio::test]
    async fn health_sends_get_to_health_route() {
        let (client, handle) = mock::new_with_static_ok(json!({"ok": true}));
        let reply = client.health().await;
        assert!(reply.ok);
        let calls = handle.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "/health");
        assert_eq!(calls[0].verb, "GET");
        assert!(calls[0].payload.is_null());
    }

    #[tokio::test]
    async fn upsert_wraps_chunks_in_chunks_key() {
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        let chunks = vec![json!({"id": "c1", "path": "a.py"})];
        client.upsert(chunks).await;
        let calls = handle.calls();
        assert_eq!(calls[0].method, "/upsert");
        assert_eq!(calls[0].payload["chunks"][0]["id"], "c1");
    }

    #[tokio::test]
    async fn traverse_payload_includes_optional_fields_when_set() {
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        let req = TraverseRequest {
            entities: vec!["foo".into(), "bar".into()],
            max_hops: 3,
            limit: 25,
            workspace: Some("ws-1".into()),
            decay_alpha: Some(0.5),
            rel_depths: Some(vec![
                (schema::BUCKET_CALLS_IMPORTS.to_owned(), 1),
                (schema::BUCKET_CONFIGURES_EXPOSES.to_owned(), 2),
            ]),
        };
        client.traverse(req).await;
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["entities"][0], "foo");
        assert_eq!(payload["max_hops"], 3);
        assert_eq!(payload["limit"], 25);
        assert_eq!(payload["workspace"], "ws-1");
        assert_eq!(payload["decay_alpha"], 0.5);
        assert_eq!(payload["rel_depths"]["calls_imports"], 1);
        assert_eq!(payload["rel_depths"]["configures_exposes"], 2);
    }

    #[tokio::test]
    async fn traverse_payload_omits_optional_fields_when_unset() {
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        client
            .traverse(TraverseRequest::for_entities(vec!["x".into()]))
            .await;
        let payload = &handle.calls()[0].payload;
        let obj = payload.as_object().expect("object payload");
        assert!(obj.contains_key("entities"));
        assert!(obj.contains_key("max_hops"));
        assert!(obj.contains_key("limit"));
        // Optional fields stay out of the envelope when unset, mirroring
        // the Python client's minimum-shape body.
        assert!(!obj.contains_key("workspace"));
        assert!(!obj.contains_key("decay_alpha"));
        assert!(!obj.contains_key("rel_depths"));
    }

    #[tokio::test]
    async fn multihop_uses_server_aligned_field_names() {
        // Pin the divergence-from-Python: this sends `entities` (not
        // `start`) and `expand_hops` (not `max_hops`). If a future
        // change "fixes" this to match Python's buggy keys, the server
        // route stops returning chunks — that's the regression this
        // test guards against.
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        client.multihop(vec!["a".into()], 2, 30).await;
        let payload = &handle.calls()[0].payload;
        assert!(payload.get("entities").is_some(), "field must be entities");
        assert!(payload.get("expand_hops").is_some(), "field must be expand_hops");
        assert!(payload.get("start").is_none(), "must not send `start`");
        assert!(payload.get("max_hops").is_none(), "must not send `max_hops`");
        assert_eq!(payload["entities"][0], "a");
        assert_eq!(payload["expand_hops"], 2);
        assert_eq!(payload["limit"], 30);
    }

    #[tokio::test]
    async fn relate_serialises_pairs_into_source_target_objects() {
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        client
            .relate(
                schema::REL_CALLS,
                vec![
                    EntityPair::new("foo", "bar"),
                    EntityPair::new("baz", "qux"),
                ],
            )
            .await;
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["rel_type"], "CALLS");
        assert_eq!(payload["pairs"][0]["source"], "foo");
        assert_eq!(payload["pairs"][0]["target"], "bar");
        assert_eq!(payload["pairs"][1]["source"], "baz");
    }

    #[tokio::test]
    async fn upsert_edge_carries_weight_delta() {
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        client.upsert_edge("a", "MENTIONS", "b", 1.5).await;
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["source"], "a");
        assert_eq!(payload["label"], "MENTIONS");
        assert_eq!(payload["target"], "b");
        assert_eq!(payload["weight_delta"], 1.5);
    }

    #[tokio::test]
    async fn delete_workspace_uses_workspace_key_not_workspace_id() {
        // The server route reads `body.get("workspace")` not
        // `workspace_id` — Python's signature happens to take
        // workspace_id positionally but maps it to "workspace" in the
        // payload. Pin the wire-side name.
        let (client, handle) = mock::new_with_static_ok(Value::Null);
        client.delete_workspace("ws-7").await;
        let payload = &handle.calls()[0].payload;
        assert_eq!(payload["workspace"], "ws-7");
        assert!(payload.get("workspace_id").is_none());
    }

    #[tokio::test]
    async fn ok_or_err_passes_data_through_on_success() {
        let v = ok_or_err(Reply::ok(json!({"x": 1}))).unwrap();
        assert_eq!(v["x"], 1);
    }

    #[tokio::test]
    async fn ok_or_err_surfaces_ipc_error_on_failure() {
        let err = ok_or_err(Reply::err_msg("not_found", "no node")).unwrap_err();
        assert_eq!(err.code, "not_found");
        assert_eq!(err.message, "no node");
    }

    #[tokio::test]
    async fn new_client_uses_env_service_name() {
        // Snapshot + restore so we don't leak into sibling tests.
        let prev = std::env::var("WYLDE_MEMGRAPH_SERVICE").ok(); // wylde-check: discard-result-ok
        std::env::set_var("WYLDE_MEMGRAPH_SERVICE", "test-memgraph-xyz");
        let c = Client::new();
        assert_eq!(c.service(), "test-memgraph-xyz");
        match prev {
            Some(v) => std::env::set_var("WYLDE_MEMGRAPH_SERVICE", v),
            None => std::env::remove_var("WYLDE_MEMGRAPH_SERVICE"),
        }
    }
}
