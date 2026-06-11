//! Long-running Lifecycle daemon — Rust port of
//! `Core/Lifecycle/daemon.py::serve_forever`.
//!
//! Runs the boot-time service spawns once, then stays alive serving
//! the `\\.\pipe\wylde-lifecycle` named pipe so the GUI (and anything
//! else on the local box) can drive shutdown via the action surface
//! installed by [`crate::control::register_with_ipc`].
//!
//! ## Parity with the Python daemon
//!
//! The Python `serve_forever` also runs a discovery sweep, the
//! launcher (which spawns non-core services from `services.yaml`),
//! starts the harness pipe in-process, and hosts the memory
//! scheduler. None of those have Rust-side equivalents yet:
//!
//! * Discovery + launcher are Python-only — services.yaml is read by
//!   Python tools and rewritten by `Core/Lifecycle/discovery.py`.
//!   (N8N, the one service that used to ride that path, became the
//!   daemon-managed `wylde-n8n` in taxonomy reorg TX S3 — the n8n
//!   daemon itself stays external and user-managed.) The extension
//!   bridge is a daemon-managed tier=core service (spawned directly
//!   below), so it boots under either daemon.
//! * The harness pipe (`\\.\pipe\wylde-harness`) is hosted by Python's
//!   `Core/harness/server.py`. The Rust daemon doesn't bring it up;
//!   chat-surface clients will see `pipe_unavailable` until the
//!   harness gets its own port.
//! * The memory scheduler is an in-process Python thread; see
//!   [`crate::state::services::start_memory_scheduler`].
//!
//! The strangler-fig (launch-time, via `WYLDE_LIFECYCLE_IMPL`) lets
//! the Wylde user pick the Python daemon for sessions that need any of those
//! subsystems. The Rust daemon is the right choice when the goal is a
//! minimal, fast supervisor for the tier=core services that *do* have
//! Rust binaries (vram_broker, device_gate, gateway).
//!
//! ## No-spawn mode (test / parity ONLY)
//!
//! `WYLDE_LIFECYCLE_NOSPAWN=1` or the `--no-spawn` CLI flag brings the
//! control + manifest surfaces up without forking ANY tier=core child —
//! see the no-spawn warning in [`crate::state`]. It is the byte-for-byte
//! counterpart of the Python daemon's no-spawn mode and exists only for
//! the cross-language parity suite. **Never enable it in production.**
//!
//! `WYLDE_LIFECYCLE_PIPE_NAME` overrides the service name the daemon
//! binds its pipe / identifies as (default `wylde-lifecycle` →
//! `\\.\pipe\wylde-lifecycle`). The parity suite sets
//! `wylde-lifecycle-parity-rs` so its daemons run on isolated pipes and
//! never collide with a production daemon on the canonical pipe. Like
//! no-spawn, this is **test/parity only** — never set it in production.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use tracing::Level;
use wylde_shared::ipc::serve_forever_background;
use wylde_shared::logging::configure_logging;

use crate::control;
use crate::state::{
    nospawn_snapshot, register_core_manifest, register_stop_event, services, set_nospawn,
    start_orphan_sweep, stop_all_daemon_managed,
};

const SERVICE_NAME: &str = "wylde-lifecycle";

/// Resolve the service name the daemon binds its pipe / identifies as.
///
/// Defaults to `wylde-lifecycle`. `WYLDE_LIFECYCLE_PIPE_NAME` overrides
/// it; the cross-language parity suite sets `wylde-lifecycle-parity-rs`
/// so the parity Python and Rust daemons run on isolated pipes and never
/// collide with a production daemon on the canonical pipe. **Test/parity
/// only — never set this in production.**
fn resolve_pipe_service_name() -> String {
    std::env::var("WYLDE_LIFECYCLE_PIPE_NAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SERVICE_NAME.to_string())
}

/// Resolve no-spawn mode from the `--no-spawn` CLI flag or the
/// `WYLDE_LIFECYCLE_NOSPAWN` env var. TEST/PARITY ONLY — see the no-spawn
/// warning in [`crate::state`].
fn detect_nospawn() -> bool {
    if std::env::args().any(|a| a == "--no-spawn") {
        return true;
    }
    matches!(
        std::env::var("WYLDE_LIFECYCLE_NOSPAWN")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Boot the daemon, register actions, serve the pipe, block until
/// shutdown. Returns 0 on a clean exit (matches POSIX conventions).
pub async fn serve_forever() -> Result<i32> {
    // Service name the pipe surface binds / identifies as. Normally
    // `wylde-lifecycle`; the parity suite overrides it via
    // WYLDE_LIFECYCLE_PIPE_NAME so its daemons get isolated pipes.
    let service_name = resolve_pipe_service_name();

    configure_logging(Some(&service_name), Level::INFO);
    tracing::info!("daemon: booting Lifecycle controller (rust impl)");

    // No-spawn mode (test/parity only). When set, the control + manifest
    // surfaces still come up but every `start_<service>` short-circuits to
    // a "would-have-spawned" record instead of forking a child. See the
    // loud warning in `crate::state`.
    let nospawn = detect_nospawn();
    set_nospawn(nospawn);
    if nospawn {
        tracing::warn!(
            "daemon: NO-SPAWN MODE ACTIVE — control + manifest surfaces will \
             come up but NO tier=core children will be forked. This mode is \
             for testing/parity ONLY and must never run in production."
        );
    }

    // Phase 1 — register action handlers BEFORE starting the pipe so
    // any client that races us doesn't see "no_action" for shutdown.
    control::register_with_ipc();

    // Phase 2 — start the pipe accept loop in the background. We
    // ignore the JoinHandle: the loop runs until shutdown and tokio
    // will abort it when the runtime drains.
    let _pipe_task = serve_forever_background(&service_name);

    // Phase 2b — publish Core's runtime manifest so `service.list`
    // can render a fresh Core entry. Starts the 60s heartbeat that
    // matches the Wylde user's "Heartbeat every 60s in this task's own work"
    // constraint.
    //
    // Skipped under no-spawn: core.json is host-wide shared state.
    // Writing (and, at shutdown, deleting) it would clobber a production
    // daemon's manifest — so a parity run stays runnable while the real
    // stack is up.
    if !nospawn {
        if let Err(e) = register_core_manifest() {
            tracing::error!("daemon: register_core_manifest raised: {:#}", e);
        }
    }

    // Phase 2b-sweep — synchronous orphan sweep BEFORE any start_<service>.
    // A manifest left behind by an ungraceful prior exit (Ctrl-C / taskkill
    // / SIGKILL) still marks its service "alive" with a now-dead pid. The
    // recurring 60s sweep (Phase 2d) only fires AFTER the boot spawns, so
    // without this one-shot a stale manifest survives a lifecycle restart
    // and the affected service stays dark (the harness / extension_bridge /
    // ollama outage on 2026-05-31). Running it
    // here self-heals on every boot. Under no-spawn it inspects + logs but
    // deletes nothing — core.json was just (re)written above with this
    // daemon's live pid, so the sweep skips it.
    let boot_sweep = crate::state::boot_orphan_sweep();
    tracing::info!(
        "daemon: boot orphan-sweep checked {} manifest(s), removed {:?}{}",
        boot_sweep.checked,
        boot_sweep.removed,
        if nospawn {
            format!(" (no-spawn would-remove {:?})", boot_sweep.would_remove)
        } else {
            String::new()
        }
    );

    // Phase 2c — spawn the daemon-managed tier=core services. Each
    // start_<service> logs its own failure path; we don't bail on
    // individual failures because a partial bring-up is still useful
    // (e.g. broker comes up, device_gate fails — the user can still
    // drive the broker via the GUI).
    if let Err(e) = services::start_memgraph().await {
        tracing::error!("daemon: start_memgraph raised: {:#}", e);
    }
    if let Err(e) = services::start_memory_scheduler().await {
        tracing::error!("daemon: start_memory_scheduler raised: {:#}", e);
    }
    if let Err(e) = services::start_vram_broker().await {
        tracing::error!("daemon: start_vram_broker raised: {:#}", e);
    }
    if let Err(e) = services::start_voice().await {
        tracing::error!("daemon: start_voice raised: {:#}", e);
    }
    if let Err(e) = services::start_device_gate().await {
        tracing::error!("daemon: start_device_gate raised: {:#}", e);
    }
    // Extension bridge before Gateway — Gateway dispatches browser-
    // extension calls through `\\.\pipe\wylde-extension-bridge`.
    if let Err(e) = services::start_extension_bridge().await {
        tracing::error!("daemon: start_extension_bridge raised: {:#}", e);
    }
    // wylde-ollama AFTER the broker (depends on it for VRAM leases)
    // but BEFORE the gateway/harness (which call into it).
    if let Err(e) = services::start_ollama().await {
        tracing::error!("daemon: start_ollama raised: {:#}", e);
    }
    if let Err(e) = services::start_gateway().await {
        tracing::error!("daemon: start_gateway raised: {:#}", e);
    }
    // wylde-harness — Phase 5 chat-turn driver. Slice 5.D (2026-05-25)
    // flipped the default impl from `python` to `rust`: this start
    // now spawns the Rust `wylde-harness.exe` binary which exposes
    // the full chat.* surface over `\\.\pipe\wylde-harness`. Set
    // `WYLDE_WYLDE_HARNESS_IMPL=python` to revert to the in-process
    // Python driver inside the existing Python harness service
    // (in which case this start is a no-op).
    if let Err(e) = services::start_harness().await {
        tracing::error!("daemon: start_harness raised: {:#}", e);
    }
    // wylde-treesitter — greenfield structural-parsing sidecar. A leaf
    // service (no dependency on broker/gateway), so ordering is free;
    // spawned last in the core tier. Default impl is rust.
    if let Err(e) = services::start_treesitter().await {
        tracing::error!("daemon: start_treesitter raised: {:#}", e);
    }
    // wylde-workspaces — Thought Bubble System Phase 0 service. Spawned
    // LAST: its ingest pipeline consumes wylde-ollama (embeddings),
    // wylde-treesitter (chunk/extract over the pipe), and Memgraph (Bolt
    // graph writes), all of which are started above. Greenfield Rust,
    // default impl rust. A missing binary is non-fatal — every consumer
    // degrades gracefully when it's absent (Slice 0d).
    if let Err(e) = services::start_workspaces().await {
        tracing::error!("daemon: start_workspaces raised: {:#}", e);
    }
    // wylde-n8n — optional pipe surface over the external, user-managed
    // n8n daemon (taxonomy reorg TX S3). A leaf service: nothing in core
    // depends on it (the harness verb layer fail-softs), and it launches
    // nothing itself — the n8n daemon is the user's to run. A missing
    // binary is non-fatal.
    if let Err(e) = services::start_n8n().await {
        tracing::error!("daemon: start_n8n raised: {:#}", e);
    }

    if nospawn {
        tracing::info!(
            "daemon: NO-SPAWN — would-have-spawned: {:?}",
            nospawn_snapshot()
        );
    }

    // Phase 2d — start the orphan-detection sweep. Background task
    // ticks every 60s walking data/manifests/*.json and flipping any
    // alive-marked manifest with a dead pid to dead-orphan.
    //
    // Skipped under no-spawn: there are no real children to orphan, and
    // the sweep would otherwise rewrite unrelated manifests on the host.
    if !nospawn {
        start_orphan_sweep();
    }

    // Phase 3 — block until either ctrl-c or the
    // service.shutdown_all action flips the stop notify.
    let stop = Arc::new(Notify::new());
    register_stop_event(stop.clone());
    let pipe_suffix = service_name.strip_prefix("wylde-").unwrap_or(&service_name);
    tracing::info!(r"daemon: ready (\\.\pipe\wylde-{})", pipe_suffix);

    tokio::select! {
        _ = stop.notified() => {
            tracing::info!("daemon: shutdown_all action requested exit");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("daemon: ctrl-c received, draining children");
            // Mirror the Python signal handler: tear down the children
            // ourselves rather than relying on kill_on_drop, so each
            // service gets its graceful CTRL_BREAK + wait window.
            let summary = stop_all_daemon_managed().await;
            tracing::info!(
                "daemon: ctrl-c teardown drained {} services ({} failures)",
                summary.count,
                summary.failed.len()
            );
        }
    }

    // Action-triggered shutdown path also runs through
    // stop_all_daemon_managed — but it's already been called by the
    // action handler before flipping the notify. Avoid calling it
    // again here (it's idempotent, but the second call would log a
    // "no services" empty pass). The ctrl-c branch above already
    // drained for that path.

    // Give the pipe task a beat to finish flushing any in-flight
    // replies before the runtime drains. Matches the 500ms exit delay
    // in the Python daemon's request_daemon_exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    tracing::info!("daemon: exit");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test owns a distinct env var, so no two tests race on the same
    // key; the mutations within a test are sequential.

    #[test]
    fn resolve_pipe_service_name_default_and_override() {
        std::env::remove_var("WYLDE_LIFECYCLE_PIPE_NAME");
        assert_eq!(resolve_pipe_service_name(), "wylde-lifecycle");

        std::env::set_var("WYLDE_LIFECYCLE_PIPE_NAME", "wylde-lifecycle-parity-rs");
        assert_eq!(resolve_pipe_service_name(), "wylde-lifecycle-parity-rs");

        // Blank / whitespace-only falls back to the canonical name.
        std::env::set_var("WYLDE_LIFECYCLE_PIPE_NAME", "   ");
        assert_eq!(resolve_pipe_service_name(), "wylde-lifecycle");

        std::env::remove_var("WYLDE_LIFECYCLE_PIPE_NAME");
    }

    #[test]
    fn detect_nospawn_reads_env() {
        std::env::remove_var("WYLDE_LIFECYCLE_NOSPAWN");
        assert!(!detect_nospawn());

        for truthy in ["1", "true", "yes", "on"] {
            std::env::set_var("WYLDE_LIFECYCLE_NOSPAWN", truthy);
            assert!(detect_nospawn(), "{truthy:?} should enable no-spawn");
        }
        std::env::set_var("WYLDE_LIFECYCLE_NOSPAWN", "0");
        assert!(!detect_nospawn());

        std::env::remove_var("WYLDE_LIFECYCLE_NOSPAWN");
    }
}
