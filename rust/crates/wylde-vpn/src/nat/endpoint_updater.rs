//! Endpoint poller — periodic STUN probe + change notification.
//! Port of `Wylde/VPN/nat/endpoint_updater.py`.
//!
//! Runs as a long-lived `tokio::task`. On every tick (default 300s,
//! configurable via the constructor) it does a cheap STUN discovery.
//! If the mapped endpoint differs from the previous tick's result:
//!
//! * The change is appended to `endpoint-history.json` under
//!   `LINK_DATA_DIR` (last 100 entries kept, matching Python).
//! * The supplied `on_change(previous, current)` callback fires.
//!
//! **Phase 2.D dependency.** Python's callback broadcasts a push
//! notification to every paired peer via `peers/push.py::broadcast`.
//! The Rust push surface lands in Phase 2.D — this module wires the
//! callback shape so 2.D can drop it in without touching the
//! updater. Until then, [`EndpointUpdater::start`] callers (currently
//! just `main.rs`) provide a logging-only callback.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::stun::{self, Transport, UdpTransport};

const HISTORY_CAP: usize = 100;

/// Callback signature used by [`EndpointUpdater`] on every endpoint
/// change. `previous` is `None` for the very first probe (no prior
/// value to compare against).
pub type OnChange = Arc<dyn Fn(Option<&str>, &str) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    timestamp: String,
    previous: String,
    current: String,
}

pub struct EndpointUpdater {
    inner: Arc<Inner>,
}

struct Inner {
    stun_servers: Vec<String>,
    interval: Duration,
    history_path: PathBuf,
    on_change: OnChange,
    transport: Arc<dyn Transport>,
    current: Mutex<Option<String>>,
    stop: Notify,
}

impl EndpointUpdater {
    pub fn new(
        stun_servers: Vec<String>,
        interval: Duration,
        history_path: PathBuf,
        on_change: OnChange,
    ) -> Self {
        Self::with_transport(
            stun_servers,
            interval,
            history_path,
            on_change,
            Arc::new(UdpTransport),
        )
    }

    pub fn with_transport(
        stun_servers: Vec<String>,
        interval: Duration,
        history_path: PathBuf,
        on_change: OnChange,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                stun_servers,
                interval,
                history_path,
                on_change,
                transport,
                current: Mutex::new(None),
                stop: Notify::new(),
            }),
        }
    }

    /// Last observed endpoint, or `None` before the first probe.
    pub fn current(&self) -> Option<String> {
        self.inner.current.lock().unwrap().clone()
    }

    /// Trigger graceful shutdown. The next tick (or the in-flight tick
    /// after this returns) breaks the loop.
    pub fn stop(&self) {
        self.inner.stop.notify_waiters();
    }

    /// Spawn the polling loop on the current tokio runtime. The returned
    /// `JoinHandle` lets callers wait for the task to settle on shutdown.
    pub fn start(&self) -> JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            // First tick fires immediately, then on a fixed interval.
            // Matches Python's `_loop` (tick → wait).
            loop {
                Self::tick(&inner).await;
                tokio::select! {
                    _ = inner.stop.notified() => break,
                    _ = tokio::time::sleep(inner.interval) => {}
                }
            }
        })
    }

    /// Single tick — exposed for tests so the matrix can drive timing
    /// deterministically.
    pub async fn tick_once(&self) {
        Self::tick(&self.inner).await;
    }

    async fn tick(inner: &Arc<Inner>) {
        let servers = inner.stun_servers.clone();
        let transport = Arc::clone(&inner.transport);
        // STUN probe synchronously inside a blocking task — the
        // hand-rolled udp_probe is plain std::net::UdpSocket.
        let probe = tokio::task::spawn_blocking(move || {
            stun::discover_endpoint_with(
                transport.as_ref(),
                &servers,
                0,
                Duration::from_secs(3),
            )
        })
        .await
        .ok()
        .flatten();
        let Some(res) = probe else {
            return;
        };
        let endpoint = match endpoint_from_discover(&res) {
            Some(s) => s,
            None => return,
        };
        let previous = {
            let mut guard = inner.current.lock().unwrap();
            if guard.as_deref() == Some(&endpoint) {
                return;
            }
            let prev = guard.clone();
            *guard = Some(endpoint.clone());
            prev
        };
        Self::record(&inner.history_path, previous.as_deref(), &endpoint);
        (inner.on_change)(previous.as_deref(), &endpoint);
    }

    fn record(history_path: &std::path::Path, previous: Option<&str>, current: &str) {
        let entry = HistoryEntry {
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            previous: previous.unwrap_or("").to_string(),
            current: current.to_string(),
        };
        if let Some(parent) = history_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    "endpoint-updater: cannot create history dir {}: {e}",
                    parent.display()
                );
                return;
            }
        }
        let mut history: Vec<HistoryEntry> = std::fs::read_to_string(history_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        history.push(entry);
        let trim_from = history.len().saturating_sub(HISTORY_CAP);
        let trimmed: Vec<HistoryEntry> = history.drain(trim_from..).collect();

        let tmp = history_path.with_extension("tmp");
        match serde_json::to_string_pretty(&trimmed) {
            Ok(body) => {
                if let Err(e) = std::fs::write(&tmp, body.as_bytes()) {
                    tracing::warn!(
                        "endpoint-updater: history write failed at {}: {e}",
                        tmp.display()
                    );
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, history_path) {
                    tracing::warn!(
                        "endpoint-updater: history rename failed: {e}"
                    );
                }
            }
            Err(e) => tracing::warn!("endpoint-updater: history serialize failed: {e}"),
        }
    }
}

fn endpoint_from_discover(res: &Value) -> Option<String> {
    let ip = res.get("ip")?.as_str()?;
    let port = res.get("port")?.as_u64()?;
    Some(format!("{ip}:{port}"))
}

impl Inner {
    // Marker to keep clippy off `dead_code` warnings about unused fields
    // when tests don't construct via every code path.
    #[allow(dead_code)]
    fn touch(&self) {
        let _ = &self.history_path;
        let _ = &self.stun_servers;
        let _ = self.interval;
    }
}

// ── helpers exposed for tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;

    /// STUN transport that returns a pre-loaded sequence of results.
    /// Each probe pops the next result from the head of the queue.
    struct ScriptedStun {
        queue: StdMutex<std::collections::VecDeque<Option<stun::StunResult>>>,
    }
    impl ScriptedStun {
        fn new(results: Vec<Option<stun::StunResult>>) -> Self {
            Self {
                queue: StdMutex::new(results.into_iter().collect()),
            }
        }
    }
    impl Transport for ScriptedStun {
        fn probe(
            &self,
            server: &str,
            _change_flags: u32,
            _local_port: u16,
            _timeout: Duration,
        ) -> Option<stun::StunResult> {
            let mut r = self.queue.lock().unwrap().pop_front().flatten();
            if let Some(ref mut v) = r {
                v.server = server.to_string();
            }
            r
        }
    }

    fn make_result(ip: &str, port: u16) -> stun::StunResult {
        stun::StunResult {
            server: String::new(),
            mapped_ip: ip.to_string(),
            mapped_port: port,
            other_address: None,
            rtt_ms: 1.0,
        }
    }

    fn tmp_history() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("endpoint-history.json");
        (dir, path)
    }

    #[tokio::test]
    async fn tick_fires_on_change_and_records_history() {
        let (_dir, history) = tmp_history();
        let calls = Arc::new(AtomicU32::new(0));
        type Captured = Vec<(Option<String>, String)>;
        let captured: Arc<StdMutex<Captured>> =
            Arc::new(StdMutex::new(Vec::new()));
        let captured_cb = Arc::clone(&captured);
        let calls_cb = Arc::clone(&calls);
        let cb: OnChange = Arc::new(move |prev, curr| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            captured_cb
                .lock()
                .unwrap()
                .push((prev.map(str::to_string), curr.to_string()));
        });
        let scripted = Arc::new(ScriptedStun::new(vec![
            Some(make_result("1.2.3.4", 5000)),
            Some(make_result("1.2.3.4", 5000)), // no change
            Some(make_result("9.9.9.9", 6000)), // change!
        ]));
        let updater = EndpointUpdater::with_transport(
            vec!["stun.example:3478".to_string()],
            Duration::from_millis(10),
            history.clone(),
            cb,
            scripted,
        );

        updater.tick_once().await;
        updater.tick_once().await;
        updater.tick_once().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let cap = captured.lock().unwrap();
        assert_eq!(cap.len(), 2);
        assert_eq!(cap[0].0, None);
        assert_eq!(cap[0].1, "1.2.3.4:5000");
        assert_eq!(cap[1].0.as_deref(), Some("1.2.3.4:5000"));
        assert_eq!(cap[1].1, "9.9.9.9:6000");

        let raw = std::fs::read_to_string(&history).unwrap();
        let recorded: Vec<HistoryEntry> = serde_json::from_str(&raw).unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].previous, "");
        assert_eq!(recorded[0].current, "1.2.3.4:5000");
        assert_eq!(recorded[1].previous, "1.2.3.4:5000");
        assert_eq!(recorded[1].current, "9.9.9.9:6000");
    }

    #[tokio::test]
    async fn tick_swallows_probe_failure_without_callback() {
        let (_dir, history) = tmp_history();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = Arc::clone(&calls);
        let cb: OnChange = Arc::new(move |_, _| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
        });
        let scripted = Arc::new(ScriptedStun::new(vec![None, None]));
        let updater = EndpointUpdater::with_transport(
            vec!["stun.example:3478".to_string()],
            Duration::from_millis(10),
            history,
            cb,
            scripted,
        );
        updater.tick_once().await;
        updater.tick_once().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(updater.current().is_none());
    }

    #[tokio::test]
    async fn history_caps_at_100_entries() {
        // Cap behaviour matches Python — only the last 100 entries
        // survive. Verify by recording 105 changes.
        let (_dir, history) = tmp_history();
        let mut script: Vec<Option<stun::StunResult>> = Vec::new();
        for i in 0..105 {
            script.push(Some(make_result(&format!("10.0.0.{i}"), 100)));
        }
        let scripted = Arc::new(ScriptedStun::new(script));
        let cb: OnChange = Arc::new(|_, _| {});
        let updater = EndpointUpdater::with_transport(
            vec!["s:3478".to_string()],
            Duration::from_millis(0),
            history.clone(),
            cb,
            scripted,
        );
        for _ in 0..105 {
            updater.tick_once().await;
        }
        let raw = std::fs::read_to_string(&history).unwrap();
        let recorded: Vec<HistoryEntry> = serde_json::from_str(&raw).unwrap();
        assert_eq!(recorded.len(), HISTORY_CAP);
        // Oldest survivor should be the 6th entry (`10.0.0.5`) since
        // we recorded 105 and trimmed to the last 100.
        assert_eq!(recorded[0].current, "10.0.0.5:100");
        assert_eq!(recorded[99].current, "10.0.0.104:100");
    }

    #[tokio::test]
    async fn start_and_stop_round_trip() {
        let (_dir, history) = tmp_history();
        let cb: OnChange = Arc::new(|_, _| {});
        let scripted = Arc::new(ScriptedStun::new(vec![
            Some(make_result("1.1.1.1", 100)),
        ]));
        let updater = EndpointUpdater::with_transport(
            vec!["s:3478".to_string()],
            Duration::from_secs(3600),
            history,
            cb,
            scripted,
        );
        let handle = updater.start();
        // Give the spawned task a tick to capture the first probe.
        tokio::time::sleep(Duration::from_millis(50)).await;
        updater.stop();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert_eq!(updater.current().as_deref(), Some("1.1.1.1:100"));
    }

    #[test]
    fn endpoint_from_discover_parses_shape() {
        let v: Value = serde_json::json!({"ip": "1.1.1.1", "port": 100});
        assert_eq!(endpoint_from_discover(&v).as_deref(), Some("1.1.1.1:100"));
        let bad: Value = serde_json::json!({"ip": "1.1.1.1"});
        assert!(endpoint_from_discover(&bad).is_none());
    }
}
