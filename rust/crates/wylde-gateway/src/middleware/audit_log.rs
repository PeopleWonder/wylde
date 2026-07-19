//! Audit log — JSONL writer for ingress + egress activity.
//!
//! Rust port of `Gateway/middleware/audit_log.py`. Two streams, both JSONL:
//!
//! * `logs/gateway.jsonl` — one record per HTTP request. Captures
//!   request id, method, path, status, duration, client ip.
//! * `logs/egress.jsonl`  — one record per outbound call. The egress
//!   client (wave 2+) emits these via [`emit_egress`] so they don't need
//!   a Request object on hand.
//!
//! Writes are append-only with a process-shared lock to avoid
//! interleaving. The directory is created on first write; if the writer
//! cannot open the file (permissions, full disk) it logs a single
//! warning and silently drops further records — losing the audit log
//! must never break the request path.
//!
//! Wire-format parity with Python is on every record field: `ts`, `rid`,
//! `method`, `path`, `status`, `client`, `dur_ms`. The duration is
//! rounded to three decimals exactly like the Python implementation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::extract::{ConnectInfo, Request};
use axum::response::Response;
use chrono::Utc;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tower::{Layer, Service};

use wylde_shared::logging::RotatingLog;

use crate::middleware::trace::RequestId;
use crate::settings::get_settings;

const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Lazy-opened JSONL writer for a single audit stream.
///
/// Backed by a [`RotatingLog`], so `gateway.jsonl` / `egress.jsonl`
/// inherit the shared size + retention policy — no per-file cap lives
/// here. The writer keeps the port's warn-once semantics: a failed write
/// logs a single warning and further failures are silent, so losing the
/// audit log never breaks the request path.
pub struct JsonlWriter {
    sink: RotatingLog,
    failed_once: AtomicBool,
}

impl JsonlWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            sink: RotatingLog::new(path),
            failed_once: AtomicBool::new(false),
        }
    }

    /// Append one JSON record. Adds `ts` if absent.
    pub fn emit(&self, mut record: Value) {
        if record.get("ts").is_none() {
            if let Some(map) = record.as_object_mut() {
                map.insert("ts".into(), Value::String(now_iso()));
            }
        }
        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                self.warn_once(&format!("serialize: {e}"));
                return;
            }
        };
        if let Err(e) = self.write_line(&line) {
            self.warn_once(&format!("write: {e}"));
        }
    }

    fn write_line(&self, line: &str) -> std::io::Result<()> {
        self.sink.write_line(line)
    }

    fn warn_once(&self, msg: &str) {
        if !self.failed_once.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "audit: write to {} failed: {} — silenced",
                self.sink.path().display(),
                msg
            );
        }
    }

    fn close(&self) {
        self.sink.close();
    }
}

type WriterMap = HashMap<String, Arc<JsonlWriter>>;

fn writers() -> &'static Mutex<WriterMap> {
    static W: OnceLock<Mutex<WriterMap>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_iso() -> String {
    Utc::now().format(TIME_FORMAT).to_string()
}

/// Return the JSONL writer for `name` (defaults to the request log).
///
/// Writers are cached by absolute path so two different `name` values
/// pointing at the same file share the same lock and file handle.
pub fn get_audit_logger(name: &str) -> Arc<JsonlWriter> {
    let settings = get_settings();
    let path = settings.audit_log_dir.join(format!("{name}.jsonl"));
    get_or_create_writer(path)
}

/// Get-or-create the writer for an absolute path. Used by the
/// middleware (which already knows the path from its captured settings
/// snapshot) so it never has to consult [`get_settings`] at request
/// time.
pub fn get_or_create_writer(path: PathBuf) -> Arc<JsonlWriter> {
    let key = path.to_string_lossy().into_owned();
    let mut map = writers().lock().expect("audit writers poisoned");
    if let Some(w) = map.get(&key) {
        return w.clone();
    }
    let w = Arc::new(JsonlWriter::new(path));
    map.insert(key, w.clone());
    w
}

/// Append a record to the egress audit log (called by the egress client).
pub fn emit_egress(record: Value) {
    if !get_settings().audit_log_enabled {
        return;
    }
    get_audit_logger("egress").emit(record);
}

/// Close + drop cached writers — for tests that change the log dir.
pub fn reset_audit_writers() {
    if let Ok(mut map) = writers().lock() {
        for w in map.values() {
            w.close();
        }
        map.clear();
    }
}

// ── Tower middleware ──────────────────────────────────────────────────

/// Tower layer wrapper. The `enabled` flag and `dir` are captured at
/// build time rather than read from the global settings cache — that
/// keeps the layer decoupled from the process-wide env-var state, which
/// matters both for parallel tests and for hypothetical mid-run
/// settings reloads.
#[derive(Clone)]
pub struct AuditLogLayer {
    enabled: bool,
    dir: PathBuf,
}

impl AuditLogLayer {
    /// Build a layer that reads its enable flag + log dir from the
    /// current [`crate::settings::get_settings`] snapshot.
    pub fn from_current_settings() -> Self {
        let s = get_settings();
        Self {
            enabled: s.audit_log_enabled,
            dir: s.audit_log_dir,
        }
    }

    /// Build a layer with an explicit enable flag + log dir. Useful for
    /// tests that want to keep the layer fully out of the request path.
    pub fn new(enabled: bool, dir: PathBuf) -> Self {
        Self { enabled, dir }
    }
}

impl<S> Layer<S> for AuditLogLayer {
    type Service = AuditLogMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuditLogMiddleware {
            inner,
            enabled: self.enabled,
            dir: self.dir.clone(),
        }
    }
}

/// The actual middleware service.
#[derive(Clone)]
pub struct AuditLogMiddleware<S> {
    inner: S,
    enabled: bool,
    dir: PathBuf,
}

impl<S> Service<Request> for AuditLogMiddleware<S>
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

    fn call(&mut self, req: Request) -> Self::Future {
        if !self.enabled {
            let clone = self.inner.clone();
            let mut inner = std::mem::replace(&mut self.inner, clone);
            return Box::pin(async move { inner.call(req).await });
        }

        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let rid = req
            .extensions()
            .get::<RequestId>()
            .map(|r| r.0.clone())
            .unwrap_or_default();
        let client = req
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_default();

        let log_path = self.dir.join("gateway.jsonl");
        let writer = get_or_create_writer(log_path);

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let t0 = Instant::now();
            let result = inner.call(req).await;
            let dur_ms = (t0.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0;
            let status = match &result {
                Ok(r) => r.status().as_u16(),
                Err(_) => 0,
            };
            writer.emit(json!({
                "rid": rid,
                "method": method,
                "path": path,
                "status": status,
                "client": client,
                "dur_ms": dur_ms,
            }));
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn redirect_logs(tmp: &TempDir) {
        std::env::set_var("WYLDE_GATEWAY_AUDIT_LOG_DIR", tmp.path());
        std::env::set_var("WYLDE_GATEWAY_AUDIT_LOG_ENABLED", "true");
        crate::reset_settings_cache();
        reset_audit_writers();
    }

    #[test]
    fn writes_one_jsonl_record_per_emit() {
        let _g = ENV_LOCK.lock().expect("env lock");
        let tmp = TempDir::new().unwrap();
        redirect_logs(&tmp);

        let w = get_audit_logger("gateway");
        w.emit(json!({"rid": "r1", "status": 200}));
        w.emit(json!({"rid": "r2", "status": 500}));

        let body = std::fs::read_to_string(tmp.path().join("gateway.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["rid"], "r1");
        assert_eq!(parsed["status"], 200);
        assert!(parsed["ts"].is_string());
    }

    #[test]
    fn emit_egress_respects_disabled_flag() {
        let _g = ENV_LOCK.lock().expect("env lock");
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_GATEWAY_AUDIT_LOG_DIR", tmp.path());
        std::env::set_var("WYLDE_GATEWAY_AUDIT_LOG_ENABLED", "false");
        crate::reset_settings_cache();
        reset_audit_writers();

        emit_egress(json!({"dest": "openai", "status": 200}));

        assert!(!tmp.path().join("egress.jsonl").exists());
    }
}
