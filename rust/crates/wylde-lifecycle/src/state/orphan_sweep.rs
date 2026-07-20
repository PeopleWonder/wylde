//! Orphan-detection sweep for daemon-managed services.
//!
//! Rust port of `Core/Lifecycle/daemon_state/_orphan_sweep.py`. Post
//! manifest-ownership refactor, services own their `data/manifests/`
//! files (`write_manifest` at startup, `mark_stopped` on graceful
//! shutdown). The daemon's job is:
//!
//!   1. Track what it *spawned* — the spawn record (see
//!      [`crate::state::record_spawn`]) is the daemon's source of
//!      truth that "I started this thing", separate from whether the
//!      service got far enough to write its manifest.
//!   2. Sweep periodically. For every alive-marked manifest whose pid
//!      is no longer running, call [`wylde_shared::manifest::mark_orphan_dead`];
//!      for every spawn record older than the grace window with no
//!      matching manifest, log a failed-to-launch warning. Both run
//!      from one task that ticks on the unified 60s heartbeat cadence.
//!
//! Spawn records live in-memory only. They reset on every daemon
//! boot, which is the right behaviour: a fresh daemon doesn't inherit
//! stale spawn expectations from the prior session.

use std::fs;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;

use crate::state::{
    manifest_dir, manifest_path_for, mark_grace_satisfied, nospawn_enabled, orphan_sweep_running,
    pid_alive, register_orphan_sweep_stop, spawn_records_snapshot, take_orphan_sweep_stop,
    ORPHAN_SWEEP_INTERVAL, SPAWN_GRACE_SECONDS,
};

/// Structured summary of one sweep pass — suitable for logging,
/// integration tests, and the smoke surface. Field shape matches the
/// Python dict.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SweepReport {
    pub orphans: Vec<String>,
    pub failed_to_launch: Vec<String>,
    pub checked: usize,
}

/// Structured summary of the synchronous boot-time orphan sweep.
#[derive(Debug, Default, Clone, Serialize)]
pub struct BootSweepReport {
    /// Manifests whose pid was dead and were DELETED. Always empty under
    /// no-spawn, which inspects but never mutates the shared dir.
    pub removed: Vec<String>,
    /// Manifests whose pid was dead but left in place because no-spawn
    /// mode is active. Empty in production.
    pub would_remove: Vec<String>,
    /// Total manifest files walked.
    pub checked: usize,
}

/// One synchronous dead-manifest cleanup pass, run at daemon boot
/// *before* any `start_<service>` call.
///
/// A manifest left behind by an ungraceful prior exit (Ctrl-C, taskkill,
/// SIGKILL) still marks its service `alive` with a now-dead pid. The
/// recurring [`sweep_orphans`] only fires on the 60s tick, which lands
/// *after* the boot spawns — so without this pass a stale manifest
/// survives a lifecycle restart and the affected service stays dark (the
/// harness / extension_bridge / ollama outage
/// the Wylde user hit on 2026-05-31, recovered only by hand-wiping the manifest
/// dir). Running it here means every lifecycle restart self-heals.
///
/// Unlike the recurring sweep (which marks alive-but-dead manifests
/// `dead-orphan` *in place*), the boot sweep DELETES any manifest whose
/// pid is no longer running — the same hand-wipe the Wylde user had to do — so the
/// immediately-following spawn decisions start from clean state.
///
/// Under no-spawn mode it still walks and logs but DELETES NOTHING: a
/// parity daemon must never mutate the host's shared manifest dir
/// (matching the `register_core_manifest` / `unregister_core_manifest`
/// no-spawn guards). The 60s recurring sweep is unaffected — this is an
/// *additional* one-shot at the top of boot, not a replacement.
pub fn boot_orphan_sweep() -> BootSweepReport {
    let mut report = BootSweepReport::default();
    let dir = manifest_dir();
    let nospawn = nospawn_enabled();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return report,
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();

    for path in paths {
        report.checked += 1;
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let data: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let status = match data.get("status").and_then(Value::as_object) {
            Some(s) => s,
            None => continue,
        };
        // Manifests predating the `state` field are treated as alive —
        // same backwards-compat the recurring sweep applies.
        let state = status.get("state").and_then(Value::as_str);
        if !matches!(state, Some("alive") | None) {
            continue;
        }
        let pid = match status.get("pid").and_then(Value::as_u64) {
            Some(p) if p > 0 && p <= u64::from(u32::MAX) => p as u32,
            _ => continue,
        };
        if pid_alive(pid) {
            continue;
        }
        let service = data
            .get("service")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<unknown>")
                    .to_owned()
            });

        if nospawn {
            tracing::info!(
                "boot_orphan_sweep: NO-SPAWN — {} (manifest pid={}) is dead; would remove {} (left in place)",
                service,
                pid,
                path.display()
            );
            report.would_remove.push(service);
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => {
                tracing::warn!(
                    "boot_orphan_sweep: removed stale manifest for {} (manifest pid={} is dead) — self-healing before spawn",
                    service,
                    pid
                );
                report.removed.push(service);
            }
            Err(e) => {
                tracing::warn!(
                    "boot_orphan_sweep: failed to remove stale manifest {} for {}: {:#}",
                    path.display(),
                    service,
                    e
                );
            }
        }
    }

    report
}

/// One pass of the orphan-detection sweep.
///
/// Walks every `data/manifests/*.json` file. For each manifest with
/// `status.state == "alive"` (or with no `state` field for
/// backwards-compatibility with manifests that predate it) whose pid
/// is no longer running, calls
/// [`wylde_shared::manifest::mark_orphan_dead`]. Also checks each
/// in-flight spawn record — if past the grace window with no manifest
/// on disk and a dead pid, logs a failed-to-launch warning.
pub fn sweep_orphans() -> SweepReport {
    let mut report = SweepReport::default();
    let dir = manifest_dir();

    if let Ok(entries) = fs::read_dir(&dir) {
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        paths.sort();

        for path in paths {
            report.checked += 1;
            let raw = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let data: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let status = match data.get("status").and_then(Value::as_object) {
                Some(s) => s,
                None => continue,
            };
            let state = status.get("state").and_then(Value::as_str);
            // Treat manifests that predate the `state` field as alive
            // — same UX as the old behaviour.
            if !matches!(state, Some("alive") | None) {
                continue;
            }
            let pid = match status.get("pid").and_then(Value::as_u64) {
                Some(p) if p > 0 && p <= u64::from(u32::MAX) => p as u32,
                _ => continue,
            };
            if pid_alive(pid) {
                continue;
            }
            let service = match data.get("service").and_then(Value::as_str) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            if let Err(e) = wylde_shared::manifest::mark_orphan_dead(&service) {
                tracing::warn!(
                    "orphan_sweep: mark_orphan_dead({}) failed: {:#}",
                    service,
                    e
                );
                continue;
            }
            tracing::warn!(
                "orphan_sweep: {} (pid={}) is no longer running — marked dead-orphan",
                service,
                pid
            );
            report.orphans.push(service);
        }
    }

    // Failed-to-launch check: spawn records older than the grace
    // window with no live pid AND no on-disk manifest. The service
    // either died before reaching write_manifest() or never started
    // its runtime at all.
    let now = Instant::now();
    for (name, rec) in spawn_records_snapshot() {
        if rec.grace_satisfied {
            continue;
        }
        let manifest_path = manifest_path_for(&name);
        if manifest_path.exists() {
            mark_grace_satisfied(&name);
            continue;
        }
        if now.duration_since(rec.spawn_time) < Duration::from_secs_f64(SPAWN_GRACE_SECONDS) {
            continue;
        }
        if pid_alive(rec.pid) {
            // Pid is alive but no manifest yet — probably still in
            // startup. Skip; we'll catch it next sweep.
            continue;
        }
        tracing::warn!(
            "orphan_sweep: {} (pid={}) failed to launch — no manifest after {:.0}s grace and pid is gone",
            name,
            rec.pid,
            SPAWN_GRACE_SECONDS,
        );
        report.failed_to_launch.push(name.clone());
        mark_grace_satisfied(&name);
    }

    report
}

/// Spawn the daemon's orphan-detection task (idempotent).
///
/// Called once from [`crate::daemon::serve_forever`] after services
/// are spawned. The task sleeps on a Notify so [`stop_orphan_sweep`]
/// can drain it cleanly during shutdown.
pub fn start_orphan_sweep() {
    start_orphan_sweep_with_interval(ORPHAN_SWEEP_INTERVAL);
}

/// Implementation seam for tests that want a tighter sweep cadence
/// than 60s.
pub fn start_orphan_sweep_with_interval(interval: Duration) {
    if orphan_sweep_running() {
        return;
    }
    let stop = std::sync::Arc::new(Notify::new());
    register_orphan_sweep_stop(stop.clone());

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                _ = tokio::time::sleep(interval) => {
                    // sweep_orphans walks the filesystem and calls
                    // mark_orphan_dead — both are synchronous and short
                    // (10s of ms even with hundreds of manifests). No
                    // need to spawn_blocking here.
                    let report =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(sweep_orphans));
                    // Crash-restart: hand the freshly-detected orphans to the
                    // restart supervisor. It reuses THIS dead-orphan transition
                    // (no parallel watcher), keeps intended stops sacrosanct
                    // (spawn-record gated), and applies backoff + a crash-loop
                    // breaker. Default ON; a clean no-op when disabled or when
                    // the sweep found nothing. Skipped if the sweep panicked.
                    if let Ok(report) = report {
                        crate::state::restart::drive_restarts(&report.orphans).await;
                    }
                }
            }
        }
    });
    tracing::info!(
        "orphan_sweep: started (interval={}s)",
        interval.as_secs_f64()
    );
}

/// Signal the orphan-detection task to exit (idempotent).
pub fn stop_orphan_sweep() {
    if let Some(stop) = take_orphan_sweep_stop() {
        stop.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::sync::Mutex as AsyncMutex;

    static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

    fn write_manifest(dir: &PathBuf, name: &str, body: Value) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    }

    /// The orphan-sweep helpers resolve `manifest_dir` from the
    /// state singleton, which caches `WYLDE_ROOT` from the process
    /// environment at first read. Tests can still operate on the
    /// resolved dir by checking what the singleton returns.
    #[serial]
    #[tokio::test]
    async fn sweep_dead_pid_marks_orphan() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        // Write an "alive" manifest with a clearly dead pid.
        write_manifest(
            &dir,
            "wylde-dead-svc",
            json!({
                "service": "wylde-dead-svc",
                "version": "1.0.0",
                "status": {
                    "pid": 0xFFFFFFFE_u32,
                    "started_at": "2026-05-15T00:00:00Z",
                    "heartbeat": "2026-05-15T00:00:00Z",
                    "state": "alive"
                }
            }),
        );

        let report = sweep_orphans();
        // The orphan list may include unrelated stale manifests from
        // earlier tests in the same temp; what matters is that ours
        // appears.
        assert!(
            report.orphans.iter().any(|s| s == "wylde-dead-svc"),
            "expected wylde-dead-svc in orphans, got {:?}",
            report.orphans
        );
    }

    #[serial]
    #[tokio::test]
    async fn sweep_live_pid_skipped() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        let me = std::process::id();
        write_manifest(
            &dir,
            "wylde-self-svc",
            json!({
                "service": "wylde-self-svc",
                "version": "1.0.0",
                "status": {
                    "pid": me,
                    "started_at": "2026-05-15T00:00:00Z",
                    "heartbeat": "2026-05-15T00:00:00Z",
                    "state": "alive"
                }
            }),
        );

        let report = sweep_orphans();
        assert!(
            !report.orphans.iter().any(|s| s == "wylde-self-svc"),
            "live pid should not be marked orphan, got {:?}",
            report.orphans
        );
    }

    #[serial]
    #[tokio::test]
    async fn sweep_skips_stopped_manifests() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        write_manifest(
            &dir,
            "wylde-stopped-svc",
            json!({
                "service": "wylde-stopped-svc",
                "version": "1.0.0",
                "status": {
                    "pid": 0xFFFFFFFE_u32,
                    "started_at": "2026-05-15T00:00:00Z",
                    "heartbeat": "2026-05-15T00:00:00Z",
                    "state": "stopped"
                }
            }),
        );

        let report = sweep_orphans();
        assert!(
            !report.orphans.iter().any(|s| s == "wylde-stopped-svc"),
            "stopped manifest should not be touched, got {:?}",
            report.orphans
        );
    }

    #[serial]
    #[tokio::test]
    async fn boot_sweep_removes_dead_pid_manifest() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        let path = dir.join("wylde-boot-dead.json");
        write_manifest(
            &dir,
            "wylde-boot-dead",
            json!({
                "service": "wylde-boot-dead",
                "version": "1.0.0",
                "status": {
                    "pid": 0xFFFFFFFE_u32,
                    "started_at": "2026-05-31T00:00:00Z",
                    "heartbeat": "2026-05-31T00:00:00Z",
                    "state": "alive"
                }
            }),
        );
        assert!(path.exists(), "precondition: manifest written");

        let report = boot_orphan_sweep();
        assert!(
            report.removed.iter().any(|s| s == "wylde-boot-dead"),
            "expected wylde-boot-dead in removed, got {:?}",
            report.removed
        );
        assert!(
            !path.exists(),
            "dead-pid manifest should have been deleted by the boot sweep"
        );
    }

    #[serial]
    #[tokio::test]
    async fn boot_sweep_keeps_live_pid_manifest() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        let me = std::process::id();
        let path = dir.join("wylde-boot-live.json");
        write_manifest(
            &dir,
            "wylde-boot-live",
            json!({
                "service": "wylde-boot-live",
                "version": "1.0.0",
                "status": {
                    "pid": me,
                    "started_at": "2026-05-31T00:00:00Z",
                    "heartbeat": "2026-05-31T00:00:00Z",
                    "state": "alive"
                }
            }),
        );

        let report = boot_orphan_sweep();
        assert!(
            !report.removed.iter().any(|s| s == "wylde-boot-live"),
            "live-pid manifest should not be removed, got {:?}",
            report.removed
        );
        assert!(
            path.exists(),
            "live-pid manifest must survive the boot sweep"
        );
    }

    #[serial]
    #[tokio::test]
    async fn boot_sweep_skips_stopped_manifest() {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        let path = dir.join("wylde-boot-stopped.json");
        write_manifest(
            &dir,
            "wylde-boot-stopped",
            json!({
                "service": "wylde-boot-stopped",
                "version": "1.0.0",
                "status": {
                    "pid": 0xFFFFFFFE_u32,
                    "started_at": "2026-05-31T00:00:00Z",
                    "heartbeat": "2026-05-31T00:00:00Z",
                    "state": "stopped"
                }
            }),
        );

        let report = boot_orphan_sweep();
        assert!(
            !report.removed.iter().any(|s| s == "wylde-boot-stopped"),
            "a gracefully-stopped manifest must not be deleted, got {:?}",
            report.removed
        );
        assert!(
            path.exists(),
            "stopped-state manifest must survive the boot sweep"
        );
    }

    #[serial]
    #[tokio::test]
    async fn boot_sweep_nospawn_inspects_but_does_not_delete() {
        // Acquire the state singleton guard too — this test mutates the
        // global no-spawn flag, which `state::tests` also touch. Lock order
        // is ENV_LOCK → state_guard; state::tests only ever take the latter,
        // so no deadlock is possible.
        let _e = ENV_LOCK.lock().await;
        let _s = crate::state::tests::state_guard().await;
        let tmp = TempDir::new().unwrap();
        std::env::set_var("WYLDE_ROOT", tmp.path());

        let dir = manifest_dir();
        let path = dir.join("wylde-boot-nospawn.json");
        write_manifest(
            &dir,
            "wylde-boot-nospawn",
            json!({
                "service": "wylde-boot-nospawn",
                "version": "1.0.0",
                "status": {
                    "pid": 0xFFFFFFFE_u32,
                    "started_at": "2026-05-31T00:00:00Z",
                    "heartbeat": "2026-05-31T00:00:00Z",
                    "state": "alive"
                }
            }),
        );

        crate::state::set_nospawn(true);
        let report = boot_orphan_sweep();
        crate::state::set_nospawn(false);

        assert!(
            report.removed.is_empty(),
            "no-spawn must delete nothing, got removed={:?}",
            report.removed
        );
        assert!(
            report
                .would_remove
                .iter()
                .any(|s| s == "wylde-boot-nospawn"),
            "expected wylde-boot-nospawn in would_remove, got {:?}",
            report.would_remove
        );
        assert!(
            path.exists(),
            "no-spawn must leave the stale manifest in place"
        );
    }

    #[tokio::test]
    async fn start_stop_orphan_sweep_is_idempotent() {
        let _g = ENV_LOCK.lock().await;
        // Use a tight interval so the test isn't slow.
        start_orphan_sweep_with_interval(Duration::from_millis(50));
        start_orphan_sweep_with_interval(Duration::from_millis(50)); // second call is a no-op
                                                                     // Give the task a chance to actually tick once.
        tokio::time::sleep(Duration::from_millis(150)).await;
        stop_orphan_sweep();
        stop_orphan_sweep(); // idempotent
    }
}
