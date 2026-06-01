//! Request-trace middleware — stamps an ID on every request.
//!
//! Rust port of `Gateway/middleware/trace.py`. If the caller supplied
//! `X-Wylde-Request-ID`, honour it; otherwise mint a fresh UUID-4.
//! The ID is inserted into request extensions so handlers and the
//! audit logger can grab it, and echoed back as a response header.

use std::task::{Context, Poll};

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::response::Response;
use futures::future::BoxFuture;
use tower::{Layer, Service};
use uuid::Uuid;

/// Header name carrying the request id in both directions.
pub const REQUEST_ID_HEADER: &str = "X-Wylde-Request-ID";

/// Request-extension carrier for the resolved request id.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// Tower layer wrapper. Apply with `Router::layer(RequestTraceLayer::new())`.
#[derive(Clone, Default)]
pub struct RequestTraceLayer;

impl RequestTraceLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for RequestTraceLayer {
    type Service = RequestTraceMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTraceMiddleware { inner }
    }
}

/// The actual middleware service.
#[derive(Clone)]
pub struct RequestTraceMiddleware<S> {
    inner: S,
}

impl<S> Service<Request> for RequestTraceMiddleware<S>
where
    S: Service<Request, Response = Response> + Send + Clone + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let rid = req
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        req.extensions_mut().insert(RequestId(rid.clone()));

        // Clone the inner service so we can move it into the async block;
        // this is the standard tower middleware pattern.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let mut response: Response = inner.call(req).await?;
            if let Ok(val) = HeaderValue::from_str(&rid) {
                response
                    .headers_mut()
                    .insert(HeaderName::from_static("x-wylde-request-id"), val);
            }
            Ok(response)
        })
    }
}

/// Extract the request id from request extensions; `None` if no
/// [`RequestTraceLayer`] is mounted upstream.
pub fn get_request_id(req: &Request) -> Option<String> {
    req.extensions().get::<RequestId>().map(|r| r.0.clone())
}
