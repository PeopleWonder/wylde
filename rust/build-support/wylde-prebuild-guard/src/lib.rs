//! Pre-build guard for every wylde-* binary crate.
//!
//! Each `rust/crates/wylde-*/build.rs` is a one-line call into
//! [`run_prebuild_guard`]. This crate is the single home for that
//! policy; the build.rs files are trivial wrappers.
//!
//! ## What this guards against
//!
//! Windows holds an exclusive sharing lock on a running `.exe`. With
//! the Wylde stack up, `cargo build --release -p wylde-lifecycle`
//! compiles fine but the linker fails opaquely with `os error 32` —
//! easy to miss 100+ turns into a coding session. This guard runs
//! *before* the linker so the error message is "wylde-foo.exe is
//! still running, pid 31892, last heartbeat ..." instead of a
//! sharing-violation.
//!
//! ## Sharp scope
//!
//! One question only: "will the linker fail to overwrite the target
//! `.exe`?" No port-collision detection, no health probing, no
//! manifest validation. Those live in their own homes (lifecycle's
//! `service.health`, launcher's port pool, `wylde_check`).
//!
//! ## Signals — union of (live tasklist) ∪ (runtime manifests)
//!
//! Each signal is sometimes wrong on its own:
//!
//! * tasklist misses the python-impl daemons (they run as
//!   `python.exe`, not `wylde-foo.exe`) — but those don't hold the
//!   rust .exe lock either, so they shouldn't block the build;
//! * manifest entries can be stale (the writer crashed and the
//!   launcher hasn't GC'd yet) — those shouldn't block either.
//!
//! Both signals are first narrowed to *this crate's own exe*
//! (`<current_crate>.exe`) — the only image the current compile's
//! linker can overwrite. A running but unrelated `wylde-*` process
//! (e.g. the `wylde-release.exe` preflight tool, a standalone crate
//! this build never links) is therefore not a lock and never blocks.
//! Policy on the narrowed signals: tasklist match → block; manifest
//! entry with fresh heartbeat → block; manifest entry with stale
//! heartbeat → advisory warning only. Heartbeat freshness uses the same
//! [`wylde_shared::manifest_status::heartbeat_age_secs`] the lifecycle
//! daemon's status classifier uses, so "what the guard sees" and
//! "what production sees" never diverge.
//!
//! ## Crate location
//!
//! Lives under `rust/build-support/` rather than `rust/crates/`
//! because (a) it has to `Command::new("tasklist")` and the linter's
//! `no_external_process_spawn_rust` is restricted to wylde-lifecycle,
//! and (b) the linter only walks `rust/crates/<crate>/src/`. Keeping
//! the crate here means we don't have to expand the spawn allowlist.
//!
//! ## Skipping
//!
//! `WYLDE_PREBUILD_GUARD_SKIP=1` bypasses the guard entirely.
//! Reserved for the rare power-user case (e.g. building into a
//! sibling `target-fresh/` tree where the locked exe doesn't matter).

use std::path::PathBuf;
use std::process::Command;

use wylde_shared::manifest_status::{heartbeat_age_secs, ManifestStatus};

/// Heartbeat older than this is treated as a ghost manifest — the
/// launcher GCs it on next startup, so it's downgraded to an advisory
/// warning rather than blocking the build. Matches the "stale" bucket
/// in `wylde-lifecycle::control::classify` (`STALE_MAX_AGE_S`).
pub const HEARTBEAT_STALE_SECS: f64 = 300.0;

/// Build-script entry point. `current_crate` is the name of the wylde-*
/// binary whose `build.rs` is calling — used for the diagnostic banner
/// so the operator knows which compilation aborted.
pub fn run_prebuild_guard(current_crate: &str) {
    // Re-run when the operator flips the skip env var; cargo otherwise
    // caches build.rs based on source-file mtime, which is enough (a
    // fresh `cargo build` already implies a re-run because the source
    // tree's been touched).
    println!("cargo:rerun-if-env-changed=WYLDE_PREBUILD_GUARD_SKIP");

    if !should_run() {
        return;
    }

    let live = tasklist_alive_wylde_exes().unwrap_or_default();
    let manifests = read_runtime_manifests();
    // Sharp scope: building crate `X` only ever relinks `X.exe`. So the only
    // process that can lock *this* compile's output is `<current_crate>.exe` —
    // never some other `wylde-*` image. Narrow both signals to this crate's own
    // exe before classifying, so a live but unrelated tool (e.g. the
    // `wylde-release.exe` preflight binary, a standalone workspace that this
    // build never overwrites) can't false-positive.
    let (live, manifests) = narrow_to_crate(current_crate, live, manifests);
    let (blocking, advisory) = classify(&live, &manifests);

    if blocking.is_empty() && advisory.is_empty() {
        return;
    }

    for line in format_lines(current_crate, &blocking, &advisory) {
        println!("cargo:warning={line}");
    }
    if !blocking.is_empty() {
        panic!(
            "wylde-prebuild-guard: refusing to build {current_crate} while wylde-* daemons are running"
        );
    }
}

/// Whether the guard should actually probe the process table. `false`
/// on debug builds (target/debug/ doesn't collide), non-Windows hosts,
/// or when `WYLDE_PREBUILD_GUARD_SKIP=1` is set.
fn should_run() -> bool {
    if std::env::var("PROFILE").unwrap_or_default() != "release" {
        return false;
    }
    if !cfg!(target_os = "windows") {
        return false;
    }
    if std::env::var("WYLDE_PREBUILD_GUARD_SKIP").ok().as_deref() == Some("1") {
        return false;
    }
    true
}

/// Per-finding row, ready to be formatted into a `cargo:warning=` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardEntry {
    /// Image-name form: e.g. `wylde-gateway.exe`. Always synthesised
    /// with the `.exe` suffix even for manifest-only entries so the
    /// rendered line lines up visually with the live entries.
    pub label: String,
    pub pid: Option<i64>,
    pub heartbeat: Option<String>,
    /// `true` iff this entry came from the live process table — that's
    /// the authoritative lock signal.
    pub live: bool,
}

/// Keep only the signals that describe **this crate's own exe**
/// (`<current_crate>.exe`) — the single image the current compile's linker will
/// try to overwrite. Every other running `wylde-*` process is irrelevant to
/// *this* build: building `wylde-harness` never touches `wylde-gateway.exe`, and
/// certainly never touches `wylde-release.exe` (a standalone tool that isn't even
/// a member of the `rust/` workspace). Pure so the narrowing is unit-testable
/// without spawning `tasklist` or reading manifests.
pub fn narrow_to_crate(
    current_crate: &str,
    live: Vec<String>,
    manifests: Vec<ManifestStatus>,
) -> (Vec<String>, Vec<ManifestStatus>) {
    let target_exe = format!("{current_crate}.exe");
    let live = live
        .into_iter()
        .filter(|e| e.eq_ignore_ascii_case(&target_exe))
        .collect();
    let manifests = manifests
        .into_iter()
        .filter(|m| {
            m.name.eq_ignore_ascii_case(current_crate)
                || prefixed(&m.name).eq_ignore_ascii_case(current_crate)
        })
        .collect();
    (live, manifests)
}

/// Classify the union of (live tasklist) ∪ (runtime manifest) into
/// blocking errors and advisory warnings.
///
/// Pure function: callers pass in the captured signals so the policy
/// is exercisable without spawning `tasklist` or touching the
/// filesystem.
pub fn classify(
    live_exes: &[String],
    manifests: &[ManifestStatus],
) -> (Vec<GuardEntry>, Vec<GuardEntry>) {
    let mut blocking: Vec<GuardEntry> = Vec::new();
    let mut advisory: Vec<GuardEntry> = Vec::new();
    let mut seen_in_live: std::collections::HashSet<String> = std::collections::HashSet::new();

    for exe in live_exes {
        let svc = exe.strip_suffix(".exe").unwrap_or(exe);
        // Match the manifest by either its declared name (`wylde-gateway`)
        // or the prefixed form (`vram-broker` → `wylde-vram-broker`),
        // because the manifest convention is inconsistent.
        let m = manifests
            .iter()
            .find(|m| m.name == svc || prefixed(&m.name) == svc);
        blocking.push(GuardEntry {
            label: exe.clone(),
            pid: m.and_then(|m| m.pid),
            heartbeat: m.and_then(|m| m.heartbeat.clone()),
            live: true,
        });
        seen_in_live.insert(svc.to_owned());
    }

    for m in manifests {
        let candidate_exe = format!("{}.exe", prefixed(&m.name));
        let svc_form = candidate_exe.strip_suffix(".exe").unwrap_or("");
        if seen_in_live.contains(svc_form) {
            continue;
        }
        let age = heartbeat_age_secs(m.heartbeat.as_deref());
        let entry = GuardEntry {
            label: candidate_exe,
            pid: m.pid,
            heartbeat: m.heartbeat.clone(),
            live: false,
        };
        if age > HEARTBEAT_STALE_SECS {
            advisory.push(entry);
        } else {
            blocking.push(entry);
        }
    }
    (blocking, advisory)
}

/// Ensure the service name has the `wylde-` prefix so it lines up with
/// the `.exe` image name. `vram-broker.json` declares
/// `"service": "vram-broker"` but its image is `wylde-vram-broker.exe`.
fn prefixed(name: &str) -> String {
    if name.starts_with("wylde-") {
        name.to_owned()
    } else {
        format!("wylde-{name}")
    }
}

/// Format the operator-facing diagnostic. One leading banner, one line
/// per finding (blocking first), then a remediation hint.
pub fn format_lines(
    current_crate: &str,
    blocking: &[GuardEntry],
    advisory: &[GuardEntry],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !blocking.is_empty() {
        out.push(format!(
            "cannot build {current_crate}: wylde-* daemons are still running"
        ));
        for e in blocking {
            out.push(format_entry(e));
        }
    }
    if !advisory.is_empty() {
        out.push("advisory — stale manifest entries (launcher will GC):".to_string());
        for e in advisory {
            out.push(format_entry(e));
        }
    }
    if !blocking.is_empty() {
        out.push(
            "  → stop the stack first (tray → Shut down, or taskkill /F /IM wylde-*.exe), \
             then re-run cargo. Set WYLDE_PREBUILD_GUARD_SKIP=1 to bypass."
                .to_string(),
        );
    }
    out
}

fn format_entry(e: &GuardEntry) -> String {
    // `[tasklist]` = the OS process table saw the exe — authoritative
    // lock signal. `[manifest]` = manifest-only — no tasklist match;
    // the section header tells you whether it's blocking or advisory.
    let mut s = format!("  * {}", e.label);
    s.push_str(if e.live { " [tasklist]" } else { " [manifest]" });
    match (e.pid, e.heartbeat.as_deref()) {
        (Some(pid), Some(hb)) => s.push_str(&format!(" — pid {pid}, last heartbeat {hb}")),
        (Some(pid), None) => s.push_str(&format!(" — pid {pid}")),
        (None, Some(hb)) => s.push_str(&format!(" — last heartbeat {hb}")),
        (None, None) => {}
    }
    s
}

// ── External signals ──────────────────────────────────────────────────

/// Spawn `tasklist` filtered to `wylde-*` and return the matching image
/// names. Returns `None` if the binary can't be invoked or exits
/// non-zero; the caller treats that as "no info" and proceeds — an
/// unreadable process table must not, by itself, block the build.
fn tasklist_alive_wylde_exes() -> Option<Vec<String>> {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq wylde-*", "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    Some(parse_tasklist_csv(&raw))
}

/// Pure parser over a `tasklist /FO CSV /NH` stdout buffer. Returns
/// sorted, deduplicated `wylde-*.exe` image names. Callable from unit
/// tests with synthetic CSV so the policy doesn't depend on the live
/// Windows binary.
pub fn parse_tasklist_csv(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The "INFO: No tasks are running..." filter-miss banner is
        // also emitted to stdout; CSV /NH rows always start with a
        // quote.
        if !trimmed.starts_with('"') {
            continue;
        }
        let after = &trimmed[1..];
        let Some(end) = after.find('"') else { continue };
        let name = &after[..end];
        if name.starts_with("wylde-") && name.ends_with(".exe") {
            out.push(name.to_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Read `data/manifests/` resolved relative to the build script's
/// `CARGO_MANIFEST_DIR`. Each wylde-* binary crate sits at
/// `rust/crates/<name>` so the repo root is three levels up.
fn read_runtime_manifests() -> Vec<ManifestStatus> {
    let Some(here) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return Vec::new();
    };
    let dir: PathBuf = PathBuf::from(here)
        .join("..")
        .join("..")
        .join("..")
        .join("data")
        .join("manifests");
    wylde_shared::manifest_status::list_runtime_statuses(&dir).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ── tasklist parsing ────────────────────────────────────────────

    #[test]
    fn down_path_yields_no_processes() {
        assert!(parse_tasklist_csv("").is_empty());
        let banner = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(parse_tasklist_csv(banner).is_empty());
    }

    #[test]
    fn alive_path_extracts_image_names_sorted() {
        let csv = concat!(
            "\"wylde-lifecycle.exe\",\"31892\",\"Console\",\"1\",\"7,760 K\"\r\n",
            "\"wylde-vram-broker.exe\",\"13488\",\"Console\",\"1\",\"29,612 K\"\r\n",
            "\"wylde-device-gate.exe\",\"23164\",\"Console\",\"1\",\"6,720 K\"\r\n",
            "\"wylde-gateway.exe\",\"32240\",\"Console\",\"1\",\"13,952 K\"\r\n",
        );
        let names = parse_tasklist_csv(csv);
        assert_eq!(
            names,
            vec![
                "wylde-device-gate.exe".to_string(),
                "wylde-gateway.exe".to_string(),
                "wylde-lifecycle.exe".to_string(),
                "wylde-vram-broker.exe".to_string(),
            ]
        );
    }

    #[test]
    fn filters_non_wylde_and_dedupes() {
        let csv = concat!(
            "\"wylde-lifecycle.exe\",\"31892\",\"Console\",\"1\",\"7,760 K\"\r\n",
            "\"explorer.exe\",\"100\",\"Console\",\"1\",\"100 K\"\r\n",
            "\"wylde-lifecycle.exe\",\"99999\",\"Console\",\"1\",\"7,760 K\"\r\n",
        );
        assert_eq!(
            parse_tasklist_csv(csv),
            vec!["wylde-lifecycle.exe".to_string()]
        );
    }

    // ── narrowing to the crate's own exe ────────────────────────────

    /// A foreign live exe (here the `wylde-release.exe` preflight tool) must
    /// NOT block building an unrelated crate: the linker for `wylde-harness`
    /// never overwrites `wylde-release.exe`. This is the #47 false positive.
    #[test]
    fn foreign_live_exe_is_narrowed_away() {
        let live = vec![
            "wylde-release.exe".to_string(),
            "wylde-gateway.exe".to_string(),
        ];
        let (live, manifests) = narrow_to_crate("wylde-harness", live, vec![]);
        assert!(live.is_empty(), "no live exe matches wylde-harness.exe");
        let (blocking, advisory) = classify(&live, &manifests);
        assert!(blocking.is_empty() && advisory.is_empty());
    }

    /// The crate's OWN live exe still blocks — the real lock case is preserved.
    #[test]
    fn own_live_exe_still_blocks() {
        let live = vec![
            "wylde-harness.exe".to_string(),
            "wylde-release.exe".to_string(),
        ];
        let (live, manifests) = narrow_to_crate("wylde-harness", live, vec![]);
        assert_eq!(live, vec!["wylde-harness.exe".to_string()]);
        let (blocking, _) = classify(&live, &manifests);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].label, "wylde-harness.exe");
    }

    /// Manifest signals narrow the same way, across the `wylde-` prefix quirk:
    /// a fresh `vram-broker` manifest blocks its own build but not a sibling's.
    #[test]
    fn manifest_signals_narrow_to_crate() {
        let manifests = vec![
            ManifestStatus {
                name: "vram-broker".to_string(),
                pid: Some(13488),
                heartbeat: Some(fresh_heartbeat()),
                state: Some("alive".to_string()),
            },
            ManifestStatus {
                name: "wylde-gateway".to_string(),
                pid: Some(1),
                heartbeat: Some(fresh_heartbeat()),
                state: Some("alive".to_string()),
            },
        ];
        // Building wylde-harness: neither manifest is ours → nothing blocks.
        let (_, kept) = narrow_to_crate("wylde-harness", vec![], manifests.clone());
        assert!(kept.is_empty());
        // Building wylde-vram-broker: the prefix-stripped manifest is ours.
        let (_, kept) = narrow_to_crate("wylde-vram-broker", vec![], manifests);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "vram-broker");
    }

    // ── classification ──────────────────────────────────────────────

    fn fresh_heartbeat() -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(15))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn stale_heartbeat() -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    /// Live tasklist entry that also has a manifest: blocking, with
    /// pid + heartbeat surfaced from the manifest.
    #[test]
    fn live_with_manifest_blocks_and_carries_pid() {
        let live = vec!["wylde-gateway.exe".to_string()];
        let manifests = vec![ManifestStatus {
            name: "wylde-gateway".to_string(),
            pid: Some(32240),
            heartbeat: Some(fresh_heartbeat()),
            state: Some("alive".to_string()),
        }];
        let (blocking, advisory) = classify(&live, &manifests);
        assert_eq!(blocking.len(), 1);
        assert!(advisory.is_empty());
        assert_eq!(blocking[0].label, "wylde-gateway.exe");
        assert_eq!(blocking[0].pid, Some(32240));
        assert!(blocking[0].live);
    }

    /// Manifest convention quirk: `vram-broker.json` declares
    /// `"service": "vram-broker"` (no prefix), but its image is
    /// `wylde-vram-broker.exe`. The classifier must match across the
    /// prefix.
    #[test]
    fn live_matches_manifest_across_wylde_prefix_strip() {
        let live = vec!["wylde-vram-broker.exe".to_string()];
        let manifests = vec![ManifestStatus {
            name: "vram-broker".to_string(),
            pid: Some(13488),
            heartbeat: Some(fresh_heartbeat()),
            state: Some("alive".to_string()),
        }];
        let (blocking, _) = classify(&live, &manifests);
        assert_eq!(blocking[0].pid, Some(13488));
    }

    /// Live tasklist entry with NO matching manifest (e.g.
    /// `wylde-lifecycle.exe` doesn't write to data/manifests):
    /// still blocking, just without pid detail.
    #[test]
    fn live_without_manifest_still_blocks() {
        let live = vec!["wylde-lifecycle.exe".to_string()];
        let (blocking, advisory) = classify(&live, &[]);
        assert_eq!(blocking.len(), 1);
        assert!(advisory.is_empty());
        assert!(blocking[0].live);
        assert_eq!(blocking[0].pid, None);
    }

    /// Stale manifest with NO live counterpart: advisory only — the
    /// launcher will GC it; building over the (non-existent) exe is
    /// safe.
    #[test]
    fn stale_manifest_only_is_advisory_not_blocking() {
        let manifests = vec![ManifestStatus {
            name: "wylde-voice".to_string(),
            pid: Some(9999),
            heartbeat: Some(stale_heartbeat()),
            state: Some("alive".to_string()),
        }];
        let (blocking, advisory) = classify(&[], &manifests);
        assert!(blocking.is_empty());
        assert_eq!(advisory.len(), 1);
        assert_eq!(advisory[0].label, "wylde-voice.exe");
        assert!(!advisory[0].live);
    }

    /// Fresh manifest with NO live counterpart: a service that thinks
    /// it's alive (python-impl or just-started rust-impl whose
    /// tasklist hasn't refreshed). Block — the manifest is the more
    /// recent signal.
    #[test]
    fn fresh_manifest_only_blocks() {
        let manifests = vec![ManifestStatus {
            name: "wylde-gateway".to_string(),
            pid: Some(1234),
            heartbeat: Some(fresh_heartbeat()),
            state: Some("alive".to_string()),
        }];
        let (blocking, advisory) = classify(&[], &manifests);
        assert_eq!(blocking.len(), 1);
        assert!(advisory.is_empty());
    }

    /// All-clean: no live processes and no manifests → both sets
    /// empty, the build script returns without panicking.
    #[test]
    fn nothing_alive_no_findings() {
        let (b, a) = classify(&[], &[]);
        assert!(b.is_empty() && a.is_empty());
    }

    // ── formatting ─────────────────────────────────────────────────

    #[test]
    fn format_lines_omits_empty_sections() {
        let lines = format_lines("wylde-gateway", &[], &[]);
        assert!(lines.is_empty(), "no findings ⇒ no output");
    }

    #[test]
    fn format_lines_includes_pid_and_heartbeat_in_block() {
        let blocking = vec![GuardEntry {
            label: "wylde-gateway.exe".to_string(),
            pid: Some(32240),
            heartbeat: Some("2026-05-22T23:00:15Z".to_string()),
            live: true,
        }];
        let lines = format_lines("wylde-gateway", &blocking, &[]);
        assert!(lines[0].contains("wylde-gateway"));
        assert!(lines[1].contains("wylde-gateway.exe"));
        assert!(lines[1].contains("[tasklist]"));
        assert!(lines[1].contains("pid 32240"));
        assert!(lines[1].contains("2026-05-22T23:00:15Z"));
        assert!(lines.last().unwrap().contains("WYLDE_PREBUILD_GUARD_SKIP"));
    }

    #[test]
    fn format_lines_advisory_only_has_no_remediation_hint() {
        let advisory = vec![GuardEntry {
            label: "wylde-voice.exe".to_string(),
            pid: Some(7),
            heartbeat: Some(stale_heartbeat()),
            live: false,
        }];
        let lines = format_lines("wylde-voice", &[], &advisory);
        // advisory-only → no remediation footer, just the section.
        assert!(lines.iter().any(|l| l.contains("stale manifest")));
        assert!(!lines.iter().any(|l| l.contains("taskkill")));
        assert!(lines.iter().any(|l| l.contains("[manifest]")));
    }

    // ── end-to-end (shared primitives, no spawn) ───────────────────

    #[test]
    fn end_to_end_with_synthetic_manifests_and_live_set() {
        // Drive the full classify + format pipeline with a tempdir
        // manifest and a synthetic tasklist set.
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wylde-gateway.json"),
            format!(
                r#"{{"service": "wylde-gateway",
                     "status": {{"pid": 32240, "heartbeat": "{}", "state": "alive"}}}}"#,
                fresh_heartbeat()
            ),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("vram-broker.json"),
            format!(
                r#"{{"service": "vram-broker",
                     "status": {{"pid": 13488, "heartbeat": "{}", "state": "alive"}}}}"#,
                stale_heartbeat()
            ),
        )
        .unwrap();
        let manifests =
            wylde_shared::manifest_status::list_runtime_statuses(tmp.path()).expect("Some(vec)");
        let live = vec!["wylde-gateway.exe".to_string()];
        let (blocking, advisory) = classify(&live, &manifests);
        assert_eq!(blocking.len(), 1, "gateway is live → blocking");
        assert_eq!(blocking[0].pid, Some(32240));
        assert_eq!(
            advisory.len(),
            1,
            "vram-broker stale manifest only → advisory"
        );
        assert!(advisory[0].label.contains("wylde-vram-broker"));
    }
}
