//! Background startup update check + its shared result (Phase 12.5,
//! slice 3d).
//!
//! On launch the Shell fires one fire-and-forget [`run_startup_check`].
//! The resolved [`wylde_updater::UpdateStatus`] is cached here so two
//! surfaces can read it without re-checking:
//!
//!   * the **Shell sidebar** shows a hint dot on the Settings row when an
//!     update is available ([`update_available`]);
//!   * the **Settings panel** seeds its manual-check state from
//!     [`available_info`] so the user lands on a ready "Install" button
//!     instead of having to click "Check now" again.
//!
//! Lives in `wylde-gui-pipe` for the same reason as [`crate::nav_bus`]:
//! it's shared between the Shell and a panel crate, and neither may depend
//! on the other. The crate also already owns the tokio bridge the
//! blocking updater needs ([`crate::bridged_spawn_blocking`]) and the
//! lifecycle-action helper that reads the prefs.
//!
//! ## Privacy
//!
//! Wylde is privacy-first: it makes **no** update network call unless the
//! user has turned updates on *and* opted into automatic checks. The
//! Settings UI states this verbatim ("When off, no automatic network
//! calls"). [`due_for_check`] is the gate — the startup check is a no-op
//! (no GitHub request, nothing recorded) unless `enabled && auto_check`
//! and the chosen cadence window has elapsed since `last_checked`.

use std::sync::{Mutex, OnceLock};

use wylde_updater::{Channel, UpdateInfo, UpdateStatus};

/// Cached outcome of the one startup check.
#[derive(Debug, Clone, Default)]
pub struct StartupCheck {
    /// `None` until a check actually runs (it's skipped entirely by the
    /// privacy/cadence gate). `Some(Ok(..))` once a check resolves,
    /// `Some(Err(msg))` if it failed (network down, parse error, …).
    pub outcome: Option<Result<UpdateStatus, String>>,
    /// Unix epoch (seconds) the check ran, when it ran.
    pub checked_at: Option<u64>,
}

fn cell() -> &'static Mutex<StartupCheck> {
    static STARTUP_CHECK: OnceLock<Mutex<StartupCheck>> = OnceLock::new();
    STARTUP_CHECK.get_or_init(|| Mutex::new(StartupCheck::default()))
}

/// Store the result of a completed startup check.
fn record(outcome: Result<UpdateStatus, String>, checked_at: u64) {
    if let Ok(mut guard) = cell().lock() {
        guard.outcome = Some(outcome);
        guard.checked_at = Some(checked_at);
    }
}

/// Snapshot the cached startup-check state (clones the stored outcome).
pub fn snapshot() -> StartupCheck {
    cell().lock().map(|g| g.clone()).unwrap_or_default()
}

/// `true` iff the startup check completed and found an available update.
/// The Shell reads this each render to decide whether to badge the
/// Settings row.
pub fn update_available() -> bool {
    matches!(snapshot().outcome, Some(Ok(UpdateStatus::Available(_))))
}

/// The resolved [`UpdateInfo`] when the startup check found an update,
/// else `None`. Lets the Settings panel seed its manual-check state with
/// the already-resolved binary + signature assets.
pub fn available_info() -> Option<UpdateInfo> {
    match snapshot().outcome {
        Some(Ok(UpdateStatus::Available(info))) => Some(info),
        _ => None,
    }
}

/// Seconds in the cadence window for a persisted `frequency` string.
/// Unknown/legacy values fall back to weekly (the prefs default).
fn cadence_secs(frequency: &str) -> u64 {
    const DAY: u64 = 24 * 60 * 60;
    match frequency {
        "daily" => DAY,
        "monthly" => 30 * DAY,
        _ => 7 * DAY, // weekly
    }
}

/// The privacy + cadence gate for the startup check.
///
/// Returns `true` — meaning "make the network call" — only when the user
/// has both enabled updates and opted into automatic checks, *and* enough
/// time has passed since the last recorded check for the chosen cadence.
/// A never-checked install (`last_checked == None`) is due immediately.
///
/// Pure over its inputs so the gate is unit-tested without a clock, the
/// pipe, or the network.
pub fn due_for_check(
    enabled: bool,
    auto_check: bool,
    frequency: &str,
    last_checked: Option<u64>,
    now_secs: u64,
) -> bool {
    if !(enabled && auto_check) {
        return false;
    }
    match last_checked {
        None => true,
        Some(ts) => {
            // The lifecycle daemon may persist seconds or milliseconds;
            // normalise by magnitude (matches the Settings footer's
            // `humanize_since`). A seconds-epoch won't reach 1e12 until
            // the year 33658.
            let ts_secs = if ts >= 1_000_000_000_000 {
                ts / 1000
            } else {
                ts
            };
            now_secs.saturating_sub(ts_secs) >= cadence_secs(frequency)
        }
    }
}

/// Whether an available `version` should be suppressed because the user
/// skipped exactly it. Pure so the "Skip this version" gate is unit-tested
/// without the pipe or the network.
fn skip_suppresses(version: &str, skipped: Option<&str>) -> bool {
    skipped == Some(version)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the one background update check, honouring the privacy/cadence
/// gate, and cache the result. Returns `true` iff an update is available
/// (so the caller can flip a UI flag in one step).
///
/// Fire-and-forget: callers `cx.spawn` this and `cx.notify()` on the
/// returned bool. When the gate says "not due" this is a pure no-op — no
/// GitHub request is made and nothing is recorded.
///
/// `current_version` is the running binary's `CARGO_PKG_VERSION`.
pub async fn run_startup_check(current_version: &str) -> bool {
    // Read the user's persisted update preferences. A pref read that fails
    // (lifecycle daemon not up yet) means we simply don't check — never
    // fall through to a network call on an unreadable policy.
    let prefs = match crate::lifecycle_action("updater.get_prefs", serde_json::json!({})).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "updater startup check: prefs unavailable; skipping");
            return false;
        }
    };
    let enabled = prefs
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let auto_check = prefs
        .get("auto_check")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let frequency = prefs
        .get("frequency")
        .and_then(|v| v.as_str())
        .unwrap_or("weekly")
        .to_owned();
    let channel_str = prefs
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("stable")
        .to_owned();
    let last_checked = prefs.get("last_checked").and_then(|v| v.as_u64());
    let skipped_version = prefs
        .get("skipped_version")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let now = now_secs();
    if !due_for_check(enabled, auto_check, &frequency, last_checked, now) {
        tracing::debug!(
            enabled,
            auto_check,
            frequency = %frequency,
            "updater startup check: not due — no network call made"
        );
        return false;
    }

    // Hop the blocking updater onto the tokio bridge (gpui's executor has
    // no reactor). Same path the Settings "Check now" button uses.
    let channel = Channel::from_str_lossy(&channel_str);
    let version = current_version.to_owned();
    let outcome = crate::bridged_spawn_blocking(move || {
        wylde_updater::check_for_update(channel, &version).map_err(|e| e.to_string())
    })
    .await;

    // Honour the "Skip this version" decision on the *automatic* path only:
    // if the resolved update is the exact version the user declined, treat
    // it as up-to-date so the sidebar badge and the Settings seed stay
    // quiet. A newer release carries a different version string and so is
    // never suppressed (the skip self-expires). The manual "Check now"
    // button deliberately ignores this — an explicit query always shows the
    // real answer, which is also the user's path to un-skip.
    let outcome = match outcome {
        Ok(UpdateStatus::Available(info))
            if skip_suppresses(&info.version, skipped_version.as_deref()) =>
        {
            tracing::info!(
                version = %info.version,
                "updater startup check: update available but user skipped this version"
            );
            Ok(UpdateStatus::UpToDate {
                current: current_version.to_owned(),
            })
        }
        other => other,
    };

    let available = matches!(&outcome, Ok(UpdateStatus::Available(_)));
    match &outcome {
        Ok(UpdateStatus::Available(info)) => {
            tracing::info!(version = %info.version, "updater startup check: update available");
        }
        Ok(UpdateStatus::UpToDate { .. }) => {
            tracing::info!("updater startup check: up to date");
        }
        Err(e) => tracing::warn!(error = %e, "updater startup check failed"),
    }
    record(outcome, now);

    // Persist `last_checked` so the Settings footer reflects this run even
    // when there's no update. Best-effort: a failed write just leaves the
    // footer showing the prior check time until the next manual one.
    let _ = crate::lifecycle_action(
        "updater.set_prefs",
        serde_json::json!({ "last_checked": now }),
    )
    .await;

    available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_blocks_when_updates_disabled() {
        // The privacy contract: master toggle off ⇒ no network call, even
        // if auto_check somehow reads true.
        assert!(!due_for_check(false, true, "daily", None, 1_000_000));
        assert!(!due_for_check(false, false, "daily", None, 1_000_000));
    }

    #[test]
    fn gate_blocks_when_auto_check_disabled() {
        // Enabled but manual-only ⇒ the startup check stays silent; the
        // user drives checks with the "Check now" button.
        assert!(!due_for_check(true, false, "daily", None, 1_000_000));
    }

    #[test]
    fn gate_allows_first_ever_check_when_opted_in() {
        // enabled + auto_check + never checked ⇒ due immediately.
        assert!(due_for_check(true, true, "weekly", None, 1_000_000));
    }

    #[test]
    fn gate_respects_cadence_window() {
        let now = 10_000_000u64;
        let day = 24 * 60 * 60;
        // Checked 12 hours ago on a daily cadence ⇒ not yet due.
        assert!(!due_for_check(
            true,
            true,
            "daily",
            Some(now - day / 2),
            now
        ));
        // Checked just over a day ago ⇒ due.
        assert!(due_for_check(true, true, "daily", Some(now - day - 1), now));
        // Weekly cadence: a 2-day-old check is not due.
        assert!(!due_for_check(
            true,
            true,
            "weekly",
            Some(now - 2 * day),
            now
        ));
        // ...but an 8-day-old one is.
        assert!(due_for_check(
            true,
            true,
            "weekly",
            Some(now - 8 * day),
            now
        ));
    }

    #[test]
    fn gate_normalises_millisecond_timestamps() {
        let now = 2_000_000_000u64;
        let day = 24 * 60 * 60;
        // last_checked expressed in millis, two days ago, weekly ⇒ not due.
        let ts_millis = (now - 2 * day) * 1000;
        assert!(!due_for_check(true, true, "weekly", Some(ts_millis), now));
    }

    #[test]
    fn skip_suppresses_only_the_exact_version() {
        // The skipped version is suppressed...
        assert!(skip_suppresses("0.3.1", Some("0.3.1")));
        // ...but a newer release (different string) is not — skip self-expires.
        assert!(!skip_suppresses("0.3.2", Some("0.3.1")));
        // No skip recorded ⇒ never suppress.
        assert!(!skip_suppresses("0.3.1", None));
    }

    #[test]
    fn cadence_maps_each_frequency() {
        let day = 24 * 60 * 60;
        assert_eq!(cadence_secs("daily"), day);
        assert_eq!(cadence_secs("weekly"), 7 * day);
        assert_eq!(cadence_secs("monthly"), 30 * day);
        // Unknown/legacy ⇒ weekly baseline (matches the prefs default).
        assert_eq!(cadence_secs("nightly"), 7 * day);
    }

    #[test]
    fn default_snapshot_reports_no_update() {
        // Before any check records a result, the accessors are quiet.
        // (The OnceLock is process-wide; a sibling test in this module
        // never records, so this holds as long as nothing calls `record`.)
        let snap = StartupCheck::default();
        assert!(snap.outcome.is_none());
        assert!(snap.checked_at.is_none());
    }

    #[test]
    fn record_then_read_round_trips_available() {
        // Exercises the cache accessors over the process-wide cell.
        let info = UpdateInfo {
            version: "9.9.9".into(),
            tag: "v9.9.9".into(),
            notes: "test".into(),
            html_url: "https://example.test/r".into(),
            binary: wylde_updater::ReleaseAsset {
                name: "wylde-gui.exe".into(),
                url: "https://example.test/bin".into(),
                size: 1,
            },
            signature: wylde_updater::ReleaseAsset {
                name: "wylde-gui.exe.minisig".into(),
                url: "https://example.test/sig".into(),
                size: 1,
            },
        };
        record(Ok(UpdateStatus::Available(info.clone())), 1_234);
        assert!(update_available());
        assert_eq!(available_info().map(|i| i.version), Some("9.9.9".into()));
        assert_eq!(snapshot().checked_at, Some(1_234));
        // Reset to UpToDate so we don't leak "available" into other tests
        // sharing this process-wide cell.
        record(
            Ok(UpdateStatus::UpToDate {
                current: "9.9.9".into(),
            }),
            1_235,
        );
        assert!(!update_available());
        assert!(available_info().is_none());
    }
}
