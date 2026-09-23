//! Manifest writers for daemon-managed services.
//!
//! Rust port of `Core/Lifecycle/daemon_state/_manifest.py`. Core's
//! runtime manifest lives at `data/manifests/core.json`. Core is one
//! logical service in the dashboard — its internal pipes
//! (wylde-lifecycle, wylde-harness, wylde-memgraph) are NOT
//! individually surfaced. Registry probes each constituent pipe live;
//! this manifest only carries identity + heartbeat so the dashboard
//! can show uptime and a fresh heartbeat indicator.
//!
//! Daemon-managed top-level services (Voice, device_gate, gateway,
//! vram_broker) publish their own runtime manifests via
//! `wylde-shared::manifest`. This module only deals with the Core
//! roll-up — the children own their own files.
//!
//! Manifest format must stay wire-compatible with what
//! `Core.shared.manifest.write_manifest` writes (W4.1 verified that
//! parity). We write the same dict shape as the Python side, atomic
//! tmpfile + rename.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::state::{manifest_dir, manifest_path_for};

const MANIFEST_VERSION: &str = "1.0.0";
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Stale per-pipe manifest files from the prior granularity (each Core
/// sub-pipe got its own row). Cleared on [`register_core_manifest`] so
/// a fresh daemon start doesn't leave the dashboard reporting them as
/// peers. Mirrors `_DEPRECATED_CORE_SUB_MANIFESTS` on the Python side.
pub const DEPRECATED_CORE_SUB_MANIFESTS: &[&str] = &[
    "wylde-lifecycle",
    "wylde-harness",
    "wylde-memgraph",
    "wylde-memory-scheduler",
];

/// Cancellation handle for Core's heartbeat task. Stored in
/// [`CORE_HEARTBEAT`] so [`unregister_core_manifest`] can drain it.
struct HeartbeatHandle {
    cancel: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        self.cancel.notify_waiters();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn heartbeat_slot() -> &'static Mutex<Option<HeartbeatHandle>> {
    static H: OnceLock<Mutex<Option<HeartbeatHandle>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

fn now_iso() -> String {
    Utc::now().format(TIME_FORMAT).to_string()
}

/// Atomic write — pid-suffixed tmpfile + rename, matching the pattern
/// `Core/shared/manifest.py` and `daemon_state/__init__._atomic_write_json`
/// use so Python and Rust writes don't clobber each other's in-flight
/// files during the strangler-fig phase.
fn atomic_write_json(path: &Path, data: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("manifest: create parent dir {}", parent.display()))?;
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("manifest: invalid stem for {}", path.display()))?;
    let pid = std::process::id();
    let tmp = path.with_file_name(format!("{stem}.{pid}.tmp"));
    let body = serde_json::to_vec_pretty(data).context("manifest: serialize")?;
    if let Err(e) = fs::write(&tmp, &body) {
        let _ = fs::remove_file(&tmp); // wylde-check: discard-result-ok
        return Err(e).with_context(|| format!("manifest: write tmp {}", tmp.display()));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp); // wylde-check: discard-result-ok
        return Err(e).with_context(|| format!("manifest: rename to {}", path.display()));
    }
    Ok(())
}

fn delete_manifest_file(name: &str) {
    let path = manifest_path_for(name);
    if let Err(e) = fs::remove_file(&path) {
        // Missing is fine; everything else gets a warning.
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("manifest: delete failed for {}: {}", name, e);
        }
    }
}

/// Write `data/manifests/<name>.json` so `service.list` can see this
/// service. Preserves `status.started_at` across writes within the
/// same session so the dashboard's uptime field stays honest after a
/// heartbeat update.
fn write_daemon_manifest(
    name: &str,
    pid: u32,
    description: &str,
    category: &str,
    pipe: Option<&str>,
    contributes: Value,
) -> Result<()> {
    let path = manifest_path_for(name);
    let started_at = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("status")?
                .get("started_at")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(now_iso);

    let manifest = json!({
        "service": name,
        "version": MANIFEST_VERSION,
        "kind": "daemon-managed",
        "pipe": pipe,
        "port": Value::Null,
        "category": category,
        "description": description,
        "contributes": contributes,
        "status": {
            "pid": pid,
            "started_at": started_at,
            "heartbeat": now_iso(),
        },
    });
    atomic_write_json(&path, &manifest)?;
    tracing::info!("manifest: wrote {} (pid={})", name, pid);
    Ok(())
}

/// Spawn a background task that bumps `status.heartbeat` every
/// `interval`. Drop the returned handle (or call
/// [`unregister_core_manifest`]) to stop the task. Idempotent: a
/// second registration cancels the prior task before installing the
/// new one.
fn start_core_heartbeat(interval: Duration) {
    let cancel = Arc::new(Notify::new());
    let cancel_clone = cancel.clone();
    let path = manifest_path_for("wylde-core");
    let pid = std::process::id();

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_clone.notified() => break,
                _ = tokio::time::sleep(interval) => {
                    if !path.exists() {
                        continue;
                    }
                    let mut manifest: Value = match fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                    {
                        Some(v) => v,
                        None => continue,
                    };
                    if let Some(status) = manifest.get_mut("status").and_then(|s| s.as_object_mut()) {
                        status.insert("heartbeat".into(), Value::String(now_iso()));
                        if status.get("pid").and_then(Value::as_u64) != Some(u64::from(pid)) {
                            status.insert("pid".into(), Value::from(pid));
                        }
                    }
                    if let Err(e) = atomic_write_json(&path, &manifest) {
                        tracing::warn!("manifest: core heartbeat write failed: {}", e);
                    }
                }
            }
        }
    });

    if let Ok(mut slot) = heartbeat_slot().lock() {
        *slot = Some(HeartbeatHandle {
            cancel,
            task: Some(task),
        });
    }
}

/// Publish Core's runtime manifest as a single `core.json`.
///
/// Replaces the previous per-pipe manifests (wylde-lifecycle,
/// wylde-harness, wylde-memgraph, wylde-memory-scheduler). Core is
/// one service in the dashboard — registry probes constituent pipes
/// live, so this manifest only carries identity + heartbeat.
/// Idempotent: callers can re-invoke it after subsystems come up to
/// refresh.
///
/// Also clears stale per-pipe manifest files from prior daemon
/// versions so the dashboard doesn't surface them as peers during
/// the transition.
pub fn register_core_manifest() -> Result<()> {
    for stale in DEPRECATED_CORE_SUB_MANIFESTS {
        delete_manifest_file(stale);
    }
    write_daemon_manifest(
        "wylde-core",
        std::process::id(),
        // Match the Python description verbatim so dashboard tooltips
        // stay identical regardless of which daemon is running.
        "Wylde core infrastructure (lifecycle, harness, memgraph, memory scheduler). \
         Constituent pipes: \\\\.\\pipe\\wylde-lifecycle, \\\\.\\pipe\\wylde-harness, \
         \\\\.\\pipe\\wylde-memgraph.",
        "core",
        None,
        json!({
            "dashboard": {"label": "Core", "icon": "cpu", "color": "blue"},
        }),
    )?;
    start_core_heartbeat(Duration::from_secs(60));
    Ok(())
}

/// Cancel the heartbeat task and delete `core.json`. Called from
/// [`crate::state::stop_all_daemon_managed`] during graceful shutdown
/// so the dashboard doesn't surface a Core entry with a stale
/// heartbeat after the daemon exits.
pub fn unregister_core_manifest() -> Result<()> {
    if let Ok(mut slot) = heartbeat_slot().lock() {
        *slot = None;
    }
    delete_manifest_file("wylde-core");
    Ok(())
}

/// Resolve the manifest path for `name`. Re-exported helper so
/// submodules don't need to dig through `crate::state` for it.
#[allow(dead_code)]
pub(crate) fn path_for(name: &str) -> PathBuf {
    manifest_path_for(name)
}

/// Resolve the manifest dir. Same rationale.
#[allow(dead_code)]
pub(crate) fn dir() -> PathBuf {
    manifest_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    // These tests write / delete the process-shared `core.json`. So does
    // `control::tests::shutdown_all_returns_structured_summary` (via
    // `unregister_core_manifest`). Both groups serialise on the SAME
    // process-global lock — `state::tests::state_guard` — so a concurrent
    // delete can't race a rewrite's started-at read.
    fn set_root(tmp: &TempDir) {
        std::env::set_var("WYLDE_ROOT", tmp.path());
        // The state's manifest_dir is captured once at OnceLock init;
        // for tests we work around that by writing directly to where
        // the function points. The test confirms wiring within a fresh
        // process, which is the only realistic environment.
    }

    #[serial]
    #[tokio::test]
    async fn write_then_read_core_manifest() {
        let _g = crate::state::tests::state_guard().await;
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);

        // The state cache may already point at a different dir if it
        // was initialised by a prior test; fall back to the resolved
        // path the helpers use.
        let core_path = manifest_path_for("wylde-core");
        if let Some(parent) = core_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        write_daemon_manifest(
            "wylde-core",
            std::process::id(),
            "core test",
            "core",
            None,
            json!({"dashboard": {"label": "Core"}}),
        )
        .unwrap();

        let raw = fs::read_to_string(&core_path).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["service"], "wylde-core");
        assert_eq!(v["category"], "core");
        assert_eq!(v["kind"], "daemon-managed");
        assert_eq!(v["status"]["pid"], std::process::id());
        assert!(v["status"]["started_at"].is_string());
        assert!(v["status"]["heartbeat"].is_string());

        delete_manifest_file("wylde-core");
    }

    #[serial]
    #[tokio::test]
    async fn preserves_started_at_across_rewrite() {
        let _g = crate::state::tests::state_guard().await;
        let tmp = TempDir::new().unwrap();
        set_root(&tmp);
        let path = manifest_path_for("wylde-core");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        // Avoid contamination from prior test runs in the same process.
        let _ = fs::remove_file(&path); // wylde-check: discard-result-ok

        write_daemon_manifest(
            "wylde-core",
            std::process::id(),
            "first",
            "core",
            None,
            json!({}),
        )
        .unwrap();
        let first: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let first_started = first["status"]["started_at"].as_str().unwrap().to_owned();

        tokio::time::sleep(Duration::from_millis(1100)).await;

        write_daemon_manifest(
            "wylde-core",
            std::process::id(),
            "second",
            "core",
            None,
            json!({}),
        )
        .unwrap();
        let second: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            second["status"]["started_at"].as_str().unwrap(),
            first_started
        );
        assert_eq!(second["description"], "second");
    }
}
