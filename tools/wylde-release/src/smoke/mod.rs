//! L2 (cold-start smoke) + L3 (service health) — the launch-and-verify gate.
//!
//! ## Why this exists
//!
//! Alpha shipped broken repeatedly, and **every** defect passed a green
//! 1167-test suite because unit tests verify *code*, not the *assembled,
//! running system*: a GUI that was never built, Neo4j booting an empty graph
//! from a `${VAR}` folder, a RAG service that couldn't start, a service that
//! segfaulted on a clean runner. The only thing that would have caught them is
//! *launching the shipped artifacts and exercising them*. That is this module.
//!
//! ## The ladders (each check is a discrete, individually-reported verdict)
//!
//! * **L2 cold-start** — do the shipped artifacts actually LAUNCH? We start the
//!   real daemon binary (not `cargo run`) from a **neutral working directory**
//!   so it proves env-var resolution rather than cwd luck, and assert it stays
//!   up and binds `\\.\pipe\wylde-lifecycle`; likewise the GUI process (alive +
//!   no panic — window *content* is the CI panel-walk's job, not ours).
//! * **L3 service health** — is the assembled system FUNCTIONAL? The daemon
//!   discovers its services; the VRAM broker answers; Ollama has the reasoner +
//!   embed models; **Memgraph holds real data** (not just an open port); RAG
//!   answers a query; a chat turn completes; a memory round-trips.
//! * **L5 shipped-config** — did we ship the right *switches*? A system can
//!   launch and be perfectly healthy while shipping an experimental tier turned
//!   on. `l5.reasoning_disabled` (issue #27) asks the running harness for its
//!   effective reasoning config and fails unless `enabled:false`. (L5's other
//!   half, the reasoning-eval guardrail, is the benchmark gate in
//!   [`crate::bench`].)
//!
//! ## Fail closed, clean up, honest
//!
//! Every check that cannot determine a healthy state FAILs (never "assume up").
//! Everything spawned here is torn down (graceful `service.shutdown_all`, then a
//! `taskkill /T` backstop) so a preflight never leaves orphan processes or
//! collides on pipes with a parallel session. If a daemon is already running we
//! *attach* to it rather than spawn/teardown a sibling's stack.

pub mod memgraph;
pub mod pipe;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// The reasoner + embed models the assembled system needs present in Ollama.
/// Mirror `wylde_harness::turn::reasoning::config::{DEFAULT_REASONER_MODEL,
/// DEFAULT_EMBED_MODEL}`; overridable so a re-tag doesn't wrongly fail the gate.
const DEFAULT_REASONER_MODEL: &str = "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS";
const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";

/// The core pipe services whose liveness L3.2 proves by a direct `/__ping__` to
/// their own pipe — ground truth, independent of the daemon's `service.list`
/// bookkeeping (which can lag or miss a running service). These two are the
/// load-bearing core services without a dedicated deeper check of their own;
/// the VRAM broker (L3.3), Ollama (L3.4) and Memgraph (L3.5) each get one.
/// voice/vpn/n8n are optional and deliberately excluded.
const CORE_PIPE_SERVICES: &[&str] = &["wylde-harness", "wylde-workspaces"];

// Per-check timeouts. Generous enough to absorb a cold cache, bounded so a hang
// fails the gate rather than wedging it (a hung preflight is a skipped one).
const DAEMON_LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
const GUI_ALIVE_WINDOW: Duration = Duration::from_secs(8);
const PIPE_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to let the service tree converge after the daemon binds its pipe.
/// The daemon binds `\\.\pipe\wylde-lifecycle` *then* spawns services
/// asynchronously (memgraph → broker → ollama → harness → …), so a health check
/// fired the instant the pipe appears races the spawn. We poll `service.list`
/// until the critical services are up, or fail closed at this deadline.
const SERVICES_CONVERGE_TIMEOUT: Duration = Duration::from_secs(90);
const OLLAMA_TIMEOUT_SECS: u64 = 10;
const MEMGRAPH_TIMEOUT: Duration = Duration::from_secs(10);
const CARGO_CHECK_TIMEOUT: Duration = Duration::from_secs(900);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// One check's verdict. Maps 1:1 onto the receipt's `GateOutcome`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    /// The check was deliberately not run (operator flag). Recorded honestly;
    /// the caller decides whether a skip keeps the receipt from going green
    /// (it does, unless explicitly allowed — fail-closed).
    Skip,
}

impl CheckStatus {
    pub fn tag(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skip => "SKIP",
        }
    }
}

/// The result of one check — a stable gate key (for the receipt), a human
/// title, the verdict, and a one-line diagnosis so a failure is actionable.
#[derive(Clone, Debug)]
pub struct CheckResult {
    /// Stable receipt gate key, e.g. `l3.memgraph_has_data`.
    pub key: &'static str,
    /// Human title with the ladder id, e.g. `L3.5 memgraph-has-data`.
    pub title: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

impl CheckResult {
    fn pass(key: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self {
            key,
            title,
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    fn fail(key: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self {
            key,
            title,
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
    fn skip(key: &'static str, title: &'static str, detail: impl Into<String>) -> Self {
        Self {
            key,
            title,
            status: CheckStatus::Skip,
            detail: detail.into(),
        }
    }
    /// Collapse a `Result` into a pass/fail verdict — the fail-closed idiom used
    /// by every check: an `Err` (including a timeout or "couldn't determine") is
    /// a FAIL, never a silent pass.
    fn from_result(
        key: &'static str,
        title: &'static str,
        ok_detail: impl Into<String>,
        result: Result<()>,
    ) -> Self {
        match result {
            Ok(()) => Self::pass(key, title, ok_detail),
            Err(e) => Self::fail(key, title, format!("{e:#}")),
        }
    }
}

/// How the daemon came to be running for this smoke run.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DaemonMode {
    /// We spawned it from cold (the real L2 cold-start proof).
    ColdStarted,
    /// It was already running; we attached and did not tear it down.
    Attached,
    /// Neither — it isn't up and we couldn't/ wouldn't start it.
    Absent,
}

/// The whole smoke run: every check verdict, plus whether we cold-started.
pub struct SmokeOutcome {
    pub checks: Vec<CheckResult>,
    pub daemon_mode: DaemonMode,
}

impl SmokeOutcome {
    /// Every check passed (the bar for a launch-verified receipt).
    pub fn all_passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.status == CheckStatus::Pass)
    }
    pub fn any_failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }
    pub fn skipped(&self) -> Vec<&CheckResult> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Skip)
            .collect()
    }
}

/// Knobs for a smoke run.
pub struct SmokeOpts {
    /// Repo/install root — WYLDE_ROOT for a cold-start, and where artifacts live.
    pub repo_root: PathBuf,
    /// Require an already-running daemon; never spawn (and never tear down).
    pub attach_only: bool,
    /// Cold-start the daemon in NO-SPAWN parity mode: it binds the pipe but does
    /// not fork the service tree. For validating this gate's own plumbing — the
    /// service-health checks will (correctly) fail, since nothing is running.
    pub nospawn: bool,
    /// Skip the GUI cold-start (L2.2) — e.g. no desktop session, or don't want a
    /// window to flash up.
    pub skip_gui: bool,
    /// Skip the slow cargo-driven functional checks (RAG / chat / memory).
    pub skip_functional: bool,
    /// A chat turn already succeeded upstream (the preflight benchmark's
    /// reasoning run) — reuse that verdict for L3.7 instead of re-running the
    /// costly eval. `None` ⇒ run a `reasoning_eval --smoke` turn ourselves.
    pub chat_turn_ok: Option<bool>,
}

/// Run the full L2/L3 gate. Never panics; every failure becomes a FAIL verdict.
/// Always tears down anything it spawned before returning.
pub fn run(opts: &SmokeOpts) -> SmokeOutcome {
    let mut checks = Vec::new();
    let neutral = neutral_cwd();

    // ── L2.1 daemon cold-start (or attach) ────────────────────────────────
    let (daemon_result, daemon_mode, daemon_child) = launch_or_attach_daemon(opts, &neutral);
    checks.push(daemon_result);

    // ── L2.2 GUI cold-start ───────────────────────────────────────────────
    let gui_child = if opts.skip_gui {
        checks.push(CheckResult::skip(
            "l2.gui_launch",
            "L2.2 gui-launch",
            "skipped (--skip-gui)",
        ));
        None
    } else {
        let (result, child) = launch_gui(opts, &neutral);
        checks.push(result);
        child
    };

    // ── L3 service health ─────────────────────────────────────────────────
    checks.push(check_daemon_pipe(daemon_mode));
    checks.push(check_services_discovered(daemon_mode, opts.nospawn));
    checks.push(check_vram_broker());
    checks.push(check_ollama_models());
    checks.push(check_memgraph_has_data());

    // ── L5 shipped-config assertion (issue #27) ───────────────────────────
    // Not behind `--skip-functional`: it's a single cheap pipe read, and a
    // release-grade receipt should never be able to skip "did we ship the
    // experimental tier switched on?".
    checks.push(check_reasoning_disabled());

    if opts.skip_functional {
        for (key, title) in [
            ("l3.rag_answers", "L3.6 rag-answers"),
            ("l3.chat_turn", "L3.7 chat-turn"),
            ("l3.memory_round_trip", "L3.8 memory-round-trip"),
        ] {
            checks.push(CheckResult::skip(key, title, "skipped (--skip-functional)"));
        }
    } else {
        checks.push(check_rag_answers(&opts.repo_root));
        checks.push(check_chat_turn(
            &opts.repo_root,
            &neutral,
            opts.chat_turn_ok,
        ));
        checks.push(check_memory_round_trip(&opts.repo_root));
    }

    // ── Cleanup — never leave orphans ─────────────────────────────────────
    teardown(daemon_mode, daemon_child, gui_child);

    SmokeOutcome {
        checks,
        daemon_mode,
    }
}

// ── L2: launch ────────────────────────────────────────────────────────────

/// A fresh, empty directory to launch from, so a passing cold-start proves the
/// daemon resolves everything from `WYLDE_ROOT`, not from a lucky cwd.
fn neutral_cwd() -> PathBuf {
    let dir = std::env::temp_dir().join("wylde-preflight-neutral");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Resolve a shipped binary, mirroring the daemon's own resolution order
/// (`rust/bin` → `target/release` → `target/debug`).
fn resolve_exe(repo_root: &Path, rel_candidates: &[&str]) -> Option<PathBuf> {
    rel_candidates
        .iter()
        .map(|rel| repo_root.join(rel))
        .find(|p| p.is_file())
}

fn daemon_exe(repo_root: &Path) -> Option<PathBuf> {
    resolve_exe(
        repo_root,
        &[
            "rust/bin/wylde-lifecycle.exe",
            "rust/target/release/wylde-lifecycle.exe",
            "rust/target/debug/wylde-lifecycle.exe",
        ],
    )
}

fn gui_exe(repo_root: &Path) -> Option<PathBuf> {
    resolve_exe(
        repo_root,
        &[
            "Core/GUI/target/release/wylde-gui.exe",
            "Core/GUI/target/debug/wylde-gui.exe",
        ],
    )
}

/// L2.1: attach to a running daemon, or cold-start one from a neutral cwd and
/// wait for it to bind `\\.\pipe\wylde-lifecycle`.
fn launch_or_attach_daemon(
    opts: &SmokeOpts,
    neutral: &Path,
) -> (CheckResult, DaemonMode, Option<Child>) {
    const KEY: &str = "l2.daemon_launch";
    const TITLE: &str = "L2.1 daemon-launch";

    // Already up? Attach — do not spawn or tear down a sibling session's stack.
    if pipe::pipe_exists("wylde-lifecycle") {
        return (
            CheckResult::pass(
                KEY,
                TITLE,
                "attached to an already-running daemon (cold-start not exercised this run)",
            ),
            DaemonMode::Attached,
            None,
        );
    }

    if opts.attach_only {
        return (
            CheckResult::fail(
                KEY,
                TITLE,
                "--attach-only was set but no daemon is bound on \\\\.\\pipe\\wylde-lifecycle",
            ),
            DaemonMode::Absent,
            None,
        );
    }

    let Some(exe) = daemon_exe(&opts.repo_root) else {
        return (
            CheckResult::fail(
                KEY,
                TITLE,
                "wylde-lifecycle.exe not found (build it: `cargo build --release -p wylde-lifecycle`) \
                 — this is the 'artifact was never built' class the gate exists to catch",
            ),
            DaemonMode::Absent,
            None,
        );
    };

    // Spawn from the NEUTRAL cwd with an absolute WYLDE_ROOT, exactly as the
    // launcher does — so a pass proves env-var resolution, not cwd luck.
    let root_abs =
        std::fs::canonicalize(&opts.repo_root).unwrap_or_else(|_| opts.repo_root.clone());
    let stderr_log = neutral.join("daemon-stderr.log");
    let mut cmd = Command::new(&exe);
    cmd.current_dir(neutral)
        .env("WYLDE_ROOT", &root_abs)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if opts.nospawn {
        // Parity mode — binds the pipe, records "would-have-spawned" instead of
        // forking the tree. Plumbing validation only.
        cmd.env("WYLDE_LIFECYCLE_NOSPAWN", "1");
    }
    match std::fs::File::create(&stderr_log) {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                CheckResult::fail(
                    KEY,
                    TITLE,
                    format!("could not spawn {}: {e}", exe.display()),
                ),
                DaemonMode::Absent,
                None,
            );
        }
    };

    // Poll for the pipe to appear, or for the process to die early.
    let deadline = Instant::now() + DAEMON_LAUNCH_TIMEOUT;
    loop {
        if pipe::pipe_exists("wylde-lifecycle") {
            let mode = if opts.nospawn {
                "no-spawn parity mode"
            } else {
                "cold start"
            };
            return (
                CheckResult::pass(
                    KEY,
                    TITLE,
                    format!(
                        "daemon launched from a neutral cwd ({}) and bound the lifecycle pipe",
                        mode
                    ),
                ),
                DaemonMode::ColdStarted,
                Some(child),
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let tail = read_tail(&stderr_log, 500);
                return (
                    CheckResult::fail(
                        KEY,
                        TITLE,
                        format!(
                            "daemon exited early ({status}) before binding the pipe.{}",
                            if tail.is_empty() {
                                String::new()
                            } else {
                                format!(" stderr: {tail}")
                            }
                        ),
                    ),
                    DaemonMode::Absent,
                    None,
                );
            }
            Ok(None) => {}
            Err(e) => {
                let _ = kill_tree(&child);
                return (
                    CheckResult::fail(KEY, TITLE, format!("error waiting on daemon: {e}")),
                    DaemonMode::Absent,
                    None,
                );
            }
        }
        if Instant::now() >= deadline {
            let _ = kill_tree(&child);
            let _ = child.kill();
            return (
                CheckResult::fail(
                    KEY,
                    TITLE,
                    format!(
                        "daemon did not bind \\\\.\\pipe\\wylde-lifecycle within {}s",
                        DAEMON_LAUNCH_TIMEOUT.as_secs()
                    ),
                ),
                DaemonMode::Absent,
                Some(child),
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// L2.2: launch the GUI process and assert it stays alive without panicking.
/// The honest automatable bar — window *content* is the CI panel-walk's job.
fn launch_gui(opts: &SmokeOpts, neutral: &Path) -> (CheckResult, Option<Child>) {
    const KEY: &str = "l2.gui_launch";
    const TITLE: &str = "L2.2 gui-launch";

    let Some(exe) = gui_exe(&opts.repo_root) else {
        return (
            CheckResult::fail(
                KEY,
                TITLE,
                "wylde-gui.exe not found (build it: `cargo build --release -p wylde-gui` from Core/GUI/) \
                 — the exact 'GUI wasn't even built' failure the gate exists to catch",
            ),
            None,
        );
    };

    let root_abs =
        std::fs::canonicalize(&opts.repo_root).unwrap_or_else(|_| opts.repo_root.clone());
    let stderr_log = neutral.join("gui-stderr.log");
    let mut cmd = Command::new(&exe);
    cmd.current_dir(neutral)
        .env("WYLDE_ROOT", &root_abs)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    match std::fs::File::create(&stderr_log) {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                CheckResult::fail(
                    KEY,
                    TITLE,
                    format!("could not spawn {}: {e}", exe.display()),
                ),
                None,
            );
        }
    };

    // Give it a window to boot, then assert it didn't fall over. `panic = abort`
    // in the GUI release profile means a panic is a non-zero early exit.
    let deadline = Instant::now() + GUI_ALIVE_WINDOW;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let tail = read_tail(&stderr_log, 500);
                return (
                    CheckResult::fail(
                        KEY,
                        TITLE,
                        format!(
                            "GUI exited within {}s ({status}) — likely a panic on startup.{}",
                            GUI_ALIVE_WINDOW.as_secs(),
                            if tail.is_empty() {
                                String::new()
                            } else {
                                format!(" stderr: {tail}")
                            }
                        ),
                    ),
                    None,
                );
            }
            Ok(None) => {}
            Err(e) => {
                return (
                    CheckResult::fail(KEY, TITLE, format!("error waiting on GUI: {e}")),
                    Some(child),
                );
            }
        }
        if Instant::now() >= deadline {
            return (
                CheckResult::pass(
                    KEY,
                    TITLE,
                    format!(
                        "GUI process stayed alive {}s without panicking (window content is the CI panel-walk's job)",
                        GUI_ALIVE_WINDOW.as_secs()
                    ),
                ),
                Some(child),
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

// ── L3: service health ─────────────────────────────────────────────────────

/// L3.1: the lifecycle pipe answers a native `/__ping__`.
fn check_daemon_pipe(mode: DaemonMode) -> CheckResult {
    const KEY: &str = "l3.daemon_pipe";
    const TITLE: &str = "L3.1 daemon-pipe";
    if mode == DaemonMode::Absent {
        return CheckResult::fail(KEY, TITLE, "no daemon is running (see L2.1)");
    }
    let result = pipe::ping("wylde-lifecycle", PIPE_CALL_TIMEOUT).map(|_| ());
    CheckResult::from_result(KEY, TITLE, "lifecycle pipe answered /__ping__", result)
}

/// L3.2: the daemon discovered its services (`service.list`) and the critical
/// ones are actually up.
fn check_services_discovered(mode: DaemonMode, nospawn: bool) -> CheckResult {
    const KEY: &str = "l3.services_discovered";
    const TITLE: &str = "L3.2 services-discovered";
    if mode == DaemonMode::Absent {
        return CheckResult::fail(KEY, TITLE, "no daemon is running (see L2.1)");
    }
    if nospawn {
        return CheckResult::fail(
            KEY,
            TITLE,
            "daemon is in --nospawn parity mode; no services were actually started \
             (this mode validates plumbing only, never real health)",
        );
    }
    // Poll until the critical services are up, or fail closed at the deadline —
    // the daemon spawns them asynchronously after binding the pipe.
    let deadline = Instant::now() + SERVICES_CONVERGE_TIMEOUT;
    loop {
        let last = match evaluate_services_once() {
            Ok(detail) => return CheckResult::pass(KEY, TITLE, detail),
            Err(e) => format!("{e:#}"),
        };
        if Instant::now() >= deadline {
            return CheckResult::fail(
                KEY,
                TITLE,
                format!(
                    "service tree did not converge within {}s — {last}",
                    SERVICES_CONVERGE_TIMEOUT.as_secs()
                ),
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// One convergence probe. `Ok` only when the daemon reports a discovered roster
/// (`service.list`) **and** each core pipe service answers a direct ping.
///
/// We assert liveness by pinging each service's own pipe rather than reading the
/// `service.list` row's status, because that roster can lag or omit a service
/// that is in fact running (observed live: the VRAM broker answered its pipe
/// while absent from the list). The direct ping is ground truth.
fn evaluate_services_once() -> Result<String> {
    // Discovery: the daemon walked its core set + the WYLDE_SERVICES bucket and
    // can report a roster.
    let data = pipe::action(
        "wylde-lifecycle",
        "service.list",
        serde_json::json!({}),
        PIPE_CALL_TIMEOUT,
    )
    .context("service.list")?;
    let services = data
        .get("services")
        .and_then(Value::as_array)
        .context("service.list reply had no `services` array")?;
    if services.is_empty() {
        bail!("the daemon discovered zero services");
    }

    // Liveness: each core pipe service must answer its own /__ping__.
    let mut down = Vec::new();
    for svc in CORE_PIPE_SERVICES {
        if pipe::ping(svc, PIPE_CALL_TIMEOUT).is_err() {
            down.push(svc.strip_prefix("wylde-").unwrap_or(svc));
        }
    }
    if !down.is_empty() {
        bail!(
            "core services not reachable on their pipes: [{}] ({} services in the roster)",
            down.join(", "),
            services.len()
        );
    }
    Ok(format!(
        "daemon discovered {} services; core services (harness, workspaces) reachable",
        services.len()
    ))
}

/// L3.3: the VRAM broker answers and sees a GPU.
fn check_vram_broker() -> CheckResult {
    const KEY: &str = "l3.vram_broker";
    const TITLE: &str = "L3.3 vram-broker";
    let result = (|| -> Result<String> {
        let data = pipe::action(
            "wylde-vram-broker",
            "vram.state",
            serde_json::json!({}),
            PIPE_CALL_TIMEOUT,
        )
        .context("vram.state")?;
        let gpu = data.get("gpu").context("vram.state reply had no `gpu`")?;
        let total = gpu.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
        if total == 0 {
            bail!("broker reports 0 total VRAM (no GPU seen)");
        }
        let name = gpu.get("name").and_then(Value::as_str).unwrap_or("GPU");
        Ok(format!(
            "broker up; sees {name} with {} GiB VRAM",
            total / (1024 * 1024 * 1024)
        ))
    })();
    match result {
        Ok(detail) => CheckResult::pass(KEY, TITLE, detail),
        Err(e) => CheckResult::fail(KEY, TITLE, format!("{e:#}")),
    }
}

/// L3.4: Ollama is reachable and has the reasoner + embed models.
fn check_ollama_models() -> CheckResult {
    const KEY: &str = "l3.ollama_model";
    const TITLE: &str = "L3.4 ollama-model";
    let want_reasoner =
        std::env::var("WYLDE_EVAL_MODEL").unwrap_or_else(|_| DEFAULT_REASONER_MODEL.to_string());
    let want_embed =
        std::env::var("WYLDE_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_EMBED_MODEL.to_string());
    let result = (|| -> Result<String> {
        let tags = ollama_get("/api/tags").context("GET /api/tags")?;
        let models: Vec<String> = tags
            .get("models")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if models.is_empty() {
            bail!("Ollama reachable but has no models pulled");
        }
        let has = |want: &str| models.iter().any(|m| model_matches(m, want));
        let mut missing = Vec::new();
        if !has(&want_reasoner) {
            missing.push(format!("reasoner `{want_reasoner}`"));
        }
        if !has(&want_embed) {
            missing.push(format!("embed `{want_embed}`"));
        }
        if !missing.is_empty() {
            bail!("Ollama reachable but missing {}", missing.join(" + "));
        }
        Ok(format!(
            "Ollama up with {} models incl. the reasoner + embed",
            models.len()
        ))
    })();
    match result {
        Ok(detail) => CheckResult::pass(KEY, TITLE, detail),
        Err(e) => CheckResult::fail(KEY, TITLE, format!("{e:#}")),
    }
}

/// A pulled tag matches a wanted model if it equals it or shares its base
/// (ignoring a `:tag` suffix like `:latest`).
fn model_matches(pulled: &str, want: &str) -> bool {
    if pulled == want {
        return true;
    }
    fn base(s: &str) -> &str {
        s.split(':').next().unwrap_or(s)
    }
    base(pulled) == base(want)
}

/// GET a JSON endpoint from Ollama via `curl` (no HTTP-client dep — the same
/// idiom `host::ollama_version` already uses). Fails closed on any error.
fn ollama_get(path: &str) -> Result<Value> {
    let host = std::env::var("OLLAMA_URL")
        .or_else(|_| std::env::var("OLLAMA_HOST"))
        .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let url = format!("{}{}", host.trim_end_matches('/'), path);
    let out = Command::new("curl")
        .args(["-s", "--max-time", &OLLAMA_TIMEOUT_SECS.to_string(), &url])
        .output()
        .context("spawning curl (is it on PATH?)")?;
    if !out.status.success() {
        bail!("curl to {url} failed ({})", out.status);
    }
    serde_json::from_slice(&out.stdout).with_context(|| format!("parsing JSON from {url}"))
}

/// L3.5: Memgraph holds REAL data — the empty-graph catch.
fn check_memgraph_has_data() -> CheckResult {
    const KEY: &str = "l3.memgraph_has_data";
    const TITLE: &str = "L3.5 memgraph-has-data";
    match memgraph::graph_counts(MEMGRAPH_TIMEOUT) {
        Ok(counts) if counts.is_populated() => CheckResult::pass(
            KEY,
            TITLE,
            format!(
                "graph populated: {} chunks, {} entities",
                counts.chunks, counts.entities
            ),
        ),
        Ok(counts) => CheckResult::fail(
            KEY,
            TITLE,
            format!(
                "Bolt is up but the graph is EMPTY ({} chunks, {} entities) — \
                 the ${{VAR}} empty-boot / stale-data bug the port ping can't see",
                counts.chunks, counts.entities
            ),
        ),
        Err(e) => CheckResult::fail(KEY, TITLE, format!("{e:#}")),
    }
}

/// L5: the SHIPPED config keeps the reasoning tier off (issue #27).
///
/// The tier is a post-0.2 experiment and must ship `enabled:false`. The *code*
/// default already says so (`ReasoningConfig::default`, unit-tested), but a unit
/// test only proves the fallback — it cannot see a `reasoning.json` that ships
/// (or gets written) with the tier on. That file is what the running system
/// actually obeys, so it is what this asserts.
///
/// Asks the **running harness** for its effective config rather than reading a
/// file, which is deliberate: `ReasoningConfig::current()` is the value the turn
/// engine uses, already resolved through the same
/// `WYLDE_DATA_DIR`/`DATA_DIR`/`WYLDE_ROOT` chain the product resolves. So one
/// live read subsumes both halves — a shipped file that enables the tier fails
/// here, and so does an in-memory value that disagrees with the file. Reading
/// `<data_dir>/settings/reasoning.json` ourselves would re-implement that
/// resolution and could pass while the running system disagreed.
///
/// **Honest limit:** this asserts the config of the install the preflight runs
/// against. On a clean install there is no `reasoning.json`, so the tier falls
/// back to the unit-tested default (off) — but a *clean-install* run is what
/// #37 tracks; on a warm rig this proves that rig ships it off.
///
/// Fails closed: a missing/non-boolean `enabled`, or a harness that won't
/// answer, is a FAIL — "couldn't determine" never counts as "it's off".
fn check_reasoning_disabled() -> CheckResult {
    const KEY: &str = "l5.reasoning_disabled";
    const TITLE: &str = "L5 shipped-config reasoning-off";
    let result = (|| -> Result<String> {
        let cfg = pipe::action(
            "wylde-harness",
            "settings.reasoning.get",
            serde_json::json!({}),
            PIPE_CALL_TIMEOUT,
        )
        .context("settings.reasoning.get")?;
        reasoning_verdict(&cfg)
    })();
    match result {
        Ok(detail) => CheckResult::pass(KEY, TITLE, detail),
        Err(e) => CheckResult::fail(KEY, TITLE, format!("{e:#}")),
    }
}

/// The pure verdict for [`check_reasoning_disabled`], split from the pipe call so
/// the fail-closed contract is unit-testable without a running stack.
///
/// `Ok` ⇒ the tier is provably off. Every other shape — enabled, missing key,
/// wrong type, non-object — is `Err` ⇒ FAIL. There is deliberately no
/// "assume off" branch: this gate exists because a *silent* on is the defect.
fn reasoning_verdict(cfg: &Value) -> Result<String> {
    let enabled = cfg
        .get("enabled")
        .and_then(Value::as_bool)
        .context("settings.reasoning.get reply had no boolean `enabled` — cannot determine")?;
    if enabled {
        bail!(
            "shipped config has reasoning enabled:TRUE — the tier is a post-0.2 experiment and \
             must ship OFF (issue #27). Set it false in `<data_dir>/settings/reasoning.json` \
             (or via settings.reasoning.set) before shipping"
        );
    }
    let depth = cfg
        .get("default_depth")
        .and_then(Value::as_str)
        .unwrap_or("<unset>");
    Ok(format!(
        "running harness reports reasoning enabled:false (default_depth {depth})"
    ))
}

#[cfg(test)]
mod reasoning_gate_tests {
    use super::reasoning_verdict;
    use serde_json::json;

    #[test]
    fn disabled_config_passes_and_reports_depth() {
        let d = reasoning_verdict(&json!({"enabled": false, "default_depth": "Fast"})).unwrap();
        assert!(d.contains("enabled:false"), "{d}");
        assert!(d.contains("Fast"), "{d}");
    }

    #[test]
    fn enabled_config_fails() {
        let e = reasoning_verdict(&json!({"enabled": true, "default_depth": "Think"}))
            .expect_err("enabled:true must FAIL the gate");
        assert!(format!("{e:#}").contains("must ship OFF"));
    }

    /// Fail-closed: the whole point. A reply we can't read is NOT a pass.
    #[test]
    fn undeterminable_replies_fail_closed() {
        for bad in [
            json!({}),                   // key absent
            json!({"enabled": "false"}), // string, not bool — a real serde slip
            json!({"enabled": 0}),       // falsy but not a bool
            json!({"enabled": null}),    // explicit null
            json!("enabled=false"),      // not an object at all
        ] {
            assert!(
                reasoning_verdict(&bad).is_err(),
                "must fail closed, got a pass for {bad}"
            );
        }
    }

    /// A missing `default_depth` is cosmetic — it must not turn a provably-off
    /// config into a failure.
    #[test]
    fn missing_depth_still_passes_when_disabled() {
        let d = reasoning_verdict(&json!({"enabled": false})).unwrap();
        assert!(d.contains("<unset>"), "{d}");
    }
}

/// L3.6: RAG answers a query — the hermetic index→query fixture test (#26).
fn check_rag_answers(repo_root: &Path) -> CheckResult {
    const KEY: &str = "l3.rag_answers";
    const TITLE: &str = "L3.6 rag-answers";
    let mut cmd = cargo_in_rust(repo_root);
    cmd.args([
        "test",
        "-p",
        "wylde-workspaces",
        "--test",
        "integration_rag_indexer",
    ]);
    let result = run_cargo_check(cmd, CARGO_CHECK_TIMEOUT, "integration_rag_indexer");
    CheckResult::from_result(
        KEY,
        TITLE,
        "RAG indexed a fixture corpus, answered a query, and ranked the right file first",
        result,
    )
}

/// L3.7: a full chat turn completes end-to-end. Reuses the preflight benchmark's
/// reasoning verdict when available; else runs a `reasoning_eval --smoke` turn.
fn check_chat_turn(repo_root: &Path, neutral: &Path, upstream: Option<bool>) -> CheckResult {
    const KEY: &str = "l3.chat_turn";
    const TITLE: &str = "L3.7 chat-turn";
    if let Some(ok) = upstream {
        return if ok {
            CheckResult::pass(
                KEY,
                TITLE,
                "a chat turn completed end-to-end in the benchmark reasoning run",
            )
        } else {
            CheckResult::fail(
                KEY,
                TITLE,
                "the benchmark reasoning run produced no successful turn",
            )
        };
    }
    // Standalone: run a real turn via the reasoning_eval smoke path.
    let out_dir = neutral.join("reasoning-smoke");
    let _ = std::fs::create_dir_all(&out_dir);
    let mut cmd = cargo_in_rust(repo_root);
    cmd.args([
        "run",
        "--release",
        "--example",
        "reasoning_eval",
        "--",
        "--smoke",
        "--out",
        &out_dir.to_string_lossy(),
    ]);
    let result = (|| -> Result<()> {
        run_cargo_check(cmd, CARGO_CHECK_TIMEOUT, "reasoning_eval --smoke")?;
        let json = out_dir.join("reasoning-eval-results.json");
        let raw = std::fs::read_to_string(&json)
            .with_context(|| format!("reading {}", json.display()))?;
        let parsed: Value =
            serde_json::from_str(&raw).context("parsing reasoning-eval-results.json")?;
        let any_ok = parsed
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .any(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            })
            .unwrap_or(false);
        if any_ok {
            Ok(())
        } else {
            bail!("reasoning_eval ran but no turn succeeded")
        }
    })();
    CheckResult::from_result(
        KEY,
        TITLE,
        "a chat turn completed end-to-end (reasoning_eval smoke)",
        result,
    )
}

/// L3.8: memory round-trips — write a memory, retrieve it via a live embed
/// search (the capability today's live verification proved; issue #43).
fn check_memory_round_trip(repo_root: &Path) -> CheckResult {
    const KEY: &str = "l3.memory_round_trip";
    const TITLE: &str = "L3.8 memory-round-trip";
    let mut cmd = cargo_in_rust(repo_root);
    cmd.args([
        "test",
        "-p",
        "wylde-harness",
        "--test",
        "embed_live",
        "text_search_round_trip_against_live_wylde_ollama",
        "--",
        "--ignored",
    ]);
    let result = run_cargo_check(cmd, CARGO_CHECK_TIMEOUT, "embed_live round-trip");
    CheckResult::from_result(
        KEY,
        TITLE,
        "wrote a memory and retrieved it back through a live embed search",
        result,
    )
}

// ── Process + IO helpers ────────────────────────────────────────────────────

/// A `cargo` command rooted in `rust/` (where the backend workspace lives).
fn cargo_in_rust(repo_root: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(repo_root.join("rust"));
    cmd
}

/// Run a cargo command as a check, bounded by `timeout`. Success = exit 0. On
/// timeout the process tree is killed and the check fails closed.
fn run_cargo_check(mut cmd: Command, timeout: Duration, label: &str) -> Result<()> {
    cmd.stdin(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning cargo for {label}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().context("waiting on cargo")? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => bail!("{label} failed ({status})"),
            None => {}
        }
        if Instant::now() >= deadline {
            let _ = kill_tree(&child);
            let _ = child.kill();
            bail!("{label} timed out after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(300));
    }
}

/// Read the last `max` bytes of a file, trimmed to one line, for a failure
/// diagnosis. Best-effort — an unreadable log yields an empty string.
fn read_tail(path: &Path, max: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let trimmed = text.trim();
    let start = trimmed.len().saturating_sub(max);
    trimmed[start..]
        .replace(['\n', '\r'], " ")
        .trim()
        .to_string()
}

/// Kill a whole process tree via `taskkill /T /F` — the backstop that reaps the
/// service children a bare `Child::kill` would orphan.
fn kill_tree(child: &Child) -> Result<()> {
    let pid = child.id();
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning taskkill")?;
    if status.success() {
        Ok(())
    } else {
        bail!("taskkill /PID {pid} /T /F exited {status}")
    }
}

/// Tear down everything this run spawned. A cold-started daemon is asked to
/// drain its children gracefully (`service.shutdown_all`) before the taskkill
/// backstop; an attached daemon is left untouched (it belongs to another
/// session). The GUI we spawned is always killed.
fn teardown(mode: DaemonMode, daemon: Option<Child>, gui: Option<Child>) {
    if let Some(gui) = gui {
        let _ = kill_tree(&gui);
    }
    if mode != DaemonMode::ColdStarted {
        // Attached / absent: nothing of ours to stop.
        return;
    }
    // Graceful drain first, then the hard backstop.
    let _ = pipe::action(
        "wylde-lifecycle",
        "service.shutdown_all",
        serde_json::json!({}),
        PIPE_CALL_TIMEOUT,
    );
    // Give the daemon a moment to unwind the service tree.
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while Instant::now() < deadline && pipe::pipe_exists("wylde-lifecycle") {
        thread::sleep(Duration::from_millis(200));
    }
    if let Some(daemon) = daemon {
        let _ = kill_tree(&daemon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_matches_ignores_tag_suffix() {
        assert!(model_matches("nomic-embed-text:latest", "nomic-embed-text"));
        assert!(model_matches("nomic-embed-text", "nomic-embed-text"));
        assert!(model_matches(
            "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS",
            "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS"
        ));
        assert!(!model_matches("llama3:8b", "nomic-embed-text"));
    }

    #[test]
    fn outcome_all_passed_requires_every_pass() {
        let pass = CheckResult::pass("k", "t", "");
        let skip = CheckResult::skip("k", "t", "");
        let fail = CheckResult::fail("k", "t", "");
        assert!(SmokeOutcome {
            checks: vec![pass.clone()],
            daemon_mode: DaemonMode::ColdStarted
        }
        .all_passed());
        // A skip is not a pass — fail-closed.
        assert!(!SmokeOutcome {
            checks: vec![pass.clone(), skip],
            daemon_mode: DaemonMode::ColdStarted
        }
        .all_passed());
        assert!(!SmokeOutcome {
            checks: vec![pass, fail],
            daemon_mode: DaemonMode::ColdStarted
        }
        .all_passed());
        // Empty is not "all passed".
        assert!(!SmokeOutcome {
            checks: vec![],
            daemon_mode: DaemonMode::Absent
        }
        .all_passed());
    }

    #[test]
    fn from_result_is_fail_closed_on_err() {
        let ok = CheckResult::from_result("k", "t", "good", Ok(()));
        assert_eq!(ok.status, CheckStatus::Pass);
        let bad = CheckResult::from_result("k", "t", "good", Err(anyhow::anyhow!("boom")));
        assert_eq!(bad.status, CheckStatus::Fail);
        assert!(bad.detail.contains("boom"));
    }
}
