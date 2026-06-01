//! Line-delimited JSON-RPC framing over a child process's stdin/stdout.
//!
//! MCP convention (2025-11-25): each JSON-RPC message is a single line
//! of UTF-8 JSON on stdout. Stderr is reserved for logs and is never
//! parsed as a frame. A misbehaving server that prints non-JSON to
//! stdout corrupts the stream; we log the bad line and continue
//! rather than crash.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};

use super::wire::{Notification, Request, Response, RpcError};

/// One in-flight request awaiting its `Response`.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// Stdio-transported MCP connection. Spawns one reader task per
/// connection that demultiplexes incoming responses to their
/// awaiting [`send_request`] caller.
pub struct StdioConn {
    pub child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<Mutex<u64>>,
    /// Handle of the spawned reader task — aborted on drop.
    reader_handle: tokio::task::JoinHandle<()>,
}

impl StdioConn {
    /// Attach to `child`'s stdio. Starts the reader loop.
    pub fn attach(mut child: Child) -> Result<Self> {
        let stdin = child.stdin.take().context("child has no stdin")?;
        let stdout = child.stdout.take().context("child has no stdout")?;
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_task = pending.clone();
        let reader_handle = tokio::spawn(reader_loop(stdout, pending_for_task));
        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: Arc::new(Mutex::new(1)),
            reader_handle,
        })
    }

    /// Allocate the next request id.
    pub async fn next_id(&self) -> u64 {
        let mut g = self.next_id.lock().await;
        let id = *g;
        *g = g.wrapping_add(1);
        id
    }

    /// Send a JSON-RPC request and await its response. Returns the
    /// raw response (caller decides whether `result` or `error` is
    /// populated).
    pub async fn send_request(&self, req: Request) -> Result<Response> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(req.id, tx);
        }
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .context("write request to child stdin")?;
            // Flush failure is benign — the next write will retry the
            // buffer, and the response timeout catches a stuck pipe.
            let _ = stdin.flush().await; // wylde-check: discard-result-ok
        }
        let resp = rx
            .await
            .map_err(|_| anyhow!("child closed before responding to id {}", req.id))?;
        Ok(resp)
    }

    /// Send a notification (no response expected).
    pub async fn send_notification(&self, note: Notification) -> Result<()> {
        let mut line = serde_json::to_string(&note)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("write notification to child stdin")?;
        let _ = stdin.flush().await; // wylde-check: discard-result-ok
        Ok(())
    }

    /// Drop the reader task, close stdin, kill the child.
    pub async fn shutdown(mut self) {
        // Close stdin so a well-behaved server can exit cleanly.
        // Inner stdin is wrapped in Arc<Mutex>; drop our handle by
        // taking it and letting it fall out of scope.
        {
            let mut stdin = self.stdin.lock().await;
            // Stdin already closed = success for our purposes.
            let _ = stdin.shutdown().await; // wylde-check: discard-result-ok
        }
        // Give it a moment to exit on its own, then SIGKILL.
        let kill_window = std::time::Duration::from_secs(2);
        let waiter = self.child.wait();
        // Timeout-Elapsed is the "child didn't exit cleanly" path we
        // fall through to start_kill on — not an error.
        let _ = tokio::time::timeout(kill_window, waiter).await; // wylde-check: discard-result-ok
        // start_kill errors only if the child already exited.
        let _ = self.child.start_kill(); // wylde-check: discard-result-ok
        self.reader_handle.abort();
        // Final reap; failure here just means the child was already
        // reaped by start_kill above.
        let _ = self.child.wait().await; // wylde-check: discard-result-ok
    }
}

async fn reader_loop(stdout: ChildStdout, pending: PendingMap) {
    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                tracing::debug!("mcp stdio reader: EOF");
                break;
            }
            Ok(_) => {
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                // Drop server-side log lines that aren't valid JSON.
                let Ok(resp) = serde_json::from_str::<Response>(line) else {
                    tracing::warn!("mcp stdio: ignoring non-JSON line: {}", truncate(line, 200));
                    continue;
                };
                // Drop notifications-from-server (no id) on the floor for
                // this minimal client; we don't subscribe to anything.
                let Some(id_val) = resp.id.as_ref() else {
                    continue;
                };
                let Some(id) = id_val.as_u64() else {
                    tracing::warn!("mcp stdio: response id {:?} not a u64", id_val);
                    continue;
                };
                let waker = {
                    let mut p = pending.lock().await;
                    p.remove(&id)
                };
                if let Some(w) = waker {
                    // Receiver dropped = caller aborted; nothing to do.
                    let _ = w.send(resp); // wylde-check: discard-result-ok
                } else {
                    tracing::warn!("mcp stdio: orphan response id={}", id);
                }
            }
            Err(e) => {
                tracing::warn!("mcp stdio: read error: {}", e);
                break;
            }
        }
    }
    // EOF — fail every still-waiting requester so they don't deadlock.
    let mut p = pending.lock().await;
    for (id, tx) in p.drain() {
        let resp = Response {
            jsonrpc: super::wire::JSONRPC_VERSION.to_owned(),
            id: Some(serde_json::json!(id)),
            result: None,
            error: Some(RpcError {
                code: -32000,
                message: "child stdout closed".into(),
                data: None,
            }),
        };
        // Receiver dropped = caller aborted; nothing to do.
        let _ = tx.send(resp); // wylde-check: discard-result-ok
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..n])
    }
}
