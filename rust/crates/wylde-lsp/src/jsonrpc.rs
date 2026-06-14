//! LSP wire framing — JSON-RPC 2.0 over `Content-Length`-delimited frames.
//!
//! The encode side + header parsing are pure and unit-tested here; the async
//! read/write against the rust-analyzer child's stdio lives in [`crate::client`].

use serde_json::{json, Value};

/// Encode a JSON-RPC message into an LSP wire frame
/// (`Content-Length: N\r\n\r\n<body>`).
pub fn encode(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Parse the `Content-Length` value out of a frame's header block (the text
/// before the blank line). Case-insensitive on the header name. `None` when
/// absent or unparseable.
pub fn content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n").flat_map(|l| l.split('\n')) {
        let mut parts = line.splitn(2, ':');
        let (name, value) = (parts.next()?, parts.next());
        if name.trim().eq_ignore_ascii_case("content-length") {
            if let Some(v) = value {
                if let Ok(n) = v.trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Build a JSON-RPC **request** (expects a response, carries an `id`).
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Build a JSON-RPC **notification** (no response, no `id`).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prefixes_content_length() {
        let frame = encode(&json!({ "jsonrpc": "2.0", "id": 1, "method": "x" }));
        let text = String::from_utf8(frame).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));
        // The byte count after the blank line equals the declared length.
        let (header, body) = text.split_once("\r\n\r\n").unwrap();
        let declared = content_length(header).unwrap();
        assert_eq!(declared, body.len());
    }

    #[test]
    fn content_length_is_case_insensitive_and_tolerant() {
        assert_eq!(content_length("Content-Length: 42"), Some(42));
        assert_eq!(content_length("content-length:  7 "), Some(7));
        assert_eq!(
            content_length("Content-Type: x\r\nContent-Length: 13"),
            Some(13)
        );
        assert_eq!(content_length("X-Other: 5"), None);
        assert_eq!(content_length("Content-Length: not-a-number"), None);
    }

    #[test]
    fn request_and_notification_shapes() {
        let r = request(7, "textDocument/hover", json!({"a": 1}));
        assert_eq!(r["id"], 7);
        assert_eq!(r["method"], "textDocument/hover");
        let n = notification("textDocument/didOpen", json!({}));
        assert!(n.get("id").is_none());
        assert_eq!(n["method"], "textDocument/didOpen");
    }
}
