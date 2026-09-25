//! OpenAI server-sent events: bare `data: {chunk}` frames ending in
//! `data: [DONE]`.
//!
//! This is deliberately *not* [`crate::streaming`]: that module emits
//! Wylde's `event: token` frames wrapped in `{ok: true, …}`, which OpenAI
//! SDKs don't parse. A mid-stream failure is sent as a final
//! `data: {"error": …}` frame and the stream ends without `[DONE]`, which
//! is how OpenAI clients detect a failed stream.

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use futures::Stream;
use serde_json::Value;

use super::errors::OpenAiError;

/// One `data: <compact JSON>\n\n` frame.
pub fn chunk(payload: &Value) -> Bytes {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_owned());
    Bytes::from(format!("data: {json}\n\n"))
}

/// The terminal `data: [DONE]\n\n` frame.
pub fn done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

/// A mid-stream error frame (`data: {"error": …}`); send it last and
/// omit [`done`].
pub fn error(err: &OpenAiError) -> Bytes {
    chunk(&err.body())
}

/// Wrap a stream of frames as a `text/event-stream` 200 response.
pub fn response<S>(frames: S) -> Response
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let mut resp = Response::new(Body::from_stream(frames));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;

    #[test]
    fn chunk_is_a_bare_data_frame() {
        let f = chunk(&json!({"id": "c1", "choices": []}));
        assert_eq!(&f[..], &b"data: {\"choices\":[],\"id\":\"c1\"}\n\n"[..]);
    }

    #[test]
    fn done_and_error_frames() {
        assert_eq!(&done()[..], b"data: [DONE]\n\n");
        let e = error(&OpenAiError::upstream_unavailable());
        let s = std::str::from_utf8(&e).unwrap();
        assert!(s.starts_with("data: {\"error\":"), "{s}");
        assert!(!s.contains("\"ok\""));
    }

    #[tokio::test]
    async fn response_streams_frames_with_sse_headers() {
        let frames = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(chunk(&json!({"n": 1}))),
            Ok(done()),
        ]);
        let resp = response(frames);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"data: {\"n\":1}\n\ndata: [DONE]\n\n");
    }
}
