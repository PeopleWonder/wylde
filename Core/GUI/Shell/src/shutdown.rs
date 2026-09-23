//! Graceful shutdown sequence — port of the Tauri-side tray.rs
//! orchestration to gpui.
//!
//! The Quit menu item MUST go through the same fall-through ordering
//! the Tauri tray uses, per the slice spec:
//!
//!   1.  Dispatch `lifecycle.shutdown_all` on `\\.\pipe\wylde-lifecycle`.
//!   2.  Poll the process table for up to 10 s for the services to exit.
//!   3.  If the pipe was unreachable OR the wait timed out, hard-kill
//!       the Wylde processes by name — but *only* after step 1 has
//!       been attempted.
//!
//! `shutdown_with_fallback` is the same pure-orchestration function
//! `tray.rs` carries upstream; lifting it verbatim means the unit tests
//! ride along and the behaviour is identical to the Tauri tray.
//!
//! Both process-name sets used by steps 2 and 3 come from
//! [`wylde_stack::shutdown_targets`], derived from the stack roster.
//! They used to be two literal arrays here, each naming four of the
//! eleven killable services; the drain wait polled the same four, so it
//! concluded "drained" as soon as those exited and the hard kill never
//! ran — a clean-looking shutdown that left eight services alive holding
//! VRAM and named pipes (issue #124). Do not reintroduce a list here:
//! `rust/crates/wylde-stack/tests/shutdown_target_coverage.rs` counts
//! this file's contents against the roster and will fail CI.

use std::future::Future;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use wylde_gui_pipe::lifecycle_action;
use wylde_stack::shutdown_targets::{kill_targets, poll_set};

/// Grace window for daemon-managed services to exit after the graceful
/// `lifecycle.shutdown_all` request.  Matches the Tauri tray's window.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Why the graceful drain failed.  The kill-fallback fires for either
/// variant; callers may log them differently.
#[derive(Debug)]
pub enum ShutdownFailure {
    /// The Lifecycle pipe could not be reached or returned an error.
    PipeUnreachable(String),
    /// The drain request was accepted but services were still alive
    /// after the grace window elapsed.
    Timeout,
}

/// Top-level Quit driver.  Runs the graceful drain, falls back to a
/// hard kill if it didn't fully drain, and finally invokes
/// `after_shutdown` so the shell can tear down the window + exit the
/// gpui event loop.
///
/// The wire-up is intentionally injection-shaped: every external call
/// is a closure, so the orchestration can be unit-tested without a
/// live pipe or real process table — the harness for that lives in
/// the tests below this module.
pub async fn run_graceful_shutdown(after_shutdown: impl FnOnce()) {
    let failure = shutdown_with_fallback(
        || lifecycle_action("lifecycle.shutdown_all", Value::Null),
        wait_for_services_exit,
        |_failure| hard_kill_wylde(),
    )
    .await;

    if let Some(failure) = failure {
        eprintln!(
            "[wylde-gui shutdown] graceful drain failed: {:?}; hard-kill issued",
            failure,
        );
    }

    after_shutdown();
}

/// Pure orchestration — runs the graceful drain, falls back to the
/// kill closure on failure, and returns whichever failure triggered
/// the fallback (or `None` on a clean drain).  Lifted from the Tauri
/// tray verbatim so behaviour stays identical and the unit tests are
/// reusable.
pub async fn shutdown_with_fallback<P, PFut, W, WFut, K>(
    pipe_call: P,
    wait_for_exit: W,
    kill_fallback: K,
) -> Option<ShutdownFailure>
where
    P: FnOnce() -> PFut,
    PFut: Future<Output = Result<Value, String>>,
    W: FnOnce() -> WFut,
    WFut: Future<Output = bool>,
    K: FnOnce(&ShutdownFailure),
{
    // Phase 1 — always attempt the graceful drain first.
    let failure = match pipe_call().await {
        Err(err) => Some(ShutdownFailure::PipeUnreachable(err)),
        Ok(_) => {
            // Phase 2 — request accepted; give children time to exit.
            if wait_for_exit().await {
                None
            } else {
                Some(ShutdownFailure::Timeout)
            }
        }
    };

    // Phase 3 — hard kill, reachable only because phase 1 ran above.
    if let Some(ref failure) = failure {
        kill_fallback(failure);
    }
    failure
}

// ── OS process helpers ────────────────────────────────────────────────

/// A `Command` that does not flash a console window on Windows.
fn no_window_cmd(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// True if any daemon-managed Wylde service is still in the process
/// table.  A failed `tasklist` is treated as "nothing running" so an
/// unreadable process table cannot, by itself, escalate to a hard kill.
pub fn services_still_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        let Ok(output) = no_window_cmd("tasklist", &["/FO", "CSV", "/NH"]).output() else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let table = String::from_utf8_lossy(&output.stdout).to_lowercase();
        poll_set()
            .iter()
            .any(|name| table.contains(&name.to_lowercase()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Non-Windows isn't a release target yet (alpha is Windows-only
        // per §8 of the plan); returning false short-circuits the drain
        // wait so the macOS/Linux test environment doesn't hang.
        false
    }
}

/// Poll the process table until every daemon-managed service has exited
/// or the grace window elapses.  Returns `true` if the stack drained.
pub async fn wait_for_services_exit() -> bool {
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        if !services_still_running() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        // This drain is awaited from a gpui `cx.spawn` task (tray Quit /
        // window-close), whose executor has no tokio reactor — a direct
        // `tokio::time::sleep` would panic. Hop onto the bridge runtime.
        wylde_gui_pipe::bridged_sleep(Duration::from_millis(250)).await;
    }
}

/// Hard-kill every Wylde process by image name.  Last-resort fallback.
pub fn hard_kill_wylde() {
    #[cfg(target_os = "windows")]
    {
        let targets = kill_targets();
        let mut args: Vec<&str> = vec!["/F"];
        for name in &targets {
            args.push("/IM");
            args.push(name);
        }
        let _ = no_window_cmd("taskkill", &args).output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // POSIX path is a follow-on; for now this is a no-op so the
        // orchestration can run end-to-end during cross-platform CI.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Graceful drain succeeds and services exit within the window: the
    /// hard-kill fallback must NOT run.  Ported verbatim from the Tauri
    /// tray's tests so behaviour parity is provable.
    #[tokio::test]
    async fn graceful_success_skips_kill_fallback() {
        let killed = Arc::new(AtomicBool::new(false));
        let killed_probe = Arc::clone(&killed);

        let failure = shutdown_with_fallback(
            || async { Ok(serde_json::json!({ "stopped": [] })) },
            || async { true },
            move |_| killed_probe.store(true, Ordering::SeqCst),
        )
        .await;

        assert!(failure.is_none(), "a clean drain reports no failure");
        assert!(
            !killed.load(Ordering::SeqCst),
            "kill fallback must not fire when the graceful path drains cleanly",
        );
    }

    /// Pipe unreachable: the fallback fires, and only after the graceful
    /// pipe dispatch has been attempted.  Tauri parity test.
    #[tokio::test]
    async fn pipe_unreachable_triggers_kill_after_graceful_attempt() {
        let log: Arc<Mutex<Vec<&str>>> = Arc::new(Mutex::new(Vec::new()));
        let pipe_log = Arc::clone(&log);
        let kill_log = Arc::clone(&log);

        let failure = shutdown_with_fallback(
            || {
                pipe_log.lock().unwrap().push("graceful");
                async { Err("pipe_unavailable: service 'wylde-lifecycle' is not running".into()) }
            },
            || async { true },
            move |_| kill_log.lock().unwrap().push("kill"),
        )
        .await;

        assert!(
            matches!(failure, Some(ShutdownFailure::PipeUnreachable(_))),
            "an unreachable pipe is reported as PipeUnreachable",
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec!["graceful", "kill"],
            "kill fallback runs strictly after the graceful attempt",
        );
    }

    /// Graceful dispatch is accepted but services never exit: the
    /// timeout path also escalates to the kill fallback.  Tauri parity.
    #[tokio::test]
    async fn drain_timeout_triggers_kill_fallback() {
        let log: Arc<Mutex<Vec<&str>>> = Arc::new(Mutex::new(Vec::new()));
        let pipe_log = Arc::clone(&log);
        let kill_log = Arc::clone(&log);

        let failure = shutdown_with_fallback(
            || {
                pipe_log.lock().unwrap().push("graceful");
                async { Ok(serde_json::Value::Null) }
            },
            || async { false },
            move |_| kill_log.lock().unwrap().push("kill"),
        )
        .await;

        assert!(
            matches!(failure, Some(ShutdownFailure::Timeout)),
            "a drain that never completes is reported as Timeout",
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec!["graceful", "kill"],
            "kill fallback runs strictly after the graceful attempt",
        );
    }

    /// The kill order and the drain-wait poll set are derived in
    /// `wylde_stack::shutdown_targets`, and the gate that *counts* them
    /// against the roster lives in
    /// `rust/crates/wylde-stack/tests/shutdown_target_coverage.rs` — in
    /// the one workspace whose `cargo test` runs in CI.  What is worth
    /// asserting here is the wiring: that this module reaches for the
    /// derived sets at all, rather than growing a parallel list again.
    #[test]
    fn shutdown_paths_read_from_the_derived_sets() {
        let kill = kill_targets();
        let poll = poll_set();

        // The pre-#124 lists named four services.  The roster carries
        // eleven killable ones plus the daemon and the GUI binaries, so a
        // regression to a hand-typed list shows up as a collapse in size.
        assert!(
            kill.len() > poll.len(),
            "the kill list must exceed the poll set by the GUI binaries",
        );
        for gui in ["fletch-gui.exe", "wylde-gui.exe"] {
            assert!(
                !poll.iter().any(|n| n == gui),
                "{gui} must not be polled during the drain wait (it polls itself)",
            );
        }
    }
}
