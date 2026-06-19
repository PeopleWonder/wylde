//! Tool handlers — `fetch` / `scrape` / `extract`, the three endpoints the
//! Python `handler.py` exposes as `run_fetch` / `run_scrape` / `run_extract`.
//!
//! Each returns a `serde_json::Value` whose shape (status/code/error keys on
//! failure, the field set on success) mirrors the Python dict so the MCP
//! `structuredContent` payload is byte-compatible with the shim's.

use serde_json::{json, Value};

use crate::egress::fetch_via_gateway;
use crate::{extract, scrape, ssrf};

/// `fetch` — GET a URL, return the body as text or parsed JSON.
pub async fn run_fetch(params: Value) -> Value {
    let url = match str_param(&params, "url") {
        Some(u) => u,
        None => return invalid_params("'url' parameter is required and must be a string"),
    };
    let fmt = params
        .get("format")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("text")
        .to_lowercase();
    if fmt != "text" && fmt != "json" {
        return invalid_params(&format!("'format' must be 'text' or 'json' (got {fmt:?})"));
    }
    let timeout = timeout_param(&params);

    if let Some(err) = ssrf::validate_external_url(&url) {
        return invalid_url(&err, &url);
    }

    let result = match fetch_via_gateway(&url, timeout).await {
        Ok(r) => r,
        Err(e) => return fetch_error("FETCH_ERROR", &e, &url),
    };

    let raw_content = result.content;
    let content_length = raw_content.len();
    let content: Value = if fmt == "json" {
        match serde_json::from_str::<Value>(&raw_content) {
            Ok(v) => v,
            Err(e) => {
                return json!({
                    "status": "error",
                    "code": "PARSE_ERROR",
                    "error": format!("failed to parse JSON: {e}"),
                    "url": url,
                });
            }
        }
    } else {
        Value::String(raw_content)
    };

    json!({
        "status": "ok",
        "url": url,
        "status_code": result.status,
        "content": content,
        "format": fmt,
        "content_length": content_length,
    })
}

/// `scrape` — GET HTML, optionally apply a list of CSS selectors. Single-page
/// only; deep crawling is out of scope (parity with the Python).
pub async fn run_scrape(params: Value) -> Value {
    let url = match str_param(&params, "url") {
        Some(u) => u,
        None => return invalid_params("'url' parameter is required and must be a string"),
    };
    let selectors = match params.get("selectors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr.iter().map(value_to_selector_string).collect(),
        Some(_) => {
            return invalid_params("'selectors' must be a list of CSS selector strings");
        }
    };
    let timeout = timeout_param(&params);

    if let Some(err) = ssrf::validate_external_url(&url) {
        return invalid_url(&err, &url);
    }

    let fetched = match fetch_via_gateway(&url, timeout).await {
        Ok(r) => r,
        Err(e) => return fetch_error("SCRAPE_ERROR", &e, &url),
    };

    let html = fetched.content;
    let extracted = if selectors.is_empty() {
        json!({})
    } else {
        scrape::apply_selectors(&html, &selectors)
    };

    json!({
        "status": "ok",
        "url": url,
        "status_code": fetched.status,
        "content": html,
        "extracted": extracted,
        "selectors_used": selectors,
        "content_length": html.len(),
    })
}

/// `extract` — apply extraction rules to HTML, fetching first if only a URL
/// was given.
pub async fn run_extract(params: Value) -> Value {
    let rules = match params.get("extraction_rules") {
        Some(Value::Object(m)) => m.clone(),
        _ => {
            return invalid_params(
                "'extraction_rules' parameter is required and must be an object",
            );
        }
    };
    let url = str_param(&params, "url");
    let html_param = params.get("html").and_then(Value::as_str);

    if html_param.is_none() && url.is_none() {
        return invalid_params("either 'url' or 'html' must be provided");
    }

    // Resolve the HTML: use the inline `html` if non-empty, else fetch `url`.
    let html: String = match html_param.filter(|s| !s.is_empty()) {
        Some(h) => h.to_owned(),
        None => {
            // URL fetch path — `url` is guaranteed Some here (the both-missing
            // case returned above).
            let url = url.as_deref().unwrap_or("");
            if let Some(err) = ssrf::validate_external_url(url) {
                return invalid_url(&err, url);
            }
            match fetch_via_gateway(url, 10.0).await {
                Ok(r) => r.content,
                Err(e) => return fetch_error("FETCH_ERROR", &e, url),
            }
        }
    };

    let extracted = extract::extract_by_rules(&html, &rules);
    let fields_extracted = extracted.as_object().map(|m| m.len()).unwrap_or(0);

    json!({
        "status": "ok",
        "url": url,
        "extraction_rules": Value::Object(rules),
        "extracted_data": extracted,
        "fields_extracted": fields_extracted,
        "html_length": html.len(),
    })
}

// ── shared helpers ────────────────────────────────────────────────────────

/// Extract a required, non-blank string param.
fn str_param(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// `float(params.get("timeout", 10))` with the same tolerant default.
fn timeout_param(params: &Value) -> f64 {
    params
        .get("timeout")
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(10.0)
}

/// Python `[str(s) for s in selectors_raw]` — coerce each entry to its string
/// form (strings pass through; other JSON scalars stringify).
fn value_to_selector_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn invalid_params(msg: &str) -> Value {
    json!({ "status": "error", "code": "INVALID_PARAMS", "error": msg })
}

fn invalid_url(msg: &str, url: &str) -> Value {
    json!({ "status": "error", "code": "INVALID_URL", "error": msg, "url": url })
}

fn fetch_error(code: &str, msg: &str, url: &str) -> Value {
    json!({ "status": "error", "code": code, "error": msg, "url": url })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fetch paths need a Gateway/network; these tests exercise the
    // pre-fetch validation + the no-fetch `extract` path, all offline.

    #[tokio::test]
    async fn fetch_missing_url_is_invalid_params() {
        let r = run_fetch(json!({})).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn fetch_bad_format_is_invalid_params() {
        let r = run_fetch(json!({ "url": "https://example.com/", "format": "xml" })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn fetch_ssrf_url_is_invalid_url() {
        let r = run_fetch(json!({ "url": "http://127.0.0.1/" })).await;
        assert_eq!(r["code"], "INVALID_URL");
    }

    #[tokio::test]
    async fn fetch_metadata_endpoint_is_invalid_url() {
        let r = run_fetch(json!({ "url": "http://169.254.169.254/latest/meta-data/" })).await;
        assert_eq!(r["status"], "error");
        assert_eq!(r["code"], "INVALID_URL");
    }

    #[tokio::test]
    async fn scrape_bad_selectors_type_is_invalid_params() {
        let r = run_scrape(json!({ "url": "https://example.com/", "selectors": "h1" })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn scrape_ssrf_url_is_invalid_url() {
        let r = run_scrape(json!({ "url": "http://10.0.0.1/" })).await;
        assert_eq!(r["code"], "INVALID_URL");
    }

    #[tokio::test]
    async fn extract_missing_rules_is_invalid_params() {
        let r = run_extract(json!({ "html": "<p>x</p>" })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn extract_neither_url_nor_html_is_invalid_params() {
        let r = run_extract(json!({ "extraction_rules": { "t": { "selector": "p" } } })).await;
        assert_eq!(r["code"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn extract_inline_html_round_trip() {
        let r = run_extract(json!({
            "html": "<h1>Title</h1><a href=\"/x\">link</a>",
            "extraction_rules": {
                "heading": { "selector": "h1" },
                "url": { "selector": "a", "attribute": "href" }
            }
        }))
        .await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["extracted_data"]["heading"], "Title");
        assert_eq!(r["extracted_data"]["url"], "/x");
        assert_eq!(r["fields_extracted"], 2);
    }
}
