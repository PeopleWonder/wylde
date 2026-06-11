//! Authenticated REST client for the external n8n daemon.
//!
//! Faithful Rust port of the (deleted) Python `N8N/client.py` — that
//! module's docstrings are the spec for every reply shape here. The
//! contract it pinned, preserved verbatim:
//!
//! * **Auth modes** — API key (`X-N8N-API-KEY`, stateless, preferred)
//!   or login session (`POST /rest/login` with email+password, cookie
//!   kept on the client jar, ONE re-login retry on a mid-session 401).
//!   An API-key 401 is never retried — the key is bad and retrying
//!   won't help. An optional basic-auth pair rides on every request.
//! * **Fail fast without creds** — when neither mode is configured,
//!   every verb returns the structured `auth_not_configured` envelope
//!   instead of attempting the network.
//! * **Never raises on transport errors** — every public verb returns
//!   a JSON object: the documented success shape, or an error envelope
//!   `{"error": …}` with optional `detail` (HTTP body excerpt, ≤500
//!   chars) or `code` (`auth_not_configured` / `not_found`).
//!
//! The HTTP/parse halves are split so the parse half is pure and
//! unit-testable from canned JSON with no live n8n (the wylde-ollama
//! parser-test pattern): each `response_to_*` fn takes `(status, body)`
//! and produces the envelope.

use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::config::AuthConfig;

/// Per-call timeouts, in seconds — the Python client's literals.
const READ_TIMEOUT_S: u64 = 10; // list / get / get_execution / archive / delete
const WRITE_TIMEOUT_S: u64 = 30; // create / edit
const EXECUTE_TIMEOUT_S: u64 = 60; // workflow run
const LOGIN_TIMEOUT_S: u64 = 10;
const HEALTH_TIMEOUT_S: u64 = 3;

/// Python's `resp.text[:500]` detail cap (characters, not bytes).
const DETAIL_CAP_CHARS: usize = 500;

pub struct N8nClient {
    http: reqwest::Client,
    auth: AuthConfig,
    /// Serialises request + re-login, mirroring the Python client's
    /// `_session_lock` around `_request` (the session cookie must not
    /// be refreshed concurrently mid-retry).
    session: Mutex<()>,
}

impl N8nClient {
    /// Build a client over `auth`. The cookie jar replaces Python's
    /// process-global `requests.Session` for login-session mode.
    pub fn new(auth: AuthConfig) -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            // No global timeout — per-call deadlines only (the
            // wylde-ollama upstream pattern).
            .build()
            .expect("n8n reqwest::Client construction failed");
        Self {
            http,
            auth,
            session: Mutex::new(()),
        }
    }

    pub fn auth(&self) -> &AuthConfig {
        &self.auth
    }

    // ── Low-level transport ──────────────────────────────────────────

    /// `POST /rest/login` with email+password; the cookie jar stores
    /// the session. Returns true on 200. Credential rejections
    /// (401/403) log at ERROR — retrying without a config change will
    /// keep failing; transport errors only warn (the next request can
    /// retry).
    async fn login(&self) -> bool {
        if self.auth.email.is_empty() || self.auth.password.is_empty() {
            return false;
        }
        let req = self
            .http
            .post(format!("{}/rest/login", self.auth.url))
            .json(&json!({
                "emailOrLdapLoginId": self.auth.email,
                "password": self.auth.password,
            }))
            .timeout(Duration::from_secs(LOGIN_TIMEOUT_S));
        let req = self.apply_basic_auth(req);
        match req.send().await {
            Ok(r) if r.status().as_u16() == 200 => {
                tracing::info!("authenticated with n8n at {}", self.auth.url);
                true
            }
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                tracing::error!(
                    "n8n rejected credentials: {status} {}",
                    truncate_chars(&body, 200)
                );
                false
            }
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                tracing::warn!("n8n login failed: {status} {}", truncate_chars(&body, 200));
                false
            }
            Err(e) => {
                tracing::warn!("n8n login transport error: {e}");
                false
            }
        }
    }

    /// Authenticated request against n8n. Retries once on 401 when in
    /// session mode (an API-key 401 means the key is bad — no retry).
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout_s: u64,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let _guard = self.session.lock().await;
        let resp = self
            .send_once(method.clone(), path, body, timeout_s)
            .await?;
        if resp.status().as_u16() == 401 && self.auth.api_key.is_empty() {
            tracing::info!("n8n session expired; re-authenticating");
            if self.login().await {
                return self.send_once(method, path, body, timeout_s).await;
            }
        }
        Ok(resp)
    }

    /// Single attempt — no retry, no lock.
    async fn send_once(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout_s: u64,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let url = format!("{}{}", self.auth.url, path);
        let mut req = self
            .http
            .request(method, &url)
            .timeout(Duration::from_secs(timeout_s));
        if !self.auth.api_key.is_empty() {
            req = req.header("X-N8N-API-KEY", &self.auth.api_key);
        }
        req = self.apply_basic_auth(req);
        if let Some(b) = body {
            req = req.json(b);
        }
        req.send().await
    }

    /// Basic-auth pair, when configured — Python set it on the Session
    /// (every request, login included).
    fn apply_basic_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.auth.basic_user.is_empty() || !self.auth.basic_pass.is_empty() {
            req.basic_auth(&self.auth.basic_user, Some(&self.auth.basic_pass))
        } else {
            req
        }
    }

    /// `auth_not_configured` gate — `Some(envelope)` when no credential
    /// mode is wired (the Python `_check_auth`).
    fn check_auth(&self) -> Option<Value> {
        if self.auth.auth_ready() {
            None
        } else {
            Some(auth_not_configured())
        }
    }

    /// Drain a response into the `(status, body_text)` pair the pure
    /// parse fns take. A body-read failure degrades to an empty body —
    /// the status code still drives the envelope.
    async fn status_and_text(resp: reqwest::Response) -> (u16, String) {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    }

    // ── Public verbs (one per Python public function) ────────────────

    /// `{"workflows": [{id, name, active, description}, …], "count": N}`.
    pub async fn list_workflows(&self) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        match self
            .request(
                reqwest::Method::GET,
                "/rest/workflows",
                None,
                READ_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_list_workflows(status, &body)
            }
            Err(e) => transport_error(&e),
        }
    }

    /// Fetch a workflow definition by ID — `{"workflow": …}` or an error.
    pub async fn get_workflow(&self, workflow_id: &str) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        if workflow_id.is_empty() {
            return err_envelope("workflow_id is required");
        }
        match self
            .request(
                reqwest::Method::GET,
                &format!("/rest/workflows/{workflow_id}"),
                None,
                READ_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_get_workflow(status, &body, workflow_id)
            }
            Err(e) => transport_error(&e),
        }
    }

    /// Fetch an execution's status payload by ID — `{"execution": …}`
    /// or an error. Mirrors `get_workflow`: read-only GET, structured
    /// envelope on transport / 404 / non-2xx.
    pub async fn get_execution(&self, execution_id: &str) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        if execution_id.is_empty() {
            return err_envelope("execution_id is required");
        }
        match self
            .request(
                reqwest::Method::GET,
                &format!("/rest/executions/{execution_id}"),
                None,
                READ_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_get_execution(status, &body, execution_id)
            }
            Err(e) => transport_error(&e),
        }
    }

    /// Run a workflow by ID — `{execution_id, status, data}` or an
    /// error. `inputs` is forwarded as the run-time data payload (n8n
    /// wraps it as `{"data": <inputs>}`).
    pub async fn execute_workflow(&self, workflow_id: &str, inputs: Option<Value>) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        if workflow_id.is_empty() {
            return err_envelope("workflow_id is required");
        }
        // n8n workflow IDs are numeric. Fail fast on obvious typos and
        // avoid path-injection surface before the request goes out.
        if !is_numeric_id(workflow_id) {
            return err_envelope("workflow_id must be a numeric string");
        }
        let body = json!({ "data": inputs.unwrap_or_else(|| json!({})) });
        match self
            .request(
                reqwest::Method::POST,
                &format!("/rest/workflows/{workflow_id}/run"),
                Some(&body),
                EXECUTE_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_execute(status, &body)
            }
            Err(e) if e.is_timeout() => err_envelope("Workflow execution timed out"),
            Err(e) => transport_error(&e),
        }
    }

    /// Create a new workflow. `payload` keys: `name` (required),
    /// `nodes`, `connections`, `active`, `settings`. Returns
    /// `{workflow_id, name, active, created_at}` on success.
    pub async fn create_workflow(&self, payload: &Value) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        let body = match build_create_body(payload) {
            Ok(b) => b,
            Err(e) => return e,
        };
        match self
            .request(
                reqwest::Method::POST,
                "/rest/workflows",
                Some(&body),
                WRITE_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_create(status, &body)
            }
            Err(e) => transport_error(&e),
        }
    }

    /// PATCH an existing workflow — only recognised keys present in
    /// `payload` are sent (`name`/`nodes`/`connections`/`active`).
    /// Returns `{workflow_id, name, active, updated_at}` on success.
    pub async fn edit_workflow(&self, workflow_id: &str, payload: &Value) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        if workflow_id.is_empty() {
            return err_envelope("workflow_id is required");
        }
        let body = match build_edit_body(payload) {
            Ok(b) => b,
            Err(e) => return e,
        };
        match self
            .request(
                reqwest::Method::PATCH,
                &format!("/rest/workflows/{workflow_id}"),
                Some(&body),
                WRITE_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_edit(status, &body, workflow_id)
            }
            Err(e) => transport_error(&e),
        }
    }

    /// Permanently delete a workflow. Archives first (n8n requirement);
    /// any archive failure aborts before the DELETE goes out. Returns
    /// `{"deleted": true, "workflow_id": …}` on success.
    pub async fn delete_workflow(&self, workflow_id: &str) -> Value {
        if let Some(e) = self.check_auth() {
            return e;
        }
        if workflow_id.is_empty() {
            return err_envelope("workflow_id is required");
        }
        let archive = match self
            .request(
                reqwest::Method::POST,
                &format!("/rest/workflows/{workflow_id}/archive"),
                Some(&json!({})),
                READ_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                archive_gate(status, &body, workflow_id)
            }
            Err(e) => Some(err_envelope(&format!(
                "transport error during archive: {e}"
            ))),
        };
        if let Some(err) = archive {
            return err;
        }
        match self
            .request(
                reqwest::Method::DELETE,
                &format!("/rest/workflows/{workflow_id}"),
                None,
                READ_TIMEOUT_S,
            )
            .await
        {
            Ok(resp) => {
                let (status, body) = Self::status_and_text(resp).await;
                response_to_delete(status, &body, workflow_id)
            }
            Err(e) => err_envelope(&format!("transport error during delete: {e}")),
        }
    }

    /// `{auth_configured, url, reachable}` — the `n8n.health` payload.
    /// `reachable` is a quick unauthenticated GET against the base URL;
    /// any HTTP answer counts (even a 404 proves something is serving
    /// the port), transport failure fail-softs to `false`.
    pub async fn health(&self) -> Value {
        let reachable = self
            .http
            .get(format!("{}/", self.auth.url))
            .timeout(Duration::from_secs(HEALTH_TIMEOUT_S))
            .send()
            .await
            .is_ok();
        json!({
            "auth_configured": self.auth.auth_ready(),
            "url": self.auth.url,
            "reachable": reachable,
        })
    }
}

// ── Error envelopes ──────────────────────────────────────────────────

/// `{"error": message}` — the base envelope every failure path uses.
pub fn err_envelope(message: &str) -> Value {
    json!({ "error": message })
}

/// The fail-fast no-credentials envelope (`code: auth_not_configured`).
pub fn auth_not_configured() -> Value {
    json!({
        "error": "n8n auth not configured (set WYLDE_N8N_API_KEY or \
                  WYLDE_N8N_EMAIL+WYLDE_N8N_PASSWORD)",
        "code": "auth_not_configured",
    })
}

fn transport_error(e: &reqwest::Error) -> Value {
    err_envelope(&format!("transport error: {e}"))
}

fn http_error(status: u16, body: &str) -> Value {
    json!({
        "error": format!("n8n returned HTTP {status}"),
        "detail": truncate_chars(body, DETAIL_CAP_CHARS),
    })
}

fn not_found(message: String) -> Value {
    json!({ "error": message, "code": "not_found" })
}

/// Python's `text[:N]` — a character slice, kept char-based (not byte)
/// so multi-byte bodies never split a code point.
fn truncate_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// The Python guard was `str.isdigit()` — non-empty, every char a digit.
fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Parse a response body as JSON. The Python client called
/// `resp.json()` unguarded; an unparsable body there escaped as an
/// exception, which this port is forbidden to do — it degrades to a
/// structured envelope instead (the one deliberate divergence).
fn parse_json(body: &str) -> Result<Value, Value> {
    serde_json::from_str(body).map_err(|_| {
        json!({
            "error": "n8n returned invalid JSON",
            "detail": truncate_chars(body, DETAIL_CAP_CHARS),
        })
    })
}

/// n8n wraps most payloads as `{"data": …}` — unwrap when present,
/// pass through otherwise (the Python `payload.get("data", payload)`).
fn unwrap_data(v: Value) -> Value {
    match v {
        Value::Object(mut m) => m.remove("data").unwrap_or(Value::Object(m)),
        other => other,
    }
}

// ── Pure response → envelope shaping (unit-tested from canned JSON) ──

pub(crate) fn response_to_list_workflows(status: u16, body: &str) -> Value {
    if status != 200 {
        return http_error(status, body);
    }
    let payload = match parse_json(body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let workflows = match unwrap_data(payload) {
        Value::Array(a) => a,
        _ => Vec::new(),
    };
    let count = workflows.len();
    let rows: Vec<Value> = workflows
        .iter()
        .map(|w| {
            json!({
                "id": w.get("id").map(value_to_id_string).unwrap_or_else(|| "None".into()),
                "name": w.get("name").cloned().unwrap_or(Value::Null),
                "active": w.get("active").and_then(Value::as_bool).unwrap_or(false),
                "description": w.get("description").cloned().unwrap_or_else(|| json!("")),
            })
        })
        .collect();
    json!({ "workflows": rows, "count": count })
}

pub(crate) fn response_to_get_workflow(status: u16, body: &str, workflow_id: &str) -> Value {
    if status == 404 {
        return not_found(format!("Workflow '{workflow_id}' not found"));
    }
    if status != 200 {
        return http_error(status, body);
    }
    match parse_json(body) {
        Ok(v) => json!({ "workflow": unwrap_data(v) }),
        Err(e) => e,
    }
}

pub(crate) fn response_to_get_execution(status: u16, body: &str, execution_id: &str) -> Value {
    if status == 404 {
        return not_found(format!("Execution '{execution_id}' not found"));
    }
    if status != 200 {
        return http_error(status, body);
    }
    match parse_json(body) {
        Ok(v) => json!({ "execution": unwrap_data(v) }),
        Err(e) => e,
    }
}

pub(crate) fn response_to_execute(status: u16, body: &str) -> Value {
    if status != 200 {
        return http_error(status, body);
    }
    let result = match parse_json(body) {
        Ok(v) => unwrap_data(v),
        Err(e) => return e,
    };
    json!({
        "execution_id": result.get("executionId").cloned().unwrap_or(Value::Null),
        "status": result.get("status").cloned().unwrap_or_else(|| json!("completed")),
        "data": result.get("data").cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn response_to_create(status: u16, body: &str) -> Value {
    if !matches!(status, 200 | 201) {
        return http_error(status, body);
    }
    let w = match parse_json(body) {
        Ok(v) => unwrap_data(v),
        Err(e) => return e,
    };
    json!({
        "workflow_id": w.get("id").map(value_to_id_string).map(Value::String).unwrap_or(Value::Null),
        "name": w.get("name").cloned().unwrap_or(Value::Null),
        "active": w.get("active").and_then(Value::as_bool).unwrap_or(false),
        "created_at": w.get("createdAt").cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn response_to_edit(status: u16, body: &str, workflow_id: &str) -> Value {
    if status == 404 {
        return not_found(format!("Workflow '{workflow_id}' not found"));
    }
    if status != 200 {
        return http_error(status, body);
    }
    let w = match parse_json(body) {
        Ok(v) => unwrap_data(v),
        Err(e) => return e,
    };
    let id = w
        .get("id")
        .map(value_to_id_string)
        .unwrap_or_else(|| workflow_id.to_owned());
    json!({
        "workflow_id": id,
        "name": w.get("name").cloned().unwrap_or(Value::Null),
        "active": w.get("active").and_then(Value::as_bool).unwrap_or(false),
        "updated_at": w.get("updatedAt").cloned().unwrap_or(Value::Null),
    })
}

/// Archive step of the delete sequence — `Some(error)` aborts before
/// the DELETE goes out, `None` proceeds.
pub(crate) fn archive_gate(status: u16, body: &str, workflow_id: &str) -> Option<Value> {
    match status {
        404 => Some(not_found(format!("Workflow '{workflow_id}' not found"))),
        200 | 201 => None,
        other => Some(json!({
            "error": format!("Failed to archive workflow: HTTP {other}"),
            "detail": truncate_chars(body, DETAIL_CAP_CHARS),
        })),
    }
}

pub(crate) fn response_to_delete(status: u16, body: &str, workflow_id: &str) -> Value {
    match status {
        404 => not_found(format!("Workflow '{workflow_id}' not found after archive")),
        200 | 204 => json!({ "deleted": true, "workflow_id": workflow_id }),
        other => http_error(other, body),
    }
}

/// Mirror Python's `str(w.get("id"))` — n8n ids arrive as numbers or
/// strings depending on version; both stringify.
fn value_to_id_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

// ── Request-body builders (pure; validation lives here) ─────────────

/// Create body — `name` required; `nodes`/`connections`/`active`/
/// `settings` take the Python defaults when absent.
pub(crate) fn build_create_body(payload: &Value) -> Result<Value, Value> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        return Err(err_envelope("name is required"));
    };
    Ok(json!({
        "name": name,
        "nodes": payload.get("nodes").cloned().unwrap_or_else(|| json!([])),
        "connections": payload.get("connections").cloned().unwrap_or_else(|| json!({})),
        "active": payload.get("active").cloned().unwrap_or(json!(false)),
        "settings": payload.get("settings").cloned().unwrap_or_else(|| json!({})),
    }))
}

/// Edit body — PATCH only the recognised keys actually provided
/// (non-null), with the "No updatable fields provided" guard.
pub(crate) fn build_edit_body(payload: &Value) -> Result<Value, Value> {
    let Value::Object(map) = payload else {
        return Err(err_envelope("payload must be a dict"));
    };
    const ALLOWED: [&str; 4] = ["name", "nodes", "connections", "active"];
    let mut body = Map::new();
    for key in ALLOWED {
        if let Some(v) = map.get(key) {
            if !v.is_null() {
                body.insert(key.to_owned(), v.clone());
            }
        }
    }
    if body.is_empty() {
        return Err(err_envelope("No updatable fields provided"));
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_key_auth(url: &str) -> AuthConfig {
        AuthConfig {
            url: url.trim_end_matches('/').to_owned(),
            api_key: "test-key".into(),
            ..Default::default()
        }
    }

    // ── Envelope + validation (pure) ─────────────────────────────────

    #[test]
    fn auth_not_configured_envelope_carries_code() {
        let e = auth_not_configured();
        assert_eq!(e["code"], "auth_not_configured");
        assert!(e["error"].as_str().unwrap().contains("WYLDE_N8N_API_KEY"));
    }

    #[test]
    fn numeric_id_guard_matches_python_isdigit() {
        assert!(is_numeric_id("42"));
        assert!(!is_numeric_id(""));
        assert!(!is_numeric_id("4a"));
        assert!(!is_numeric_id("-1"));
        assert!(!is_numeric_id("1.5"));
    }

    #[test]
    fn truncate_chars_is_char_based() {
        let s = "é".repeat(600);
        assert_eq!(truncate_chars(&s, 500).chars().count(), 500);
    }

    #[test]
    fn build_create_body_requires_name_and_fills_defaults() {
        let err = build_create_body(&json!({})).unwrap_err();
        assert_eq!(err["error"], "name is required");
        let body = build_create_body(&json!({"name": "wf"})).unwrap();
        assert_eq!(body["name"], "wf");
        assert_eq!(body["nodes"], json!([]));
        assert_eq!(body["connections"], json!({}));
        assert_eq!(body["active"], json!(false));
        assert_eq!(body["settings"], json!({}));
    }

    #[test]
    fn build_edit_body_patches_only_provided_keys() {
        let body = build_edit_body(&json!({"name": "renamed", "bogus": 1})).unwrap();
        assert_eq!(body, json!({"name": "renamed"}));
        // Null values are "not provided" (Python's `v is not None` filter).
        let err = build_edit_body(&json!({"name": null})).unwrap_err();
        assert_eq!(err["error"], "No updatable fields provided");
        let err = build_edit_body(&json!({})).unwrap_err();
        assert_eq!(err["error"], "No updatable fields provided");
        let err = build_edit_body(&json!("not a dict")).unwrap_err();
        assert_eq!(err["error"], "payload must be a dict");
    }

    // ── Response parsing from canned JSON (no live n8n) ──────────────

    #[test]
    fn list_workflows_shapes_rows_and_count() {
        let body = r#"{"data": [
            {"id": 1, "name": "A", "active": true},
            {"id": "2", "name": "B", "active": false, "description": "two"}
        ]}"#;
        let out = response_to_list_workflows(200, body);
        assert_eq!(out["count"], 2);
        assert_eq!(out["workflows"][0]["id"], "1");
        assert_eq!(out["workflows"][0]["active"], true);
        assert_eq!(out["workflows"][0]["description"], "");
        assert_eq!(out["workflows"][1]["id"], "2");
        assert_eq!(out["workflows"][1]["description"], "two");
    }

    #[test]
    fn list_workflows_tolerates_undata_wrapped_and_non_list() {
        // Bare list (no {"data": …} wrapper).
        let out = response_to_list_workflows(200, r#"[{"id": 3, "name": "C"}]"#);
        assert_eq!(out["count"], 1);
        // Non-list payload → empty result, not a crash.
        let out = response_to_list_workflows(200, r#"{"data": {"odd": true}}"#);
        assert_eq!(out["count"], 0);
        assert_eq!(out["workflows"], json!([]));
    }

    #[test]
    fn non_200_produces_http_error_with_truncated_detail() {
        let long_body = "x".repeat(800);
        let out = response_to_list_workflows(503, &long_body);
        assert_eq!(out["error"], "n8n returned HTTP 503");
        assert_eq!(out["detail"].as_str().unwrap().len(), 500);
    }

    #[test]
    fn invalid_json_degrades_to_envelope_not_panic() {
        let out = response_to_list_workflows(200, "<html>oops</html>");
        assert_eq!(out["error"], "n8n returned invalid JSON");
    }

    #[test]
    fn get_workflow_maps_404_to_not_found() {
        let out = response_to_get_workflow(404, "", "7");
        assert_eq!(out["code"], "not_found");
        assert!(out["error"].as_str().unwrap().contains("'7'"));
        let out = response_to_get_workflow(200, r#"{"data": {"id": 7, "name": "wf"}}"#, "7");
        assert_eq!(out["workflow"]["name"], "wf");
    }

    #[test]
    fn get_execution_mirrors_get_workflow() {
        let out = response_to_get_execution(404, "", "9");
        assert_eq!(out["code"], "not_found");
        assert!(out["error"].as_str().unwrap().contains("Execution"));
        let out =
            response_to_get_execution(200, r#"{"data": {"id": 9, "status": "success"}}"#, "9");
        assert_eq!(out["execution"]["status"], "success");
    }

    #[test]
    fn execute_reply_carries_execution_id_status_data() {
        let body = r#"{"data": {"executionId": "55", "status": "running", "data": {"x": 1}}}"#;
        let out = response_to_execute(200, body);
        assert_eq!(out["execution_id"], "55");
        assert_eq!(out["status"], "running");
        assert_eq!(out["data"]["x"], 1);
        // Missing status defaults to "completed" (the Python default).
        let out = response_to_execute(200, r#"{"data": {"executionId": "56"}}"#);
        assert_eq!(out["status"], "completed");
    }

    #[test]
    fn create_accepts_200_and_201() {
        let body = r#"{"data": {"id": 12, "name": "new", "active": false, "createdAt": "t"}}"#;
        for status in [200u16, 201] {
            let out = response_to_create(status, body);
            assert_eq!(out["workflow_id"], "12");
            assert_eq!(out["created_at"], "t");
        }
        let out = response_to_create(400, "bad");
        assert_eq!(out["error"], "n8n returned HTTP 400");
    }

    #[test]
    fn edit_falls_back_to_caller_id_when_reply_omits_it() {
        let out = response_to_edit(200, r#"{"data": {"name": "wf", "updatedAt": "u"}}"#, "33");
        assert_eq!(out["workflow_id"], "33");
        assert_eq!(out["updated_at"], "u");
        let out = response_to_edit(404, "", "33");
        assert_eq!(out["code"], "not_found");
    }

    #[test]
    fn delete_sequence_archive_gate_then_delete() {
        // Archive 404 → not_found, no delete.
        let gate = archive_gate(404, "", "5").unwrap();
        assert_eq!(gate["code"], "not_found");
        // Archive non-2xx → abort with the archive error.
        let gate = archive_gate(500, "boom", "5").unwrap();
        assert!(gate["error"]
            .as_str()
            .unwrap()
            .contains("Failed to archive"));
        // Archive ok (200 or 201) → proceed.
        assert!(archive_gate(200, "", "5").is_none());
        assert!(archive_gate(201, "", "5").is_none());
        // Delete 200/204 → deleted; 404 → not_found-after-archive.
        for status in [200u16, 204] {
            let out = response_to_delete(status, "", "5");
            assert_eq!(out["deleted"], true);
            assert_eq!(out["workflow_id"], "5");
        }
        let out = response_to_delete(404, "", "5");
        assert!(out["error"].as_str().unwrap().contains("after archive"));
    }

    // ── Auth-mode behaviour over a mock server ───────────────────────

    #[tokio::test]
    async fn no_creds_fails_fast_without_touching_the_network() {
        // Point at a URL that would explode if dialled — the auth gate
        // must answer first.
        let client = N8nClient::new(AuthConfig {
            url: "http://127.0.0.1:1".into(),
            ..Default::default()
        });
        let out = client.list_workflows().await;
        assert_eq!(out["code"], "auth_not_configured");
        let out = client.delete_workflow("1").await;
        assert_eq!(out["code"], "auth_not_configured");
    }

    #[tokio::test]
    async fn execute_rejects_non_numeric_id_before_any_request() {
        let client = N8nClient::new(api_key_auth("http://127.0.0.1:1"));
        let out = client.execute_workflow("../../etc", None).await;
        assert_eq!(out["error"], "workflow_id must be a numeric string");
    }

    #[tokio::test]
    async fn transport_failure_is_an_envelope_not_a_panic() {
        // Bind + drop a port so nothing is listening.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = N8nClient::new(api_key_auth(&format!("http://127.0.0.1:{port}")));
        let out = client.list_workflows().await;
        assert!(
            out["error"].as_str().unwrap().contains("transport error")
                || out["error"].as_str().unwrap().contains("timed out"),
            "got {out}"
        );
    }

    #[tokio::test]
    async fn api_key_rides_every_request_and_401_is_not_retried() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // Expect exactly ONE request: an API-key 401 must not re-login.
        Mock::given(method("GET"))
            .and(path("/rest/workflows"))
            .and(header("X-N8N-API-KEY", "test-key"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .expect(1)
            .mount(&server)
            .await;
        let client = N8nClient::new(api_key_auth(&server.uri()));
        let out = client.list_workflows().await;
        assert_eq!(out["error"], "n8n returned HTTP 401");
        server.verify().await;
    }

    #[tokio::test]
    async fn session_mode_relogs_in_once_on_401() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // First GET → 401 (session expired); login → 200 with cookie;
        // retried GET → 200. wiremock serves mounts in order for
        // distinct matchers; use a counter via up_to_n_times.
        Mock::given(method("GET"))
            .and(path("/rest/workflows"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", "n8n-auth=abc; Path=/; HttpOnly")
                    .set_body_json(json!({"data": {"id": "user"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/workflows"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": 1, "name": "A"}]})),
            )
            .mount(&server)
            .await;
        let client = N8nClient::new(AuthConfig {
            url: server.uri().trim_end_matches('/').to_owned(),
            email: "a@b.c".into(),
            password: "pw".into(),
            ..Default::default()
        });
        let out = client.list_workflows().await;
        assert_eq!(out["count"], 1, "got {out}");
        server.verify().await;
    }

    #[tokio::test]
    async fn health_reports_reachability_and_auth_flag() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("n8n is up"))
            .mount(&server)
            .await;
        let client = N8nClient::new(api_key_auth(&server.uri()));
        let out = client.health().await;
        assert_eq!(out["auth_configured"], true);
        assert_eq!(out["reachable"], true);

        // Down upstream → reachable=false, never an error. No creds →
        // auth_configured=false (health itself never auth-gates).
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = N8nClient::new(AuthConfig {
            url: format!("http://127.0.0.1:{port}"),
            ..Default::default()
        });
        let out = client.health().await;
        assert_eq!(out["auth_configured"], false);
        assert_eq!(out["reachable"], false);
    }
}
