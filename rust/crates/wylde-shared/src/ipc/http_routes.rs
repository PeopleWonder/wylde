//! HTTP-shaped route table for the pipe server.
//!
//! Most Rust services answer the framed `/__action__` envelope (verb-style
//! RPC). A few callers instead address services with an *HTTP-shaped*
//! request — an `http_verb` (`GET`/`POST`/…) plus a URL-style `method`
//! path (`/api/link/status`) carried in the same msgpack request frame.
//! That shape is what the Svelte/GPUI GUI panels speak (see
//! `wylde_gui_pipe::call(service, "GET", "/api/link/status", None)`) and
//! what the Python "Flask-over-pipe" servers answered before the
//! strangler-fig cutover.
//!
//! IMPORTANT — this is **not** a raw-HTTP text parser. The pipe transport
//! is already framed msgpack (`[u32 BE len][rmp body]`); the request line
//! and "headers" arrive pre-parsed as the envelope's `http_verb`,
//! `method`, and `data` fields (see [`crate::ipc::server`]'s `IncomingFrame`). So a
//! route is matched purely on `(method, path)` — no `httparse`, no raw
//! `GET /… HTTP/1.1` line to tokenise. If a future caller ever needs to
//! push genuine raw-HTTP bytes over the pipe, that's a separate transport
//! concern; this table dispatches the parsed shape every live client
//! actually sends.
//!
//! ## Usage
//!
//! ```no_run
//! use wylde_shared::ipc::http_routes::{HttpRouteTable, HttpResponse};
//! use serde_json::json;
//!
//! let routes = HttpRouteTable::new()
//!     .route("GET", "/api/link/status", |_req| async {
//!         HttpResponse::ok(json!({ "enabled": true }))
//!     });
//! // hand `routes` to `ipc::serve_with_http_routes("wylde-vpn", None, routes)`
//! ```
//!
//! Handlers that already produce a [`Reply`] (the action surface) can plug
//! in directly via `Reply::…` → [`HttpResponse`]'s `From<Reply>` impl, so
//! the action verb and the HTTP route share one business-logic fn:
//!
//! ```no_run
//! # use wylde_shared::ipc::http_routes::{HttpRouteTable, HttpResponse};
//! # async fn handle_link_status(_: serde_json::Value) -> wylde_shared::ipc::Reply { todo!() }
//! let routes = HttpRouteTable::new()
//!     .route("GET", "/api/link/status", |req| async move {
//!         HttpResponse::from(handle_link_status(req.body).await)
//!     });
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::ipc::wire::{IpcError, Reply};

/// A parsed inbound HTTP-shaped request handed to a route handler.
///
/// `method` is the HTTP verb (uppercased: `GET`, `POST`, …). `path` is the
/// URL-style route the caller addressed (`/api/link/status`). `body` is the
/// request envelope's `data` field — `Value::Null` when the caller sent no
/// body (the common case for `GET`).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP verb, uppercased.
    pub method: String,
    /// URL-style request path, e.g. `/api/link/status`.
    pub path: String,
    /// Request body (the envelope `data`); `Null` when absent.
    pub body: Value,
}

/// A route handler's reply. Mirrors the [`Reply`] envelope so the wire
/// shape the client observes is identical whether a request hit the
/// action surface or an HTTP route.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub ok: bool,
    pub data: Value,
    pub error: Option<IpcError>,
}

impl HttpResponse {
    /// 200-equivalent success carrying `data`.
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            data,
            error: None,
        }
    }

    /// Failure carrying a structured [`IpcError`].
    pub fn err(error: IpcError) -> Self {
        Self {
            ok: false,
            data: Value::Null,
            error: Some(error),
        }
    }

    /// Convenience for a `not_found` failure (the 404-equivalent).
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::err(IpcError::new("not_found", message))
    }

    /// Convenience for a `bad_request` failure (the 400-equivalent).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::err(IpcError::new("bad_request", message))
    }
}

impl From<Reply> for HttpResponse {
    fn from(r: Reply) -> Self {
        Self {
            ok: r.ok,
            data: r.data,
            error: r.error,
        }
    }
}

/// Boxed future a route handler returns.
pub type HttpHandlerFuture = Pin<Box<dyn Future<Output = HttpResponse> + Send>>;

/// A registered route handler: `Fn(HttpRequest) -> Future<HttpResponse>`,
/// type-erased so handlers with different concrete future types live in
/// one map. Cheaply cloneable (it's an `Arc`).
pub type HttpHandler = Arc<dyn Fn(HttpRequest) -> HttpHandlerFuture + Send + Sync>;

/// A `(verb, path)` → handler table.
///
/// Matching is exact on the uppercased verb and the literal path string —
/// no path-param templating or wildcards. Every live route this unblocks
/// (VPN's `GET /api/link/*`) is static, and keeping the match exact means
/// dispatch is a single `HashMap` lookup with no per-request regex cost.
/// If a service later needs `:param` segments, add a fallback scan over a
/// small vec of compiled patterns *after* the exact-match miss — the
/// builder shape below leaves room for it without touching callers.
#[derive(Clone, Default)]
pub struct HttpRouteTable {
    routes: HashMap<(String, String), HttpHandler>,
}

impl HttpRouteTable {
    /// An empty table. Equivalent to "no HTTP routes" — every request
    /// falls through to the server's `no_handler` reply.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `handler` for `(method, path)`. Consumes and returns `self`
    /// so routes chain builder-style. The verb is uppercased on insert so
    /// lookups are case-insensitive on the verb (paths stay case-sensitive,
    /// matching URL semantics).
    pub fn route<F, Fut>(mut self, method: &str, path: &str, handler: F) -> Self
    where
        F: Fn(HttpRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let h: HttpHandler = Arc::new(move |req| Box::pin(handler(req)));
        self.routes
            .insert((method.to_ascii_uppercase(), path.to_string()), h);
        self
    }

    /// Look up the handler for `(method, path)`. Returns a cloned `Arc`
    /// handle so the caller can `await` it without holding a borrow on the
    /// table (the table is shared behind an `Arc` across connections).
    pub fn lookup(&self, method: &str, path: &str) -> Option<HttpHandler> {
        self.routes
            .get(&(method.to_ascii_uppercase(), path.to_string()))
            .cloned()
    }

    /// Number of registered routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// True when no routes are registered.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// `(verb, path)` pairs for every registered route, sorted — for
    /// startup logging / diagnostics.
    pub fn registered(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.routes.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn route_matches_exact_verb_and_path() {
        let table = HttpRouteTable::new().route("GET", "/api/link/status", |_req| async {
            HttpResponse::ok(json!({ "enabled": true }))
        });
        let h = table
            .lookup("GET", "/api/link/status")
            .expect("route present");
        let resp = h(HttpRequest {
            method: "GET".into(),
            path: "/api/link/status".into(),
            body: Value::Null,
        })
        .await;
        assert!(resp.ok);
        assert_eq!(resp.data["enabled"], json!(true));
    }

    #[test]
    fn verb_match_is_case_insensitive() {
        let table =
            HttpRouteTable::new().route("get", "/x", |_| async { HttpResponse::ok(Value::Null) });
        assert!(table.lookup("GET", "/x").is_some());
        assert!(table.lookup("gEt", "/x").is_some());
        // Path stays case-sensitive.
        assert!(table.lookup("GET", "/X").is_none());
    }

    #[test]
    fn miss_returns_none() {
        let table =
            HttpRouteTable::new().route("GET", "/a", |_| async { HttpResponse::ok(Value::Null) });
        assert!(table.lookup("POST", "/a").is_none());
        assert!(table.lookup("GET", "/b").is_none());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn reply_converts_into_http_response() {
        let ok: HttpResponse = Reply::ok(json!({"k": 1})).into();
        assert!(ok.ok);
        assert_eq!(ok.data["k"], json!(1));

        let err: HttpResponse = Reply::err(IpcError::new("bad_request", "nope")).into();
        assert!(!err.ok);
        assert_eq!(err.error.unwrap().code, "bad_request");
    }

    #[test]
    fn registered_is_sorted() {
        let table = HttpRouteTable::new()
            .route("GET", "/b", |_| async { HttpResponse::ok(Value::Null) })
            .route("GET", "/a", |_| async { HttpResponse::ok(Value::Null) });
        let regs = table.registered();
        assert_eq!(
            regs,
            vec![
                ("GET".to_string(), "/a".to_string()),
                ("GET".to_string(), "/b".to_string()),
            ]
        );
    }
}
