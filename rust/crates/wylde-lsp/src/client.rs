//! The rust-analyzer supervisor + LSP client, as a single async actor.
//!
//! A process-wide actor task owns the rust-analyzer child's stdio and all LSP
//! state (request-id correlation, the diagnostics cache, the initialize
//! handshake). Verb handlers ([`crate::service`]) send [`LspCommand`]s over an
//! mpsc channel and await a oneshot reply, so the protocol's async, stateful
//! nature is hidden behind a simple request/response surface.
//!
//! **Optional + graceful:** rust-analyzer is spawned lazily on the first
//! `lsp.open`. If it can't be spawned (not installed) or the handshake fails,
//! the actor records the reason once and every subsequent verb returns a clean
//! `unavailable` error — the editor degrades to plain text + tree-sitter. The
//! service process itself never crashes on a missing language server.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::jsonrpc;

/// How long to wait for the initialize handshake before declaring the server
/// unavailable.
const INIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A command sent to the LSP actor.
pub enum LspCommand {
    /// Liveness/availability snapshot.
    Status(oneshot::Sender<Value>),
    /// Open (or re-open) a document; lazily starts + initializes rust-analyzer
    /// against `root` on the first call.
    Open {
        root: String,
        uri: String,
        language_id: String,
        text: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Full-text document change (full sync).
    Change {
        uri: String,
        version: i64,
        text: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// A request expecting a response (completion / hover). `params` is the
    /// full LSP params object.
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Latest cached diagnostics for a document (from publishDiagnostics).
    Diagnostics {
        uri: String,
        reply: oneshot::Sender<Vec<Value>>,
    },
}

/// The lazily-started actor handle. First access spawns the actor task on the
/// current tokio runtime.
pub fn actor() -> &'static mpsc::UnboundedSender<LspCommand> {
    static ACTOR: OnceLock<mpsc::UnboundedSender<LspCommand>> = OnceLock::new();
    ACTOR.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            Actor::new(rx).run().await;
        });
        tx
    })
}

struct Actor {
    cfg: &'static Config,
    cmd_rx: mpsc::UnboundedReceiver<LspCommand>,
    incoming_tx: mpsc::UnboundedSender<Value>,
    incoming_rx: mpsc::UnboundedReceiver<Value>,
    stdin: Option<ChildStdin>,
    _child: Option<Child>,
    next_id: i64,
    pending: HashMap<i64, oneshot::Sender<Result<Value, String>>>,
    diagnostics: HashMap<String, Vec<Value>>,
    started: bool,
    /// Set once when the server is determined unavailable; never retried.
    unavailable: Option<String>,
}

impl Actor {
    fn new(cmd_rx: mpsc::UnboundedReceiver<LspCommand>) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        Self {
            cfg: Config::get(),
            cmd_rx,
            incoming_tx,
            incoming_rx,
            stdin: None,
            _child: None,
            next_id: 1,
            pending: HashMap::new(),
            diagnostics: HashMap::new(),
            started: false,
            unavailable: None,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(c) => self.handle_cmd(c).await,
                        None => break, // all senders dropped — shut down
                    }
                }
                Some(msg) = self.incoming_rx.recv() => {
                    self.handle_incoming(msg).await;
                }
            }
        }
    }

    async fn handle_cmd(&mut self, cmd: LspCommand) {
        match cmd {
            LspCommand::Status(reply) => {
                let _ = reply.send(json!({
                    "server": "rust-analyzer",
                    "running": self.started,
                    "available": self.started && self.unavailable.is_none(),
                    "unavailable_reason": self.unavailable,
                }));
            }
            LspCommand::Open {
                root,
                uri,
                language_id,
                text,
                reply,
            } => {
                if let Err(e) = self.ensure_started(&root).await {
                    let _ = reply.send(Err(e));  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
                    return;
                }
                let note = jsonrpc::notification(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri, "languageId": language_id, "version": 1, "text": text,
                    }}),
                );
                let r = self.send(&note).await;
                let _ = reply.send(r);  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
            }
            LspCommand::Change {
                uri,
                version,
                text,
                reply,
            } => {
                if !self.started || self.unavailable.is_some() {
                    let _ = reply.send(Err(self.unavailable_msg()));  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
                    return;
                }
                let note = jsonrpc::notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [ { "text": text } ], // full sync
                    }),
                );
                let r = self.send(&note).await;
                let _ = reply.send(r);  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
            }
            LspCommand::Request {
                method,
                params,
                reply,
            } => {
                if !self.started || self.unavailable.is_some() {
                    let _ = reply.send(Err(self.unavailable_msg()));  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                let req = jsonrpc::request(id, &method, params);
                if let Err(e) = self.send(&req).await {
                    let _ = reply.send(Err(e));  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
                    return;
                }
                self.pending.insert(id, reply);
            }
            LspCommand::Diagnostics { uri, reply } => {
                let _ = reply.send(self.diagnostics.get(&uri).cloned().unwrap_or_default());  // best-effort reply; requester may have cancelled (wylde-check: discard-result-ok)
            }
        }
    }

    /// Route an incoming message: a response completes a pending request; a
    /// server→client request gets an empty reply (so rust-analyzer never
    /// blocks on us); `publishDiagnostics` updates the cache.
    async fn handle_incoming(&mut self, msg: Value) {
        let has_id = msg.get("id").map(|v| !v.is_null()).unwrap_or(false);
        let is_method = msg.get("method").and_then(Value::as_str).is_some();

        if has_id && !is_method {
            // Response to one of our requests.
            if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                if let Some(tx) = self.pending.remove(&id) {
                    if let Some(err) = msg.get("error") {
                        let m = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("lsp error")
                            .to_owned();
                        let _ = tx.send(Err(m));  // pending-request channel may be gone (wylde-check: discard-result-ok)
                    } else {
                        let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));  // pending-request channel may be gone (wylde-check: discard-result-ok)
                    }
                }
            }
            return;
        }

        if has_id && is_method {
            // Server→client request — reply empty so RA isn't blocked.
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            // workspace/configuration expects an array sized to its items.
            let result = if method == "workspace/configuration" {
                let n = msg
                    .get("params")
                    .and_then(|p| p.get("items"))
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                Value::Array(vec![Value::Null; n])
            } else {
                Value::Null
            };
            let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let _ = self.send(&resp).await;  // best-effort LSP write; peer may have closed (wylde-check: discard-result-ok)
            return;
        }

        // Notification.
        if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
            if let Some(params) = msg.get("params") {
                if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                    let diags = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    self.diagnostics.insert(uri.to_owned(), diags);
                }
            }
        }
    }

    fn unavailable_msg(&self) -> String {
        self.unavailable
            .clone()
            .unwrap_or_else(|| "rust-analyzer not started".to_owned())
    }

    /// Write a frame to rust-analyzer's stdin.
    async fn send(&mut self, msg: &Value) -> Result<(), String> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.unavailable_msg());
        };
        let frame = jsonrpc::encode(msg);
        stdin
            .write_all(&frame)
            .await
            .map_err(|e| format!("lsp write: {e}"))?;
        stdin.flush().await.map_err(|e| format!("lsp flush: {e}"))?;
        Ok(())
    }

    /// Spawn + initialize rust-analyzer on first use. Idempotent; records an
    /// unavailability reason on failure and never retries.
    async fn ensure_started(&mut self, root: &str) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        if let Some(reason) = &self.unavailable {
            return Err(reason.clone());
        }
        match self.start(root).await {
            Ok(()) => {
                self.started = true;
                Ok(())
            }
            Err(e) => {
                tracing::warn!("wylde-lsp: rust-analyzer unavailable: {e}");
                self.unavailable = Some(e.clone());
                Err(e)
            }
        }
    }

    async fn start(&mut self, root: &str) -> Result<(), String> {
        let mut child = Command::new(&self.cfg.rust_analyzer)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn {:?}: {e}", self.cfg.rust_analyzer))?;

        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);

        let root_uri = path_to_uri(root);
        let init = jsonrpc::request(
            0,
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [ { "uri": root_uri, "name": "workspace" } ],
                "capabilities": client_capabilities(),
            }),
        );

        // Run the handshake under a timeout so a wedged server can't hang us.
        let handshake = async {
            write_frame(&mut stdin, &init).await?;
            // Read until the initialize response (id 0) arrives.
            loop {
                let msg = read_frame(&mut stdout).await?;
                if msg.get("id").and_then(Value::as_i64) == Some(0) {
                    if msg.get("error").is_some() {
                        return Err("initialize returned an error".to_owned());
                    }
                    break;
                }
                // Pre-init notifications/requests are ignored during handshake.
            }
            write_frame(&mut stdin, &jsonrpc::notification("initialized", json!({}))).await?;
            Ok::<(), String>(())
        };
        tokio::time::timeout(INIT_TIMEOUT, handshake)
            .await
            .map_err(|_| "rust-analyzer initialize timed out".to_owned())??;

        // Stash stdin and spawn the reader pump that forwards every subsequent
        // frame to the actor.
        self.stdin = Some(stdin);
        self._child = Some(child);
        let tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            // Pump every frame to the actor until EOF / server death / actor gone.
            while let Ok(msg) = read_frame(&mut stdout).await {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Ok(())
    }
}

/// Minimal client capabilities — enough for completion / hover / diagnostics.
fn client_capabilities() -> Value {
    json!({
        "textDocument": {
            "synchronization": { "didSave": false, "dynamicRegistration": false },
            "completion": {
                "completionItem": { "snippetSupport": false },
                "contextSupport": false
            },
            "hover": { "contentFormat": ["plaintext", "markdown"] },
            "publishDiagnostics": { "relatedInformation": false }
        },
        "workspace": { "configuration": true }
    })
}

/// Convert a filesystem path to a `file://` URI (best-effort, Windows-aware).
pub fn path_to_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        // Windows drive path `C:/...` → `file:///C:/...`.
        format!("file:///{normalized}")
    }
}

async fn write_frame(stdin: &mut ChildStdin, msg: &Value) -> Result<(), String> {
    stdin
        .write_all(&jsonrpc::encode(msg))
        .await
        .map_err(|e| format!("lsp write: {e}"))?;
    stdin.flush().await.map_err(|e| format!("lsp flush: {e}"))
}

async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("lsp read header: {e}"))?;
        if n == 0 {
            return Err("eof".to_owned());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push_str(&line);
    }
    let len =
        jsonrpc::content_length(&headers).ok_or_else(|| "missing Content-Length".to_owned())?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("lsp read body: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| format!("lsp decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_uri_windows_and_posix() {
        assert_eq!(path_to_uri("/home/x/proj"), "file:///home/x/proj");
        assert_eq!(path_to_uri(r"C:\Users\x\proj"), "file:///C:/Users/x/proj");
    }
}
