//! Service manifest writer — Rust port of `Core/shared/manifest.py`.
//!
//! Services write a JSON manifest to `data/manifests/{service}.json` on
//! startup. Fletch and the Lifecycle daemon read those files directly (zero
//! IPC) to render dashboards and supervise services. The Python and Rust
//! implementations must produce wire-equivalent output and read each other's
//! files, because the mesh runs both in parallel during the strangler-fig
//! phase.
//!
//! Ownership mirrors the Python module:
//!
//! * The service owns its manifest. It calls [`ManifestWriter::write`] at
//!   startup, [`ManifestWriter::start_heartbeat`] to keep `status.heartbeat`
//!   fresh, and [`ManifestWriter::mark_stopped`] from its shutdown handler.
//! * The Lifecycle daemon (and *only* the daemon) calls
//!   [`mark_orphan_dead`] when it observes an `alive` manifest whose pid no
//!   longer exists.
//!
//! `status.state` values: `alive` (heartbeating), `stopped` (graceful exit),
//! `dead-orphan` (orphan-detector found the pid gone).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

const MANIFEST_VERSION: &str = "1.0.0";
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";


/// Phases that fired BEFORE a `ManifestWriter::write` call.  Seeded into
/// each manifest's `startup_sequence` field when written.  Mirrors the
/// Python `_PHASES_FIRED_PRE_WRITE` buffer.
fn pre_write_phases() -> &'static Mutex<Vec<String>> {
    static BUF: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(Vec::new()))
}


/// Maps `service` to its in-memory `ManifestData` so freestanding helpers
/// (`mark_serve_loop_entered`, `attest_phase` after the manifest exists)
/// can append phases that propagate through the next heartbeat write.
fn writer_registry() -> &'static Mutex<HashMap<String, Arc<Mutex<ManifestData>>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<Mutex<ManifestData>>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}


/// Record that a named startup phase has fired this process.  Idempotent
/// on adjacent duplicates.  Called by `configure_logging` and friends so
/// the runtime self-attests the four-phase Wylde startup convention.
///
/// Phases fired BEFORE `ManifestWriter::write` are buffered globally and
/// seeded into the manifest on first write.  Phases fired AFTER write
/// land in any cached manifests directly and are atomic-written.
pub fn attest_phase(phase: &str) {
    let mut buf = pre_write_phases().lock().expect("pre_write_phases poisoned");
    if buf.last().map(String::as_str) != Some(phase) {
        buf.push(phase.to_owned());
    }
    drop(buf);
    // Also push into every active writer's manifest (one binary = one
    // service typically, but the registry handles the general case).
    let reg = writer_registry()
        .lock()
        .expect("writer_registry poisoned");
    let entries: Vec<(String, Arc<Mutex<ManifestData>>)> =
        reg.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    drop(reg);
    for (service, cached) in entries {
        if let Err(e) = persist_phase(&service, &cached, phase) {
            tracing::warn!(
                "manifest: attest_phase {} failed for {}: {}",
                phase,
                service,
                e
            );
        }
    }
}


fn persist_phase(service: &str, cached: &Arc<Mutex<ManifestData>>, phase: &str) -> Result<()> {
    let snapshot = {
        let mut guard = cached
            .lock()
            .map_err(|_| anyhow!("manifest: cache poisoned for {}", service))?;
        if guard.startup_sequence.last().map(String::as_str) != Some(phase) {
            guard.startup_sequence.push(phase.to_owned());
        }
        guard.clone()
    };
    atomic_write(&manifest_path(service), &snapshot)
}


/// Attest that the service has entered its serve loop.  Called by
/// `crate::ipc::serve` at the top of the accept loop.  Best-effort:
/// errors are logged, never raised.
pub fn mark_serve_loop_entered(service: &str) {
    let reg = writer_registry()
        .lock()
        .expect("writer_registry poisoned");
    let cached = reg.get(service).cloned();
    drop(reg);
    let phase = "serve_loop";
    match cached {
        Some(c) => {
            if let Err(e) = persist_phase(service, &c, phase) {
                tracing::warn!(
                    "manifest: mark_serve_loop_entered failed for {}: {}",
                    service,
                    e
                );
            }
        }
        None => attest_phase(phase),
    }
}

/// Resolve the manifest directory from `WYLDE_ROOT` (matches the Python
/// `_WYLDE_ROOT` env override). Read on every call so tests can set the var
/// per-case; in production WYLDE_ROOT is fixed at process start.
fn manifest_dir() -> PathBuf {
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("data").join("manifests")
}

fn manifest_path(service: &str) -> PathBuf {
    manifest_dir().join(format!("{service}.json"))
}

fn now_iso() -> String {
    Utc::now().format(TIME_FORMAT).to_string()
}

/// Best-effort fallback for `entry_point`. Python returns `python:<argv0-stem>`;
/// Rust returns `rust:<exe-stem>`. Services should pass an explicit value.
fn default_entry_point() -> String {
    let stem = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned());
    format!("rust:{stem}")
}

/// Atomic write: pretty-print JSON to a `{stem}.{pid}.tmp` sibling then
/// `rename` over the target. The pid-suffixed tmpfile matches the Python
/// pattern so Python and Rust writes on the same service don't clobber each
/// other's in-flight files during the strangler-fig phase.
fn atomic_write(path: &Path, data: &ManifestData) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("manifest: create parent dir {}", parent.display()))?;
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("manifest: invalid path stem for {}", path.display()))?;
    let pid = std::process::id();
    let tmp_path = path.with_file_name(format!("{stem}.{pid}.tmp"));
    let bytes = serde_json::to_vec_pretty(data).context("manifest: serialize")?;
    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
        let _ = std::fs::remove_file(&tmp_path); // wylde-check: discard-result-ok
        return Err(e).with_context(|| format!("manifest: write tmp {}", tmp_path.display()));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path); // wylde-check: discard-result-ok
        return Err(e).with_context(|| format!("manifest: rename to {}", path.display()));
    }
    Ok(())
}

/// Top-level manifest record. Field order here is the on-disk JSON order;
/// serde respects struct declaration order, which gives us a deterministic
/// layout matching `Core/shared/manifest.py::write_manifest`.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct ManifestData {
    service: String,
    version: String,
    pipe: String,
    port: Option<u16>,
    category: String,
    description: String,
    entry_point: String,
    contributes: Value,
    startup_sequence: Vec<String>,
    shutdown_attested: bool,
    status: Status,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Status {
    pid: u32,
    started_at: String,
    heartbeat: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    stop_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    last_seen: Option<String>,
}

/// Owns the cached manifest dict and the on-disk path. Clone-cheap via the
/// internal `Arc<Mutex<…>>` if you need to hand it to multiple threads.
pub struct ManifestWriter {
    service: String,
    path: PathBuf,
    cached: Arc<Mutex<ManifestData>>,
}


impl Drop for ManifestWriter {
    fn drop(&mut self) {
        if let Ok(mut reg) = writer_registry().lock() {
            reg.remove(&self.service);
        }
    }
}

impl ManifestWriter {
    /// Write (or overwrite) the manifest and seed the in-memory cache.
    /// Safe to call multiple times; preserves `status.started_at` from any
    /// existing file so a mid-run refresh doesn't reset the original start
    /// time (mirrors the Python behaviour).
    pub fn write(
        service: &str,
        port: Option<u16>,
        category: &str,
        description: &str,
        contributes: Value,
        entry_point: Option<&str>,
    ) -> Result<Self> {
        let path = manifest_path(service);

        let started_at = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| {
                v.get("status")?
                    .get("started_at")?
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or_else(now_iso);

        let pipe_suffix = service.strip_prefix("wylde-").unwrap_or(service);
        let entry_point = entry_point
            .map(str::to_owned)
            .unwrap_or_else(default_entry_point);

        // Seed startup_sequence from the pre-write phase buffer.  Adding
        // "write_manifest" here is the canonical second phase; downstream
        // helpers (start_heartbeat, mark_serve_loop_entered) append the
        // remaining two.
        let mut startup_sequence = pre_write_phases()
            .lock()
            .expect("pre_write_phases poisoned")
            .clone();
        if startup_sequence.last().map(String::as_str) != Some("write_manifest") {
            startup_sequence.push("write_manifest".to_owned());
        }

        let manifest = ManifestData {
            service: service.to_owned(),
            version: MANIFEST_VERSION.to_owned(),
            pipe: format!(r"\\.\pipe\wylde-{pipe_suffix}"),
            port,
            category: category.to_owned(),
            description: description.to_owned(),
            entry_point,
            contributes,
            startup_sequence,
            shutdown_attested: false,
            status: Status {
                pid: std::process::id(),
                started_at,
                heartbeat: now_iso(),
                state: "alive".to_owned(),
                stop_time: None,
                last_seen: None,
            },
        };

        atomic_write(&path, &manifest)?;
        tracing::info!("manifest: wrote {}", path.display());

        let cached = Arc::new(Mutex::new(manifest));
        writer_registry()
            .lock()
            .expect("writer_registry poisoned")
            .insert(service.to_owned(), cached.clone());

        Ok(Self {
            service: service.to_owned(),
            path,
            cached,
        })
    }

    /// Spawn a tokio task that bumps `status.heartbeat` every `interval`.
    /// Drop the returned handle to stop the task.
    pub fn start_heartbeat(&self, interval: Duration) -> HeartbeatHandle {
        // Self-attest the start_heartbeat phase before the loop spawns so
        // the very first heartbeat write already carries the phase.
        if let Err(e) = persist_phase(&self.service, &self.cached, "start_heartbeat") {
            tracing::warn!(
                "manifest: start_heartbeat attestation failed for {}: {}",
                self.service,
                e
            );
        }
        let (tx, mut rx) = oneshot::channel::<()>();
        let cached = Arc::clone(&self.cached);
        let path = self.path.clone();
        let service = self.service.clone();

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    _ = tokio::time::sleep(interval) => {
                        let snapshot = match cached.lock() {
                            Ok(mut guard) => {
                                guard.status.heartbeat = now_iso();
                                guard.clone()
                            }
                            Err(_) => {
                                tracing::error!(
                                    "manifest: heartbeat cache poisoned for {}",
                                    service
                                );
                                break;
                            }
                        };
                        if let Err(e) = atomic_write(&path, &snapshot) {
                            tracing::warn!(
                                "manifest: heartbeat write failed for {}: {}",
                                service,
                                e
                            );
                        }
                    }
                }
            }
        });

        HeartbeatHandle {
            cancel: Some(tx),
            task: Some(task),
        }
    }

    /// Flip the manifest to `state = "stopped"` and stamp `stop_time`.
    /// Also sets `shutdown_attested = true` so wylde_check sees a clean
    /// shutdown without AST-walking the signal handler.  Callers should
    /// drop any `HeartbeatHandle` before/after this so the heartbeat
    /// doesn't race the state write back to `alive`.
    pub fn mark_stopped(&self) -> Result<()> {
        let snapshot = {
            let mut guard = self
                .cached
                .lock()
                .map_err(|_| anyhow!("manifest: cache poisoned for {}", self.service))?;
            let now = now_iso();
            guard.status.state = "stopped".to_owned();
            guard.status.stop_time = Some(now.clone());
            guard.status.heartbeat = now;
            guard.shutdown_attested = true;
            guard.clone()
        };
        atomic_write(&self.path, &snapshot)
    }

    /// Replace the `contributes` block and bump heartbeat. Use when fields
    /// like device-probe results aren't known at `write` time.
    pub fn update_contributes(&self, contributes: Value) -> Result<()> {
        let snapshot = {
            let mut guard = self
                .cached
                .lock()
                .map_err(|_| anyhow!("manifest: cache poisoned for {}", self.service))?;
            guard.contributes = contributes;
            guard.status.heartbeat = now_iso();
            guard.clone()
        };
        atomic_write(&self.path, &snapshot)
    }
}

/// Cancellation handle for the heartbeat task. Dropping it signals the task
/// to stop and aborts it so the next iteration cannot fire a stale write.
pub struct HeartbeatHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(()); // wylde-check: discard-result-ok
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Mark an `alive`-but-orphaned manifest as `dead-orphan`. Used by the
/// Lifecycle daemon's orphan sweep; services never call this directly.
/// Best-effort: returns `Ok(())` when the manifest is absent.
pub fn mark_orphan_dead(service: &str) -> Result<()> {
    let path = manifest_path(service);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("manifest: read {}", path.display()))?;
    let mut data: ManifestData = serde_json::from_str(&raw)
        .with_context(|| format!("manifest: parse {}", path.display()))?;
    data.status.state = "dead-orphan".to_owned();
    data.status.last_seen = Some(now_iso());
    atomic_write(&path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::TempDir;

    // These tests mutate two process-global resources: `WYLDE_ROOT` (via
    // `set_root`) AND the manifest writer-registry / pre-write-phase statics.
    // The `manifest` serial group serialises them against EACH OTHER *and*
    // against the cross-module `logging::idempotent` test (which calls
    // `attest_phase` → the same statics) — that cross-module contention is
    // what flaked `mark_orphan_dead_works`, and a per-module mutex couldn't
    // cover it. `serial_test` is the crate-wide lock that can.
    fn set_root(tmp: &TempDir) {
        std::env::set_var("WYLDE_ROOT", tmp.path());
    }

    fn read_manifest(tmp: &TempDir, service: &str) -> Value {
        let p = tmp
            .path()
            .join("data")
            .join("manifests")
            .join(format!("{service}.json"));
        let raw = std::fs::read_to_string(&p).expect("read manifest");
        serde_json::from_str(&raw).expect("parse manifest")
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn write_and_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let _w = ManifestWriter::write(
            "wylde-vram-broker",
            Some(9101),
            "core",
            "VRAM allocation broker",
            json!({"vram_broker": {"state_path": "/vram/state"}}),
            Some("rust:wylde-vram-broker"),
        )
        .unwrap();

        let m = read_manifest(&tmp, "wylde-vram-broker");
        assert_eq!(m["service"], "wylde-vram-broker");
        assert_eq!(m["version"], "1.0.0");
        assert_eq!(m["pipe"], r"\\.\pipe\wylde-vram-broker");
        assert_eq!(m["port"], 9101);
        assert_eq!(m["category"], "core");
        assert_eq!(m["description"], "VRAM allocation broker");
        assert_eq!(m["entry_point"], "rust:wylde-vram-broker");
        assert_eq!(m["contributes"]["vram_broker"]["state_path"], "/vram/state");
        assert_eq!(m["status"]["state"], "alive");
        assert_eq!(m["status"]["pid"], std::process::id());
        assert!(m["status"]["started_at"].is_string());
        assert!(m["status"]["heartbeat"].is_string());
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn preserves_started_at_on_rewrite() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let _first = ManifestWriter::write(
            "wylde-test",
            None,
            "core",
            "first",
            json!({}),
            Some("rust:test"),
        )
        .unwrap();
        let first = read_manifest(&tmp, "wylde-test");
        let first_started = first["status"]["started_at"].as_str().unwrap().to_owned();

        // Sleep one second so the wall clock advances past Python's
        // second-resolution timestamp format.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let _second = ManifestWriter::write(
            "wylde-test",
            None,
            "core",
            "second",
            json!({}),
            Some("rust:test"),
        )
        .unwrap();
        let second = read_manifest(&tmp, "wylde-test");
        assert_eq!(
            second["status"]["started_at"].as_str().unwrap(),
            first_started
        );
        assert_eq!(second["description"], "second");
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn heartbeat_updates_heartbeat_field() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let w = ManifestWriter::write(
            "wylde-hb",
            None,
            "core",
            "hb test",
            json!({}),
            Some("rust:hb"),
        )
        .unwrap();
        let before = read_manifest(&tmp, "wylde-hb");
        let before_hb = before["status"]["heartbeat"].as_str().unwrap().to_owned();

        tokio::time::sleep(Duration::from_millis(1100)).await;

        let hb = w.start_heartbeat(Duration::from_millis(200));
        tokio::time::sleep(Duration::from_millis(700)).await;
        drop(hb);

        let after = read_manifest(&tmp, "wylde-hb");
        let after_hb = after["status"]["heartbeat"].as_str().unwrap();
        assert_ne!(before_hb, after_hb, "heartbeat field should have advanced");
        assert_eq!(after["status"]["state"], "alive");
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn mark_stopped_changes_state() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let w = ManifestWriter::write(
            "wylde-stop",
            None,
            "core",
            "stop test",
            json!({}),
            Some("rust:stop"),
        )
        .unwrap();
        w.mark_stopped().unwrap();

        let m = read_manifest(&tmp, "wylde-stop");
        assert_eq!(m["status"]["state"], "stopped");
        assert!(m["status"]["stop_time"].is_string());
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn update_contributes_replaces_block() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let w = ManifestWriter::write(
            "wylde-uc",
            None,
            "core",
            "uc test",
            json!({"old": true}),
            Some("rust:uc"),
        )
        .unwrap();
        w.update_contributes(json!({"new": 42})).unwrap();

        let m = read_manifest(&tmp, "wylde-uc");
        assert!(m["contributes"].get("old").is_none());
        assert_eq!(m["contributes"]["new"], 42);
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn mark_orphan_dead_works() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        let _w = ManifestWriter::write(
            "wylde-orph",
            None,
            "core",
            "orph test",
            json!({}),
            Some("rust:orph"),
        )
        .unwrap();

        mark_orphan_dead("wylde-orph").unwrap();
        let m = read_manifest(&tmp, "wylde-orph");
        assert_eq!(m["status"]["state"], "dead-orphan");
        assert!(m["status"]["last_seen"].is_string());

        // Idempotent: re-marking refreshes last_seen, doesn't error.
        mark_orphan_dead("wylde-orph").unwrap();
        let m2 = read_manifest(&tmp, "wylde-orph");
        assert_eq!(m2["status"]["state"], "dead-orphan");
    }

    #[tokio::test]
    #[serial(manifest)]
    async fn mark_orphan_dead_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        mark_orphan_dead("wylde-nope").unwrap();
    }

    /// Cross-language parity: a hand-written fixture matching what
    /// `Core/shared/manifest.py` would produce. Rust output must agree on
    /// every static field (everything except `pid`, `started_at`, and
    /// `heartbeat`).
    #[tokio::test]
    #[serial(manifest)]
    async fn parity_with_python_fixture() {
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        // Shape that `Core/shared/manifest.py::write_manifest` produces for
        // these inputs. Reproduced here as a fixture so the test is hermetic.
        let fixture = json!({
            "service": "wylde-vram-broker",
            "version": "1.0.0",
            "pipe": r"\\.\pipe\wylde-vram-broker",
            "port": 9101,
            "category": "core",
            "description": "GPU VRAM lease broker",
            "entry_point": "rust:wylde-vram-broker",
            "contributes": {
                "vram_broker": {
                    "state_path": "/vram/state",
                    "leases_path": "/vram/leases"
                }
            },
            "status": {
                "pid": 12345,
                "started_at": "2026-05-15T14:00:00Z",
                "heartbeat": "2026-05-15T14:00:00Z",
                "state": "alive"
            }
        });

        let _w = ManifestWriter::write(
            "wylde-vram-broker",
            Some(9101),
            "core",
            "GPU VRAM lease broker",
            json!({
                "vram_broker": {
                    "state_path": "/vram/state",
                    "leases_path": "/vram/leases"
                }
            }),
            Some("rust:wylde-vram-broker"),
        )
        .unwrap();
        let actual = read_manifest(&tmp, "wylde-vram-broker");

        for key in [
            "service",
            "version",
            "pipe",
            "port",
            "category",
            "description",
            "entry_point",
            "contributes",
        ] {
            assert_eq!(
                actual[key], fixture[key],
                "field {key} diverges from Python"
            );
        }
        assert_eq!(actual["status"]["state"], fixture["status"]["state"]);
        // pid + timestamps are runtime values; assert they're present and well-typed.
        assert!(actual["status"]["pid"].is_u64());
        assert!(actual["status"]["started_at"].is_string());
        assert!(actual["status"]["heartbeat"].is_string());
    }
}
