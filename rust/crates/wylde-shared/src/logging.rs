//! Idempotent root-logger setup for Wylde Rust services.
//!
//! Mirrors `Core/shared/logging_setup.configure_logging` on the Python side:
//! one entry point, callable from anywhere, safe to call twice. The first
//! call installs a `tracing` formatter whose output matches the Python
//! format
//!
//! ```text
//! %(asctime)s [service] %(levelname)s %(name)s: %(message)s
//! ```
//!
//! so merged subprocess log output stays readable across the boundary.
//! Subsequent calls are no-ops. Noisy upstream targets (`hyper`, `h2`,
//! `tokio_util`) are clamped to WARN — the Rust equivalent of Python's
//! `urllib3` / `requests` quieting.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::{
    format::Writer, time::FormatTime, FmtContext, FormatEvent, FormatFields,
};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

static CONFIGURED: OnceLock<()> = OnceLock::new();

struct WyldeTime;

impl FormatTime for WyldeTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        )
    }
}

struct WyldeFormat {
    service: Option<String>,
}

impl<S, N> FormatEvent<S, N> for WyldeFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        WyldeTime.format_time(&mut writer)?;
        writer.write_char(' ')?;
        if let Some(svc) = self.service.as_deref() {
            write!(writer, "[{}] ", svc)?;
        }
        let meta = event.metadata();
        write!(writer, "{} {}: ", meta.level(), meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Configure the root tracing subscriber. The first call wins; later calls
/// are silent no-ops, matching the Python behaviour.
pub fn configure_logging(service: Option<&str>, level: Level) {
    if CONFIGURED.set(()).is_err() {
        // Already configured — still attest the phase so the manifest
        // records the call (mirrors the Python re-entrant path).
        crate::manifest::attest_phase("configure_logging");
        return;
    }
    let default = format!("{}", level).to_lowercase();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&default))
        .add_directive("hyper=warn".parse().expect("static directive"))
        .add_directive("h2=warn".parse().expect("static directive"))
        .add_directive("tokio_util=warn".parse().expect("static directive"));
    let formatter = WyldeFormat {
        service: service.map(str::to_owned),
    };
    let _ = tracing_subscriber::fmt()
        .event_format(formatter)
        .with_env_filter(filter)
        .try_init();
    crate::manifest::attest_phase("configure_logging");
}

// ── Rotating file sinks ──────────────────────────────────────────────
//
// `configure_logging` above owns the *stdout/stderr* formatter. This
// section owns the other half of the logging chokepoint: every log that
// reaches *disk*. A [`RotatingLog`] caps a single file at `max_bytes`
// and keeps `keep_files` rotated generations, so total on-disk growth
// for any sink is bounded at roughly `max_bytes * (keep_files + 1)`.
//
// The policy is inherited by construction — a sink asks for
// [`rotating_sink`] (or, for a subprocess redirect, [`open_rotating_append`])
// and is bounded without knowing the numbers. Opening a raw
// `OpenOptions::new().append(true)` for a persistent log bypasses this
// and is gate-red (wylde_check rule 54: `no_unbounded_log_sink_rust`).

/// Size + retention caps every Wylde file log inherits.
///
/// Both fields are env-overridable so an operator can widen or tighten
/// the bound without a rebuild, but the literal defaults already bound
/// growth out of the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Rotate once the active file reaches this many bytes.
    pub max_bytes: u64,
    /// How many rotated generations (`name.1` … `name.K`) to keep. The
    /// active file plus `keep_files` rotated files bound total on-disk
    /// size at roughly `max_bytes * (keep_files + 1)`.
    pub keep_files: u32,
}

impl RotationPolicy {
    /// 10 MiB active-file cap. At `ipc.jsonl`'s observed ~179 MB/month a
    /// file fills in ~1.7 days, so a rotated generation is ~2 days of
    /// history — fine-grained enough to prune, coarse enough not to
    /// churn renames.
    pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
    /// Keep 5 rotated generations → ~60 MB / ~10 days per sink at the
    /// heaviest current traffic. Was ~179 MB/month unbounded.
    pub const DEFAULT_KEEP_FILES: u32 = 5;
    /// Floor for an env-supplied `max_bytes`; below this rotation would
    /// thrash. A nonsense value falls back to the default.
    pub const MIN_MAX_BYTES: u64 = 4 * 1024;

    /// Load from `WYLDE_LOG_MAX_BYTES` / `WYLDE_LOG_KEEP_FILES`, falling
    /// back to the bounded literal defaults for missing or unparseable
    /// values. `WYLDE_LOG_MAX_BYTES=0` (or anything below
    /// [`MIN_MAX_BYTES`](Self::MIN_MAX_BYTES)) is treated as unset so a
    /// stray `0` can never disable the bound.
    pub fn from_env() -> Self {
        let max_bytes = std::env::var("WYLDE_LOG_MAX_BYTES")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n >= Self::MIN_MAX_BYTES)
            .unwrap_or(Self::DEFAULT_MAX_BYTES);
        let keep_files = std::env::var("WYLDE_LOG_KEEP_FILES")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(Self::DEFAULT_KEEP_FILES);
        Self {
            max_bytes,
            keep_files,
        }
    }
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            keep_files: Self::DEFAULT_KEEP_FILES,
        }
    }
}

/// `path` with `.N` appended (`foo.jsonl` → `foo.jsonl.1`).
fn rotated_name(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

fn remove_if_exists(p: &Path) -> io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Roll `path` → `path.1`, shifting existing generations up and dropping
/// anything past `keep_files`. `keep_files == 0` removes the active file
/// outright. Leaves the active path free for a fresh `create`. The
/// caller must have closed any handle to `path` first (Windows won't
/// rename an open file).
fn rotate_files(path: &Path, keep_files: u32) -> io::Result<()> {
    if keep_files == 0 {
        return remove_if_exists(path);
    }
    // Drop the oldest, then shift `.{i}` → `.{i+1}` from high to low so
    // no rename clobbers a generation we still need.
    remove_if_exists(&rotated_name(path, keep_files))?;
    for i in (1..keep_files).rev() {
        let src = rotated_name(path, i);
        if src.exists() {
            fs::rename(&src, rotated_name(path, i + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, rotated_name(path, 1))?;
    }
    Ok(())
}

struct OpenSink {
    file: File,
    written: u64,
}

/// A file log sink that enforces a [`RotationPolicy`] on every append.
///
/// This is the single sanctioned way a Wylde-owned log reaches disk.
/// Acquire one via [`rotating_sink`] (shared, path-keyed) rather than
/// constructing it directly, so two callers naming the same file share
/// one lock and one rotation counter.
pub struct RotatingLog {
    path: PathBuf,
    policy: RotationPolicy,
    sink: Mutex<Option<OpenSink>>,
}

impl RotatingLog {
    /// A sink for `path` using the env-resolved [`RotationPolicy`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_policy(path, RotationPolicy::from_env())
    }

    /// A sink for `path` with an explicit policy — for tests that need a
    /// tiny cap to exercise rotation quickly.
    pub fn with_policy(path: impl Into<PathBuf>, policy: RotationPolicy) -> Self {
        Self {
            path: path.into(),
            policy,
            sink: Mutex::new(None),
        }
    }

    /// The active (unrotated) file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The policy this sink enforces.
    pub fn policy(&self) -> RotationPolicy {
        self.policy
    }

    /// Append one record, adding a trailing newline. Rotates the active
    /// file first if it has reached the size cap, so on-disk growth stays
    /// bounded at ~`max_bytes * (keep_files + 1)`. A single record larger
    /// than the cap still lands (an empty active file is never rotated) —
    /// the bound is best-effort per-line, exact across lines.
    pub fn write_line(&self, line: &str) -> io::Result<()> {
        let mut guard = self.sink.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(self.open_active()?);
        }
        // Rotate when the active file has reached the cap. Never rotate
        // an empty file, so an over-cap single line is still written.
        let at_cap = guard
            .as_ref()
            .is_some_and(|s| s.written > 0 && s.written >= self.policy.max_bytes);
        if at_cap {
            *guard = None; // drop the handle so the rename can proceed
            rotate_files(&self.path, self.policy.keep_files)?;
            *guard = Some(self.open_active()?);
        }
        let sink = guard.as_mut().expect("sink open");
        sink.file.write_all(line.as_bytes())?;
        sink.file.write_all(b"\n")?;
        sink.file.flush()?;
        sink.written += line.len() as u64 + 1;
        Ok(())
    }

    fn open_active(&self) -> io::Result<OpenSink> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        // wylde-check: unbounded-append-ok — this IS the rotating factory.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(OpenSink { file, written })
    }

    /// Drop the cached handle; the next write reopens. For tests that
    /// redirect the target path out from under a cached sink.
    pub fn close(&self) {
        if let Ok(mut guard) = self.sink.lock() {
            *guard = None;
        }
    }
}

type SinkMap = HashMap<PathBuf, Arc<RotatingLog>>;

fn sink_registry() -> &'static Mutex<SinkMap> {
    static R: OnceLock<Mutex<SinkMap>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The process-shared [`RotatingLog`] for `path`, created on first use.
///
/// This is the front door for every Wylde-owned file log. Callers naming
/// the same path share one sink (one lock, one rotation counter), so
/// concurrent writers never interleave or double-rotate.
pub fn rotating_sink(path: impl AsRef<Path>) -> Arc<RotatingLog> {
    let path = path.as_ref();
    let mut map = sink_registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = map.get(path) {
        return s.clone();
    }
    let sink = Arc::new(RotatingLog::new(path.to_path_buf()));
    map.insert(path.to_path_buf(), sink.clone());
    sink
}

/// Drop every cached sink — for tests that redirect log directories and
/// need the next write to reopen at the new path.
pub fn reset_rotating_sinks() {
    if let Ok(mut map) = sink_registry().lock() {
        for s in map.values() {
            s.close();
        }
        map.clear();
    }
}

/// Rotate `path` if it is already at/over the policy cap, then open it
/// for append and return the raw [`File`].
///
/// For subprocess stdout/stderr redirects that hand a file descriptor to
/// a child: the child writes to the fd directly, bypassing
/// [`RotatingLog::write_line`], so the rotation check runs once at open
/// time. That bounds growth across restarts (each launch rolls an
/// over-cap file) — the best a redirect target can inherit without a
/// Rust writer in the path.
pub fn open_rotating_append(path: &Path, policy: RotationPolicy) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let over_cap = fs::metadata(path)
        .map(|m| m.len() >= policy.max_bytes)
        .unwrap_or(false);
    if over_cap {
        rotate_files(path, policy.keep_files)?;
    }
    // wylde-check: unbounded-append-ok — rotated above; this IS the factory.
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    // `configure_logging` calls `attest_phase`, which mutates the same
    // process-global manifest statics the `manifest::tests` touch. Share the
    // `manifest` serial group so this can't interleave with (and clobber the
    // persisted state of) a concurrent manifest test — the cross-module race
    // that flaked `mark_orphan_dead_works`.
    #[serial(manifest)]
    fn idempotent() {
        configure_logging(Some("test"), Level::INFO);
        configure_logging(Some("test"), Level::DEBUG);
    }

    // ── Rotation ─────────────────────────────────────────────────────

    #[test]
    fn rotation_engages_and_bounds_disk_growth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ipc.jsonl");
        // Tiny cap so a handful of lines trips rotation; keep 2 rotated
        // generations → at most 3 files ever on disk.
        let policy = RotationPolicy {
            max_bytes: 64,
            keep_files: 2,
        };
        let log = RotatingLog::with_policy(&path, policy);

        // Each line is ~20 bytes; 100 lines is ~2 KB, far past the cap.
        // If growth were unbounded the active file would hold all 100.
        for i in 0..100 {
            log.write_line(&format!("record-line-{i:04}"))
                .expect("write");
        }

        // The active file is bounded: it holds at most a cap's worth, not
        // the whole run. This is the core proof — no unbounded append.
        let active_len = std::fs::metadata(&path).expect("active exists").len();
        assert!(
            active_len <= policy.max_bytes + 64,
            "active file grew unbounded: {active_len} bytes"
        );

        // Rotation actually happened: `.1` exists…
        assert!(rotated_name(&path, 1).exists(), "expected a rotated .1");
        // …and retention is enforced: nothing past keep_files survives.
        assert!(
            !rotated_name(&path, policy.keep_files + 1).exists(),
            "retention cap breached: .{} should have been pruned",
            policy.keep_files + 1
        );

        // Total on-disk footprint is bounded by the policy, not the run.
        let mut total = active_len;
        for i in 1..=policy.keep_files {
            if let Ok(m) = std::fs::metadata(rotated_name(&path, i)) {
                total += m.len();
            }
        }
        let ceiling = policy.max_bytes * u64::from(policy.keep_files + 1) + 256;
        assert!(
            total <= ceiling,
            "total footprint {total} exceeds bound {ceiling}"
        );

        // Old data really rolled off — the earliest record is gone from
        // every surviving file, not merely appended-after.
        let mut all = String::new();
        all.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        for i in 1..=policy.keep_files {
            all.push_str(&std::fs::read_to_string(rotated_name(&path, i)).unwrap_or_default());
        }
        assert!(
            !all.contains("record-line-0000"),
            "earliest record should have been pruned by rotation"
        );
        assert!(
            all.contains("record-line-0099"),
            "newest record must survive"
        );
    }

    #[test]
    fn oversize_single_line_still_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.jsonl");
        let log = RotatingLog::with_policy(
            &path,
            RotationPolicy {
                max_bytes: 16,
                keep_files: 3,
            },
        );
        let big = "x".repeat(1024);
        log.write_line(&big).expect("write oversize");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains(&big),
            "an over-cap single line must still land"
        );
    }

    #[test]
    fn open_rotating_append_rolls_over_cap_file_at_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("neo4j.log");
        let policy = RotationPolicy {
            max_bytes: 32,
            keep_files: 3,
        };
        // Pre-seed an over-cap file (as a long-running subprocess would).
        std::fs::write(&path, "a".repeat(100)).expect("seed");
        // Opening for a fresh redirect rolls it first…
        let mut f = open_rotating_append(&path, policy).expect("open");
        f.write_all(b"fresh boot\n").expect("write");
        drop(f);
        assert!(
            rotated_name(&path, 1).exists(),
            "over-cap file should roll to .1"
        );
        let active = std::fs::read_to_string(&path).expect("read active");
        assert!(
            !active.contains("aaaa"),
            "stale content must not stay in the active file"
        );
        assert!(active.contains("fresh boot"));
    }

    #[test]
    fn rotating_sink_shares_one_instance_per_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.jsonl");
        let a = rotating_sink(&path);
        let b = rotating_sink(&path);
        assert!(Arc::ptr_eq(&a, &b), "same path must yield the same sink");
        reset_rotating_sinks();
    }

    #[test]
    fn policy_from_env_defaults_bound_growth() {
        // Defaults must be real literals that bound growth, not unset.
        let p = RotationPolicy::default();
        assert_eq!(p.max_bytes, RotationPolicy::DEFAULT_MAX_BYTES);
        assert_eq!(p.keep_files, RotationPolicy::DEFAULT_KEEP_FILES);
        assert!(p.max_bytes > 0 && p.keep_files > 0);
    }
}
