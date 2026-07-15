//! `bench` + `preflight` command orchestration and the publish-time receipt
//! gate. The pure logic lives in [`crate::bench::spec`] and [`crate::receipt`];
//! this module is the operator-facing glue: run the harnesses, run G7, print a
//! legible report, write the baseline/receipt, and enforce the receipt at
//! publish time.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::bench::{self, Baseline, Provenance};
use crate::receipt::{self, BenchDelta, GateOutcome, Receipt, RECEIPT_FILENAME, RECEIPT_SCHEMA};

/// The reasoner model + quant the arms run against — mirrors
/// `wylde_harness::turn::reasoning::config::DEFAULT_REASONER_MODEL`. Informational
/// only (recorded in the baseline/receipt so numbers are comparable like-for-like);
/// override the live run with `WYLDE_EVAL_MODEL`.
const DEFAULT_MODEL: &str = "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS";

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Reps per reasoning arm (the medians are taken over these). More reps =
    /// less noise, more wall-clock. 2 is the preflight default; use 3+ when
    /// recording a baseline you want to trust.
    #[arg(long, default_value_t = 2)]
    pub reps: u32,
    /// Path to the committed baseline JSON.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Repo root (defaults to the git top-level of the current dir).
    #[arg(long)]
    pub repo_root: Option<PathBuf>,
    /// Record (or re-record) the baseline from THIS run instead of comparing.
    /// The deliberate, explicit path so numbers never drift up silently — a
    /// regression can only become the new normal on purpose.
    #[arg(long)]
    pub accept_baseline: bool,
    /// Treat a skipped (unavailable) benchmark as non-blocking. Off by default:
    /// a required benchmark we couldn't run keeps the gate from going green.
    #[arg(long)]
    pub allow_skips: bool,
    /// Skip the (slow) reasoning eval — for a quick lexical-only check.
    #[arg(long)]
    pub skip_reasoning: bool,
    /// Skip the lexical eval.
    #[arg(long)]
    pub skip_lexical: bool,
    /// Scratch dir for harness output (defaults to `<repo>/target/bench-run`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Reuse existing harness output in `--out` instead of re-running the
    /// (slow, live) harnesses. For re-tuning thresholds or re-recording a
    /// baseline from a run you already have.
    #[arg(long)]
    pub reuse_out: bool,
    /// Human rig label recorded in a baseline.
    #[arg(long, default_value = "Aaron's dev rig")]
    pub host_label: String,
    /// Don't append this run to the trend history.
    #[arg(long)]
    pub no_history: bool,
}

#[derive(Args, Debug)]
pub struct PreflightArgs {
    #[arg(long, default_value_t = 2)]
    pub reps: u32,
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    #[arg(long)]
    pub repo_root: Option<PathBuf>,
    /// Allow a skipped benchmark without turning the receipt red (records the
    /// skip honestly in the receipt regardless).
    #[arg(long)]
    pub allow_skips: bool,
    /// Also run an L1-lite artifact build (`cargo build --release` for the
    /// backend + GUI workspaces). Off by default — CI already builds; the
    /// preflight's unique value is the live benchmarks. Full installer/NSIS
    /// build stays a manual L1 step.
    #[arg(long)]
    pub build: bool,
    #[arg(long)]
    pub skip_reasoning: bool,
    #[arg(long)]
    pub skip_lexical: bool,
    /// Where to write the receipt (defaults to `<repo>/preflight-receipt.json`).
    #[arg(long)]
    pub receipt: Option<PathBuf>,
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Reuse existing harness output in `--out` instead of re-running.
    #[arg(long)]
    pub reuse_out: bool,
    #[arg(long, default_value = "Aaron's dev rig")]
    pub host_label: String,
    #[arg(long)]
    pub no_history: bool,
}

// ── `bench` ─────────────────────────────────────────────────────────────────

pub fn run_bench(args: BenchArgs) -> Result<()> {
    let repo_root = resolve_repo_root(args.repo_root.as_deref())?;
    let baseline_path = args
        .baseline
        .clone()
        .unwrap_or_else(|| repo_root.join(bench::BASELINE_REL_PATH));
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| repo_root.join("target").join("bench-run"));

    let run_opts = bench::RunOpts {
        repo_root: repo_root.clone(),
        reps: args.reps,
        out_dir,
        skip_reasoning: args.skip_reasoning,
        skip_lexical: args.skip_lexical,
        reuse: args.reuse_out,
    };

    println!("== wylde benchmark gate ==");
    let measurements = bench::run_all(&run_opts)?;
    for s in &measurements.skips {
        println!("  ⚠ {s}");
    }

    if args.accept_baseline {
        let commit = crate::host::head_commit(&repo_root)?;
        let prior = Baseline::load(&baseline_path).ok();
        let provenance = Provenance {
            commit,
            date: today_ymd(),
            reps: args.reps,
            host: crate::host::capture(&args.host_label, DEFAULT_MODEL),
        };
        let baseline = bench::record_baseline(prior.as_ref(), &measurements, provenance)?;
        baseline.save(&baseline_path)?;
        println!(
            "\n✓ recorded baseline ({} metrics) → {}",
            baseline.metrics.len(),
            baseline_path.display()
        );
        print_recorded(&baseline);
        maybe_append_history(
            &args.no_history,
            &repo_root,
            &baseline.recorded.commit,
            true,
            &measurements,
        );
        return Ok(());
    }

    let baseline = Baseline::load(&baseline_path).with_context(|| {
        format!(
            "no baseline at {} — record one first with `wylde-release bench --accept-baseline`",
            baseline_path.display()
        )
    })?;
    let report = bench::compare(&baseline, &measurements);
    print_report(&baseline, &report);

    let commit = crate::host::head_commit(&repo_root).unwrap_or_default();
    maybe_append_history(
        &args.no_history,
        &repo_root,
        &commit,
        report.is_green(args.allow_skips),
        &measurements,
    );

    if !report.is_green(args.allow_skips) {
        bail!("benchmark gate FAILED — see the regressions above");
    }
    println!("\n✓ benchmark gate PASSED");
    Ok(())
}

// ── `preflight` ─────────────────────────────────────────────────────────────

pub fn run_preflight(args: PreflightArgs) -> Result<()> {
    let repo_root = resolve_repo_root(args.repo_root.as_deref())?;
    let baseline_path = args
        .baseline
        .clone()
        .unwrap_or_else(|| repo_root.join(bench::BASELINE_REL_PATH));
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| repo_root.join("target").join("bench-run"));
    let receipt_path = args
        .receipt
        .clone()
        .unwrap_or_else(|| repo_root.join(RECEIPT_FILENAME));

    println!("== wylde-release preflight ==");
    let commit = crate::host::head_commit(&repo_root)?;
    let git_dirty = crate::host::is_dirty(&repo_root)?;
    let version = workspace_version(&repo_root)?;
    println!(
        "commit  : {commit}{}",
        if git_dirty { " (DIRTY)" } else { "" }
    );
    println!("version : {version}");

    let mut gates: std::collections::BTreeMap<String, GateOutcome> = Default::default();
    let mut warnings: Vec<String> = Vec::new();

    // — G7 version consistency —
    let g7 = run_g7(&repo_root)?;
    gates.insert(
        "version_consistency_g7".into(),
        if g7 {
            GateOutcome::Pass
        } else {
            GateOutcome::Fail
        },
    );
    println!(
        "G7 version-consistency: {}",
        if g7 { "PASS" } else { "FAIL" }
    );

    // — Optional L1-lite artifact build —
    if args.build {
        let built = run_build(&repo_root);
        gates.insert(
            "build_artifacts".into(),
            if built {
                GateOutcome::Pass
            } else {
                GateOutcome::Fail
            },
        );
        println!(
            "L1 build (backend + GUI, release): {}",
            if built { "PASS" } else { "FAIL" }
        );
    } else {
        gates.insert("build_artifacts".into(), GateOutcome::Skipped);
        warnings.push("L1 artifact build skipped (pass --build to include it)".into());
    }

    // — The benchmark gate —
    let run_opts = bench::RunOpts {
        repo_root: repo_root.clone(),
        reps: args.reps,
        out_dir,
        skip_reasoning: args.skip_reasoning,
        skip_lexical: args.skip_lexical,
        reuse: args.reuse_out,
    };
    let measurements = bench::run_all(&run_opts)?;
    for s in &measurements.skips {
        warnings.push(s.clone());
    }
    let baseline = Baseline::load(&baseline_path).with_context(|| {
        format!(
            "no baseline at {} — record one with `wylde-release bench --accept-baseline`",
            baseline_path.display()
        )
    })?;
    let report = bench::compare(&baseline, &measurements);
    print_report(&baseline, &report);
    let bench_green = report.is_green(args.allow_skips);
    gates.insert(
        "benchmarks".into(),
        if bench_green {
            GateOutcome::Pass
        } else {
            GateOutcome::Fail
        },
    );
    for c in &report.comparisons {
        if matches!(c.status, crate::bench::spec::Status::Warn) {
            warnings.push(format!("{}: {}", c.key, c.detail));
        }
        if matches!(c.status, crate::bench::spec::Status::Improved) {
            warnings.push(format!(
                "{}: improved past the baseline — consider `bench --accept-baseline` ({})",
                c.key, c.detail
            ));
        }
    }

    // — Roll up + write the receipt —
    let benchmarks: std::collections::BTreeMap<String, BenchDelta> = report
        .comparisons
        .iter()
        .map(|c| {
            (
                c.key.clone(),
                BenchDelta {
                    baseline: c.baseline,
                    current: c.current,
                    status: c.status.tag().to_string(),
                    gate: format!("{:?}", c.gate).to_lowercase(),
                    detail: c.detail.clone(),
                },
            )
        })
        .collect();

    let all_green =
        !git_dirty && gates.values().all(|g| !matches!(g, GateOutcome::Fail)) && bench_green;

    let rec = Receipt {
        schema: RECEIPT_SCHEMA,
        commit: commit.clone(),
        git_dirty,
        version: version.clone(),
        timestamp: now_utc_iso(),
        host: crate::host::capture(&args.host_label, DEFAULT_MODEL),
        gates,
        benchmarks,
        warnings: warnings.clone(),
        all_green,
    };
    rec.save(&receipt_path)?;
    println!("\nreceipt → {}", receipt_path.display());
    for w in &warnings {
        println!("  ⚠ {w}");
    }

    maybe_append_history(
        &args.no_history,
        &repo_root,
        &commit,
        all_green,
        &measurements,
    );

    if git_dirty {
        bail!(
            "working tree is DIRTY — the receipt cannot describe a reproducible commit. \
             Commit your changes and re-run preflight."
        );
    }
    if !all_green {
        bail!(
            "preflight is NOT green — see the failures above. `publish` will refuse this receipt."
        );
    }
    println!(
        "\n✓ preflight GREEN — receipt valid for {version} at {}",
        &commit[..8.min(commit.len())]
    );
    Ok(())
}

/// The publish-time gate: load the receipt next to the repo and validate it for
/// the version being published at the current HEAD. Called by `main::publish`.
pub fn enforce_receipt_for_publish(repo_root: Option<&Path>, version: &str) -> Result<()> {
    let repo_root = resolve_repo_root(repo_root)?;
    let receipt_path = repo_root.join(RECEIPT_FILENAME);
    if !receipt_path.exists() {
        bail!(
            "no preflight receipt at {} — run `wylde-release preflight` before publishing \
             (or `--no-preflight-receipt` to bypass, deliberately)",
            receipt_path.display()
        );
    }
    let rec = Receipt::load(&receipt_path)?;
    let head = crate::host::head_commit(&repo_root)?;
    let tag_version = version.strip_prefix('v').unwrap_or(version);
    receipt::validate_for_publish(&rec, &head, tag_version).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve the repo root: explicit flag, else `git rev-parse --show-toplevel`.
fn resolve_repo_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("spawning git rev-parse --show-toplevel")?;
    if !out.status.success() {
        bail!(
            "not in a git repo (pass --repo-root): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Read the `[workspace.package]` version from `rust/Cargo.toml`. Mirrors the
/// awk in `tools/check-versions.sh` — the tool needs the string, and a tiny
/// parser beats shelling out and parsing stdout.
fn workspace_version(repo_root: &Path) -> Result<String> {
    let toml_path = repo_root.join("rust").join("Cargo.toml");
    let text = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let mut in_wp = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            in_wp = t.starts_with("[workspace.package]");
            continue;
        }
        if in_wp {
            if let Some(rest) = t.strip_prefix("version") {
                if let Some(q0) = rest.find('"') {
                    if let Some(q1) = rest[q0 + 1..].find('"') {
                        return Ok(rest[q0 + 1..q0 + 1 + q1].to_string());
                    }
                }
            }
        }
    }
    bail!(
        "could not find [workspace.package] version in {}",
        toml_path.display()
    )
}

/// Run G7 (`tools/check-versions.sh`) via bash; success = green.
fn run_g7(repo_root: &Path) -> Result<bool> {
    let status = Command::new("bash")
        .current_dir(repo_root)
        .arg("tools/check-versions.sh")
        .status()
        .context("spawning bash tools/check-versions.sh (git-bash on PATH?)")?;
    Ok(status.success())
}

/// L1-lite: `cargo build --release` for the backend and GUI workspaces. Returns
/// whether both built.
fn run_build(repo_root: &Path) -> bool {
    let backend = Command::new("cargo")
        .current_dir(repo_root.join("rust"))
        .args(["build", "--release", "--workspace", "--locked"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let gui = Command::new("cargo")
        .current_dir(repo_root.join("Core").join("GUI"))
        .args(["build", "--release", "--workspace", "--locked"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    backend && gui
}

fn maybe_append_history(
    no_history: &bool,
    repo_root: &Path,
    commit: &str,
    green: bool,
    m: &bench::Measurements,
) {
    if *no_history {
        return;
    }
    // Default history lives in the private planning repo (junctioned in).
    let path = repo_root
        .join("outputs")
        .join("benchmarks")
        .join("history.jsonl");
    let rec = bench::HistoryRecord {
        timestamp: &now_utc_iso(),
        commit,
        green,
        values: &m.values,
    };
    match bench::append_history(&path, &rec) {
        Ok(true) => println!("  ↳ appended to trend history {}", path.display()),
        Ok(false) => { /* planning junction not mounted — fine */ }
        Err(e) => eprintln!("  ⚠ could not append history: {e}"),
    }
}

// ── Report printing ─────────────────────────────────────────────────────────

fn print_recorded(base: &Baseline) {
    println!(
        "  host: {} · {} · model {}",
        base.recorded.host.label, base.recorded.host.gpu, base.recorded.host.model
    );
    for (k, mb) in &base.metrics {
        println!("  {k:<44} {:>10.3} {}", mb.value, mb.unit);
    }
}

fn print_report(base: &Baseline, report: &crate::bench::spec::Report) {
    println!(
        "\nbaseline: commit {} · {} · {} reps · {}",
        &base.recorded.commit[..8.min(base.recorded.commit.len())],
        base.recorded.date,
        base.recorded.reps,
        base.recorded.host.label
    );
    println!("{:<44} {:<9} detail", "metric", "status");
    println!("{}", "-".repeat(88));
    for c in &report.comparisons {
        let mark = match c.status {
            crate::bench::spec::Status::Ok => "ok",
            crate::bench::spec::Status::Warn => "WARN",
            crate::bench::spec::Status::Fail => "FAIL",
            crate::bench::spec::Status::Improved => "improved",
            crate::bench::spec::Status::Skipped => "skip",
        };
        let gate = match c.gate {
            crate::bench::spec::Gate::Fail => "",
            crate::bench::spec::Gate::Warn => " (warn-only)",
            crate::bench::spec::Gate::Off => " (off)",
        };
        println!("{:<44} {:<9} {}{}", c.key, mark, c.detail, gate);
    }
    let failed = report
        .comparisons
        .iter()
        .filter(|c| crate::bench::spec::Comparison::blocks(c))
        .count();
    let warned = report
        .comparisons
        .iter()
        .filter(|c| matches!(c.status, crate::bench::spec::Status::Warn))
        .count();
    let skipped = report.required_skipped().len();
    println!(
        "{}\nsummary: {failed} failing · {warned} warnings · {skipped} required-skipped",
        "-".repeat(88)
    );
}

// ── Time (dependency-free UTC) ───────────────────────────────────────────────

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD` for the current UTC day.
fn today_ymd() -> String {
    let (y, m, d, _, _, _) = civil_from_epoch(epoch_secs());
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` for now (UTC).
fn now_utc_iso() -> String {
    let (y, mo, d, h, mi, s) = civil_from_epoch(epoch_secs());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert a Unix timestamp to a civil UTC (Y, M, D, h, m, s) using Howard
/// Hinnant's `days_from_civil` inverse — no chrono dependency for a dev tool.
fn civil_from_epoch(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days since 1970-01-01 → civil date
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch_zero_is_1970() {
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn civil_known_timestamp() {
        // 2026-07-15T12:34:56Z = 1_784_118_896 (verified against the epoch-0
        // anchor + a whole-day step).
        assert_eq!(civil_from_epoch(1_784_118_896), (2026, 7, 15, 12, 34, 56));
        // One day later.
        assert_eq!(civil_from_epoch(1_784_205_296), (2026, 7, 16, 12, 34, 56));
    }

    #[test]
    fn iso_and_ymd_shapes() {
        let iso = now_utc_iso();
        assert_eq!(iso.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {iso}");
        assert!(iso.ends_with('Z'));
        assert_eq!(today_ymd().len(), 10);
    }

    #[test]
    fn workspace_version_parses() {
        // Uses whatever the repo currently stamps — just assert it's non-empty
        // and dotted when the file is present; skip if not in a checkout.
        if let Ok(root) = resolve_repo_root(None) {
            if root.join("rust/Cargo.toml").exists() {
                let v = workspace_version(&root).unwrap();
                assert!(v.contains('.'), "version looks wrong: {v}");
            }
        }
    }
}
