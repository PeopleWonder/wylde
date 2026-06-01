//! Lifecycle daemon parity: Python `Core.Lifecycle.daemon` vs
//! `wylde-lifecycle.exe`.
//!
//! ## Why this needed a no-spawn mode
//!
//! The other three services can be exercised in isolation — the gateway is
//! a standalone HTTP server, the broker and device gate are standalone pipe
//! servers. The lifecycle daemon is different: it is a *supervisor*.
//! Historically, starting either implementation immediately forked the full
//! `tier=core` child set (Memgraph, Voice, device_gate, vram_broker,
//! extension_bridge, gateway), so a parity test would have booted Wylde's
//! entire core stack — far too invasive for an opt-in test, and the result
//! would depend on whatever else is installed on the host.
//!
//! Both daemons ship a **no-spawn mode** (`--no-spawn` /
//! `WYLDE_LIFECYCLE_NOSPAWN=1`): the control + manifest surfaces come up
//! normally but `_start_<service>` forks nothing — each records a
//! "would-have-spawned" entry instead. That makes the control surface
//! exercisable without the core stack, so this test is real.
//!
//! ## Isolated pipe names — runnable while the live stack is up
//!
//! The broker / device-gate suites bind the *canonical* pipe and so must be
//! run with no production instance up. The lifecycle daemon, by contrast,
//! is the long-lived supervisor a developer almost always has running — so
//! requiring it down would make this test impractical to run.
//!
//! Instead each parity daemon binds an **isolated pipe**: the daemons
//! resolve their service name from `WYLDE_LIFECYCLE_PIPE_NAME`, and this
//! test sets it to `wylde-lifecycle-parity-py` / `wylde-lifecycle-parity-rs`.
//! A production daemon on `\\.\pipe\wylde-lifecycle` is therefore never
//! touched — the parity run and the live stack coexist. No-spawn mode also
//! skips the `core.json` manifest write (host-wide shared state), so a
//! parity daemon cannot clobber a live daemon's manifest either.
//!
//! Capture is still **sequential** — a fixed script replayed against a
//! fresh no-spawn Python daemon, then a fresh no-spawn Rust daemon, then
//! the two reply lists diffed. A fresh process per side means each is
//! exercised from identical state, the fair comparison for a supervisor.
//!
//! ## Gated set
//!
//! Every case here is **gated** — a divergence fails the test. The set is
//! the control surface both daemons answer identically in no-spawn mode:
//!
//! * `ping` / `handshake` — the pipe server's built-in control frames.
//! * `lifecycle.status` — the no-spawn mode flag + would-have-spawned set.
//! * `lifecycle.list_services` — the would-have-spawned service map.
//! * `lifecycle.start_service` — drives the no-spawn short-circuit for one
//!   service and returns the synthetic would-have-spawned envelope.
//! * `unknown_action` / `empty_action` — the dispatcher's error envelopes
//!   (`no_action` / `bad_request`).
//! * `service.list` — the registry walk both daemons run against the same
//!   repo root (declarative folder manifests + runtime manifests +
//!   constituent-pipe filter). The Rust daemon is a faithful port of
//!   `Core.Lifecycle.registry`, so the `{services, counts}` envelope must
//!   match byte-for-byte (modulo `error.message` already in `VOLATILE`).
//! * `service.health` / `service.start` / `service.stop` / `service.wake`
//!   for an **unknown service name** — both daemons reject (or no-op,
//!   for stop) without touching subprocess state. Promoted as the safe
//!   error-path slice of the production `service.*` surface; the live
//!   (name-in-yaml) path is not promotable in no-spawn mode because the
//!   Python launcher would actually try to spawn.
//! * `lifecycle.shutdown_all` — drains the six would-have-spawned services;
//!   the envelope (`stopped` list + order, `count`, `launcher_stopped`,
//!   `daemon_managed_*`, `daemon_will_exit`) must match byte-for-byte.
//!
//! ## Coverage caveat
//!
//! No-spawn mode tests the *control* surface and the *state* surface. It
//! does NOT test real child-process supervision — restart-on-crash, orphan
//! reaping, runaway children. That remains future work; a parity test for
//! it would have to fork and kill real processes. This suite makes parity
//! meaningful at the control level, which is what blocked the lifecycle
//! cutover.

#![cfg(feature = "parity")]

use std::time::Duration;

use serde_json::{json, Value};
use wylde_parity::{diff, paths, pipe, proc};

/// Isolated service / pipe names — one per implementation, neither the
/// canonical `wylde-lifecycle`. A production daemon is never disturbed.
const PY_SERVICE: &str = "wylde-lifecycle-parity-py";
const RS_SERVICE: &str = "wylde-lifecycle-parity-rs";

/// A harmless unknown action used to probe pipe readiness — the dispatcher
/// answers `no_action` (a handler-level reply), which `wait_ready` reads as
/// "the server is up". Mutates nothing.
const READY_PROBE: &str = "lifecycle.__parity_ready_probe__";

/// Fields that legitimately differ between the two captures.
///
/// * `error.message` — the dispatcher's `no_action` message embeds the
///   action name via Python's `repr()` (single quotes) vs Rust's `{:?}`
///   (double quotes). The stable `error.code` is NOT normalized.
/// * `error.details` — shape varies on the handler-error path.
/// * `data.service` — the `handshake` frame echoes the daemon's service
///   name, which is the *isolated* pipe name and so differs by design
///   (`wylde-lifecycle-parity-py` vs `-rs`). A test-harness artifact, not a
///   real divergence.
const VOLATILE: &[&str] = &["error.message", "error.details", "data.service"];

/// One pipe round-trip in the replay script.
enum Call {
    /// Dispatched through the `/__action__` envelope.
    Action {
        action: &'static str,
        payload: Value,
    },
    /// A raw pipe method — the server's built-in control frames.
    Method { method: &'static str },
}

/// A gated lifecycle parity case.
struct LcCase {
    name: &'static str,
    call: Call,
}

/// The fixed replay script. `shutdown_all` MUST stay last: the daemon
/// schedules its own exit ~500ms after answering it.
fn cases() -> Vec<LcCase> {
    vec![
        // ── Pipe-server built-in control frames ─────────────────────────
        LcCase {
            name: "ping",
            call: Call::Method {
                method: "/__ping__",
            },
        },
        LcCase {
            name: "handshake",
            call: Call::Method {
                method: "/__handshake__",
            },
        },
        // ── No-spawn control + state surface ────────────────────────────
        LcCase {
            name: "lifecycle_status",
            call: Call::Action {
                action: "lifecycle.status",
                payload: json!({}),
            },
        },
        LcCase {
            name: "lifecycle_list_services",
            call: Call::Action {
                action: "lifecycle.list_services",
                payload: json!({}),
            },
        },
        LcCase {
            name: "lifecycle_start_service",
            call: Call::Action {
                action: "lifecycle.start_service",
                payload: json!({ "name": "wylde-voice" }),
            },
        },
        // ── Action-dispatch error envelopes ─────────────────────────────
        LcCase {
            name: "unknown_action",
            call: Call::Action {
                action: "lifecycle.__parity_nonexistent__",
                payload: json!({}),
            },
        },
        LcCase {
            name: "empty_action",
            call: Call::Action {
                action: "",
                payload: json!({}),
            },
        },
        // ── Production service.* surface (read-only / error-path slice) ─
        //
        // `service.list` walks the same filesystem on both daemons. The
        // unknown-name cases below exercise the error envelopes without
        // mutating state — using a guaranteed-bogus name keeps the
        // launcher path (Python) and the daemon-managed dispatch (Rust)
        // both at the rejection step.
        LcCase {
            name: "service_list",
            call: Call::Action {
                action: "service.list",
                payload: Value::Null,
            },
        },
        LcCase {
            name: "service_health_unknown",
            call: Call::Action {
                action: "service.health",
                payload: json!({ "name": "wylde-parity-bogus" }),
            },
        },
        LcCase {
            name: "service_start_unknown",
            call: Call::Action {
                action: "service.start",
                payload: json!({ "name": "wylde-parity-bogus" }),
            },
        },
        LcCase {
            name: "service_stop_unknown",
            call: Call::Action {
                action: "service.stop",
                payload: json!({ "name": "wylde-parity-bogus" }),
            },
        },
        LcCase {
            name: "service_wake_unknown",
            call: Call::Action {
                action: "service.wake",
                payload: json!({ "name": "wylde-parity-bogus" }),
            },
        },
        // ── The would-have-spawned teardown — MUST be last ──────────────
        LcCase {
            name: "lifecycle_shutdown_all",
            call: Call::Action {
                action: "lifecycle.shutdown_all",
                payload: Value::Null,
            },
        },
    ]
}

/// Replay the script against one no-spawn daemon bound to `service` and
/// return the `(case_name, reply_json)` list. The daemon is killed before
/// returning.
async fn capture(
    label: &str,
    cmd: std::process::Command,
    service: &str,
    cases: &[LcCase],
) -> Vec<(&'static str, Value)> {
    let mut svc = proc::Service::spawn(label, cmd).expect("spawn lifecycle daemon");
    let ready = pipe::wait_ready(service, READY_PROBE, Duration::from_secs(25)).await;
    assert!(
        ready,
        "{label}: lifecycle pipe ({service}) never became ready \
         (process exited early: {})",
        svc.has_exited(),
    );

    let mut out: Vec<(&'static str, Value)> = Vec::new();
    for case in cases {
        let reply = match &case.call {
            Call::Action { action, payload } => {
                pipe::capture(service, action, payload.clone()).await
            }
            Call::Method { method } => pipe::capture_method(service, method, json!({})).await,
        };
        out.push((case.name, reply));
    }

    drop(svc); // kill the daemon before the next capture
    out
}

#[tokio::test]
async fn lifecycle_control_parity() {
    paths::require_artifact(
        &paths::venv_python(),
        "create the Wylde virtualenv (.venv) with the service dependencies",
    );
    paths::require_artifact(
        &paths::rust_release_bin("wylde-lifecycle"),
        "run `cargo build --release` in the rust/ workspace",
    );

    // Pre-flight: the daemons bind isolated pipes, NOT `\\.\pipe\wylde-lifecycle`,
    // so a production lifecycle daemon does not block this test. We only
    // guard the isolated names — a leftover from a crashed prior parity run
    // would make the capture non-deterministic.
    for service in [PY_SERVICE, RS_SERVICE] {
        assert!(
            !pipe::pipe_in_use(service, READY_PROBE).await,
            "a daemon is already bound to the parity pipe \\\\.\\pipe\\{service} \
             — a prior parity run left it behind; kill it before re-running",
        );
    }

    let cases = cases();

    // Both daemons launch with `--no-spawn` (control surface up, no
    // tier=core children) and an isolated pipe via WYLDE_LIFECYCLE_PIPE_NAME.
    let mut py_cmd = proc::python_module("Core.Lifecycle.daemon");
    py_cmd
        .arg("--no-spawn")
        .env("WYLDE_LIFECYCLE_PIPE_NAME", PY_SERVICE);
    let python = capture("python lifecycle", py_cmd, PY_SERVICE, &cases).await;

    let mut rs_cmd = proc::rust_binary("wylde-lifecycle");
    rs_cmd
        .arg("--no-spawn")
        .env("WYLDE_LIFECYCLE_PIPE_NAME", RS_SERVICE);
    let rust = capture("rust lifecycle", rs_cmd, RS_SERVICE, &cases).await;

    assert_eq!(
        python.len(),
        rust.len(),
        "capture scripts produced different case counts",
    );

    let mut failures: Vec<String> = Vec::new();
    let mut failure_names: Vec<&str> = Vec::new();
    let mut passed: Vec<&str> = Vec::new();

    for ((py_name, py_val), (rs_name, rs_val)) in python.iter().zip(&rust) {
        assert_eq!(py_name, rs_name, "case order mismatch");
        match diff::compare(py_name, py_val, rs_val, VOLATILE) {
            Ok(()) => passed.push(py_name),
            Err(report) => {
                failure_names.push(py_name);
                failures.push(report);
            }
        }
    }

    eprintln!("\n=== Lifecycle parity ===");
    eprintln!("gated cases at parity ({}): {passed:?}", passed.len());
    if failure_names.is_empty() {
        eprintln!("gated cases diverged: none");
    } else {
        eprintln!(
            "gated cases diverged ({}): {failure_names:?}",
            failure_names.len()
        );
    }

    assert!(
        failures.is_empty(),
        "{} gated lifecycle case(s) diverged ({:?}):\n\n{}",
        failures.len(),
        failure_names,
        failures.join("\n\n"),
    );
}
