//! Automatic crash-restart supervision for daemon-managed services.
//!
//! The orphan sweep ([`crate::state::orphan_sweep`]) already detects a
//! daemon-managed service whose pid vanished and flips its manifest to
//! `dead-orphan`. Before this module that was the end of the line: a
//! crashed/panicked service stayed dark until an operator hand-restarted
//! it (`dev.restart_service`, or `service.stop` + `service.start`). That is
//! the one true divergence from the proven supervisors the architecture
//! review flagged — every battle-tested process supervisor restarts a
//! crashed child automatically.
//!
//! This module closes that gap by *consuming the existing dead-orphan
//! transition* rather than bolting on a parallel watcher. Each sweep pass
//! hands its freshly-detected orphan list to [`drive_restarts`], which:
//!
//!   1. Keeps **intended stops sacrosanct.** A service the operator stopped
//!      had its spawn record cleared by [`crate::state::forget_spawn`] (and
//!      its manifest reads `stopped`, which the sweep skips outright). Only
//!      services the daemon *still owns a spawn record for* are crash
//!      candidates — a user-stopped service is never a candidate and stays
//!      stopped.
//!   2. Applies **exponential backoff** between restarts so a service that
//!      dies on startup doesn't thrash.
//!   3. Trips a **crash-loop breaker** after `max_restarts` within a sliding
//!      `window` — the service is marked `failed` (a terminal manifest state
//!      the GUI surfaces) and left alone instead of being restarted forever.
//!
//! The policy lives in [`Governor`] (a pure state machine, unit-tested in
//! isolation) and is tuned by [`RestartConfig`] (env-gated, default ON for
//! crash-restart — the proven norm). The async side-effects (scheduling the
//! delayed restart, calling the canonical `start_<service>` path) live in
//! [`drive_restarts`] / [`schedule_restart`].

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Tunable crash-restart policy. Resolved once from the environment at first
/// use (see [`RestartConfig::from_env`]); the [`Default`] values are the
/// shipped defaults.
#[derive(Debug, Clone)]
pub struct RestartConfig {
    /// Master switch. Default ON — a crashed service is restarted. Set
    /// `WYLDE_CRASH_RESTART=0` (or `false`/`no`/`off`) to revert to the old
    /// "stays dark until hand-restarted" behaviour. Intended stops are
    /// unaffected either way.
    pub enabled: bool,
    /// How many automatic restarts are allowed inside [`window`](Self::window)
    /// before the crash-loop breaker trips and the service is marked `failed`.
    pub max_restarts: u32,
    /// Sliding window over which [`max_restarts`](Self::max_restarts) is
    /// counted. A crash arriving after the window elapses starts a fresh
    /// count (and a fresh backoff ramp), so a service that crashes once a day
    /// is never treated as a crash loop.
    pub window: Duration,
    /// Backoff before the *first* restart in a fresh window. Each subsequent
    /// restart doubles it (capped at [`max_backoff`](Self::max_backoff)).
    pub base_backoff: Duration,
    /// Ceiling on the exponential backoff.
    pub max_backoff: Duration,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_restarts: 5,
            window: Duration::from_secs(600),
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

impl RestartConfig {
    /// Resolve the policy from the environment, falling back to [`Default`]
    /// for any unset/garbled var.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: env_bool("WYLDE_CRASH_RESTART", d.enabled),
            max_restarts: env_u32("WYLDE_CRASH_RESTART_MAX", d.max_restarts),
            window: env_secs("WYLDE_CRASH_RESTART_WINDOW_SECS", d.window),
            base_backoff: env_millis("WYLDE_CRASH_RESTART_BASE_BACKOFF_MS", d.base_backoff),
            max_backoff: env_secs("WYLDE_CRASH_RESTART_MAX_BACKOFF_SECS", d.max_backoff),
        }
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => !matches!(
            v.trim().to_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        ),
        Err(_) => default,
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

fn env_secs(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(default)
}

fn env_millis(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

/// What the [`Governor`] decided to do with one observed crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart the service after `delay`. `attempt` is the 1-based restart
    /// number within the current window (drives the backoff ramp + logs).
    Restart { delay: Duration, attempt: u32 },
    /// The crash-loop breaker just tripped: `attempts` restarts inside the
    /// window weren't enough, give up and mark the service `failed`.
    TripBreaker { attempts: u32 },
    /// A restart is already scheduled/in-flight for this service — ignore the
    /// duplicate crash signal (the sweep can re-observe a still-dead manifest
    /// before the pending restart fires).
    Pending,
    /// The breaker already tripped on a previous pass — the service is
    /// `failed` and is no longer being restarted.
    GaveUp,
}

/// Per-service crash bookkeeping.
#[derive(Debug, Default)]
struct Entry {
    /// Start of the current counting window, or `None` before the first crash.
    window_start: Option<Instant>,
    /// Restarts attempted in the current window (also the backoff exponent).
    count: u32,
    /// A restart is scheduled or running; suppresses duplicate triggers.
    pending: bool,
    /// Breaker tripped — terminal until [`Governor::forget`].
    failed: bool,
}

/// The crash-restart state machine. Pure and side-effect-free: it records
/// counts and returns decisions but never spawns anything, so it is fully
/// unit-testable without a tokio runtime or a live daemon.
#[derive(Debug, Default)]
pub struct Governor {
    entries: HashMap<String, Entry>,
}

impl Governor {
    /// Record a freshly-observed crash for `name` and decide what to do.
    /// `now` is injected so tests can drive deterministic windows.
    pub fn on_crash(&mut self, name: &str, now: Instant, cfg: &RestartConfig) -> RestartDecision {
        let e = self.entries.entry(name.to_owned()).or_default();
        if e.failed {
            return RestartDecision::GaveUp;
        }
        if e.pending {
            return RestartDecision::Pending;
        }
        // Roll the window over if this crash lands after it elapsed: a fresh
        // count and a fresh backoff ramp.
        let rolled = match e.window_start {
            Some(start) => now.duration_since(start) > cfg.window,
            None => true,
        };
        if rolled {
            e.window_start = Some(now);
            e.count = 0;
        }
        if e.count >= cfg.max_restarts {
            e.failed = true;
            return RestartDecision::TripBreaker { attempts: e.count };
        }
        e.count += 1;
        e.pending = true;
        RestartDecision::Restart {
            delay: backoff_for(e.count, cfg),
            attempt: e.count,
        }
    }

    /// Clear the in-flight flag once a scheduled restart has fired (succeeded
    /// or failed). The next observed crash is then eligible for another pass.
    pub fn on_restart_done(&mut self, name: &str) {
        if let Some(e) = self.entries.get_mut(name) {
            e.pending = false;
        }
    }

    /// Drop all bookkeeping for `name`. Called on an intended stop so a later
    /// legitimate start isn't haunted by stale crash counts or a tripped
    /// breaker.
    pub fn forget(&mut self, name: &str) {
        self.entries.remove(name);
    }

    /// Whether the breaker has tripped for `name` (test/diagnostic surface).
    pub fn is_failed(&self, name: &str) -> bool {
        self.entries.get(name).map(|e| e.failed).unwrap_or(false)
    }
}

/// Exponential backoff for the `attempt`-th restart (1-based): `base * 2^(attempt-1)`,
/// capped at `max_backoff`. Saturating throughout so a long-lived loop can't
/// overflow the shift or the multiply.
fn backoff_for(attempt: u32, cfg: &RestartConfig) -> Duration {
    let shift = attempt.saturating_sub(1).min(32);
    let mult = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let base = cfg.base_backoff.as_millis().min(u128::from(u64::MAX)) as u64;
    let cap = cfg.max_backoff.as_millis().min(u128::from(u64::MAX)) as u64;
    let ms = base.saturating_mul(mult).min(cap);
    Duration::from_millis(ms)
}

// ── Process-global singletons ──────────────────────────────────────────

fn governor() -> &'static Mutex<Governor> {
    static G: OnceLock<Mutex<Governor>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Governor::default()))
}

fn config() -> &'static RestartConfig {
    static C: OnceLock<RestartConfig> = OnceLock::new();
    C.get_or_init(RestartConfig::from_env)
}

/// Drop crash bookkeeping for `name`. Wired into the service stop path so an
/// intended stop wipes any prior crash history (and cancels a pending
/// restart by making the post-backoff ownership check fail).
pub fn forget(name: &str) {
    if let Ok(mut g) = governor().lock() {
        g.forget(name);
    }
}

// ── Driver ─────────────────────────────────────────────────────────────

/// Pure planning step: given the sweep's fresh orphan list, the set of
/// services the daemon still owns a spawn record for, and the policy, decide
/// what to do with each — without performing any restart. Extracted from the
/// async driver so the intended-stop guard, the cap, and the backoff are all
/// unit-testable with a hand-built [`Governor`].
fn plan_restarts(
    orphans: &[String],
    owned: &HashSet<String>,
    cfg: &RestartConfig,
    gov: &mut Governor,
    now: Instant,
) -> Vec<(String, RestartDecision)> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }
    for name in orphans {
        // Intended-stop guard: a service the daemon no longer owns a spawn
        // record for was either never daemon-spawned or was deliberately
        // stopped (forget_spawn cleared it). Sacrosanct — never restart it.
        if !owned.contains(name) {
            continue;
        }
        let decision = gov.on_crash(name, now, cfg);
        out.push((name.clone(), decision));
    }
    out
}

/// Act on the orphans a sweep pass just detected. Called from the orphan-sweep
/// task after each [`crate::state::sweep_orphans`] pass.
pub async fn drive_restarts(orphans: &[String]) {
    let cfg = config();
    if !cfg.enabled || orphans.is_empty() {
        return;
    }
    let owned: HashSet<String> = crate::state::spawn_records_snapshot()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let now = Instant::now();
    let actions = match governor().lock() {
        Ok(mut g) => plan_restarts(orphans, &owned, cfg, &mut g, now),
        Err(_) => return,
    };
    for (name, decision) in actions {
        match decision {
            RestartDecision::Restart { delay, attempt } => {
                tracing::warn!(
                    "crash-restart: {} exited unexpectedly — scheduling restart {}/{} in {:.1}s",
                    name,
                    attempt,
                    cfg.max_restarts,
                    delay.as_secs_f64()
                );
                schedule_restart(name, delay);
            }
            RestartDecision::TripBreaker { attempts } => {
                tracing::error!(
                    "crash-restart: {} crashed {} times within {:.0}s — crash-loop breaker tripped; \
                     giving up and marking it failed (clear with service.start once the cause is fixed)",
                    name,
                    attempts,
                    cfg.window.as_secs_f64()
                );
                if let Err(e) = wylde_shared::manifest::mark_failed(&name) {
                    tracing::warn!("crash-restart: mark_failed({}) failed: {:#}", name, e);
                }
            }
            RestartDecision::Pending | RestartDecision::GaveUp => {}
        }
    }
}

/// Spawn the delayed restart task: sleep out the backoff, re-check the
/// service is still daemon-owned (an intended stop during the backoff
/// aborts), then route through the canonical `start_<service>` path.
fn schedule_restart(name: String, delay: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        // Re-check ownership: if the operator stopped the service during the
        // backoff, forget_spawn cleared its record — abort so an intended
        // stop stays sacrosanct even when it races a pending restart.
        if !crate::state::spawn_record_exists(&name) {
            tracing::info!(
                "crash-restart: {} was stopped during backoff — restart aborted",
                name
            );
            if let Ok(mut g) = governor().lock() {
                g.on_restart_done(&name);
            }
            return;
        }
        tracing::info!("crash-restart: restarting {}", name);
        match crate::control::restart_service(&name).await {
            Ok(()) => tracing::info!("crash-restart: {} restarted", name),
            Err(e) => tracing::error!("crash-restart: {} restart failed: {:#}", name, e),
        }
        if let Ok(mut g) = governor().lock() {
            g.on_restart_done(&name);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max: u32, base_ms: u64, max_ms: u64, window_s: u64) -> RestartConfig {
        RestartConfig {
            enabled: true,
            max_restarts: max,
            window: Duration::from_secs(window_s),
            base_backoff: Duration::from_millis(base_ms),
            max_backoff: Duration::from_millis(max_ms),
        }
    }

    fn owned(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn crashed_owned_service_is_restarted() {
        let c = cfg(5, 100, 1000, 600);
        let mut g = Governor::default();
        let orphans = vec!["wylde-crashed".to_owned()];
        let actions = plan_restarts(&orphans, &owned(&["wylde-crashed"]), &c, &mut g, Instant::now());
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].0, "wylde-crashed");
        assert_eq!(
            actions[0].1,
            RestartDecision::Restart {
                delay: Duration::from_millis(100),
                attempt: 1,
            }
        );
    }

    #[test]
    fn intentionally_stopped_service_is_not_restarted() {
        // The crashed manifest is in the orphan list, but the daemon no
        // longer owns its spawn record (forget_spawn cleared it on the
        // intended stop) — so it must NOT be planned for restart.
        let c = cfg(5, 100, 1000, 600);
        let mut g = Governor::default();
        let orphans = vec!["wylde-stopped".to_owned()];
        let actions = plan_restarts(&orphans, &owned(&[]), &c, &mut g, Instant::now());
        assert!(
            actions.is_empty(),
            "an intentionally-stopped service must never be restarted, got {actions:?}"
        );
        // And the governor recorded nothing for it.
        assert!(!g.is_failed("wylde-stopped"));
    }

    #[test]
    fn disabled_config_plans_nothing() {
        let mut c = cfg(5, 100, 1000, 600);
        c.enabled = false;
        let mut g = Governor::default();
        let orphans = vec!["wylde-crashed".to_owned()];
        let actions = plan_restarts(&orphans, &owned(&["wylde-crashed"]), &c, &mut g, Instant::now());
        assert!(actions.is_empty(), "disabled policy must plan nothing");
    }

    #[test]
    fn breaker_trips_after_cap_then_gives_up() {
        let c = cfg(3, 10, 100, 600);
        let mut g = Governor::default();
        let now = Instant::now();
        // Three restarts allowed (the cap), each completing before the next
        // crash — exactly the steady-state crash-loop the sweep observes.
        for i in 1..=3 {
            match g.on_crash("svc", now, &c) {
                RestartDecision::Restart { attempt, .. } => assert_eq!(attempt, i),
                other => panic!("crash {i}: expected Restart, got {other:?}"),
            }
            g.on_restart_done("svc");
        }
        // Fourth crash exceeds the cap → breaker trips and marks failed.
        assert_eq!(
            g.on_crash("svc", now, &c),
            RestartDecision::TripBreaker { attempts: 3 }
        );
        assert!(g.is_failed("svc"));
        // Further crashes are inert — no thrash, the daemon left it alone.
        assert_eq!(g.on_crash("svc", now, &c), RestartDecision::GaveUp);
        assert_eq!(g.on_crash("svc", now, &c), RestartDecision::GaveUp);
    }

    #[test]
    fn pending_restart_suppresses_duplicate_crash() {
        // A second sweep can observe the still-dead manifest before the
        // pending restart fires — that duplicate must not double-restart.
        let c = cfg(5, 10, 100, 600);
        let mut g = Governor::default();
        let now = Instant::now();
        assert!(matches!(
            g.on_crash("svc", now, &c),
            RestartDecision::Restart { attempt: 1, .. }
        ));
        assert_eq!(g.on_crash("svc", now, &c), RestartDecision::Pending);
        // Once the restart completes, the next crash is eligible again.
        g.on_restart_done("svc");
        assert!(matches!(
            g.on_crash("svc", now, &c),
            RestartDecision::Restart { attempt: 2, .. }
        ));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let c = cfg(100, 100, 800, 600);
        assert_eq!(backoff_for(1, &c), Duration::from_millis(100));
        assert_eq!(backoff_for(2, &c), Duration::from_millis(200));
        assert_eq!(backoff_for(3, &c), Duration::from_millis(400));
        assert_eq!(backoff_for(4, &c), Duration::from_millis(800));
        // Capped at max_backoff thereafter.
        assert_eq!(backoff_for(5, &c), Duration::from_millis(800));
        assert_eq!(backoff_for(6, &c), Duration::from_millis(800));
        // No overflow at large attempt numbers.
        assert_eq!(backoff_for(1_000_000, &c), Duration::from_millis(800));
    }

    #[test]
    fn window_rollover_resets_count_and_backoff() {
        let c = cfg(5, 10, 10_000, 1); // 1s window, base 10ms, generous cap
        let mut g = Governor::default();
        let t0 = Instant::now();
        // Two crashes inside the window: backoff ramps 10ms → 20ms.
        assert!(matches!(
            g.on_crash("svc", t0, &c),
            RestartDecision::Restart { attempt: 1, delay } if delay == Duration::from_millis(10)
        ));
        g.on_restart_done("svc");
        assert!(matches!(
            g.on_crash("svc", t0, &c),
            RestartDecision::Restart { attempt: 2, delay } if delay == Duration::from_millis(20)
        ));
        g.on_restart_done("svc");
        // A crash well past the window starts fresh: attempt 1, base backoff.
        let later = t0 + Duration::from_secs(5);
        assert!(matches!(
            g.on_crash("svc", later, &c),
            RestartDecision::Restart { attempt: 1, delay } if delay == Duration::from_millis(10)
        ));
    }

    #[test]
    fn forget_clears_failed_and_counts() {
        let c = cfg(1, 10, 100, 600);
        let mut g = Governor::default();
        let now = Instant::now();
        // One restart, then the cap (1) is exceeded → failed.
        assert!(matches!(
            g.on_crash("svc", now, &c),
            RestartDecision::Restart { attempt: 1, .. }
        ));
        g.on_restart_done("svc");
        assert_eq!(
            g.on_crash("svc", now, &c),
            RestartDecision::TripBreaker { attempts: 1 }
        );
        assert!(g.is_failed("svc"));
        // An intended stop forgets it; a subsequent crash starts clean.
        g.forget("svc");
        assert!(!g.is_failed("svc"));
        assert!(matches!(
            g.on_crash("svc", now, &c),
            RestartDecision::Restart { attempt: 1, .. }
        ));
    }

    #[test]
    fn env_bool_parses_truthy_and_falsy() {
        // Defaults through when unset; explicit falsy values disable.
        assert!(env_bool("WYLDE_CRASH_RESTART_TEST_UNSET_XYZ", true));
        assert!(!env_bool("WYLDE_CRASH_RESTART_TEST_UNSET_XYZ", false));
    }
}
