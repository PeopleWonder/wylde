//! Benchmark regression gate — orchestration, baseline document, and harness
//! drivers. The pure pass/fail engine is in [`spec`]; this module is the I/O
//! half: it runs the eval harnesses, extracts metrics from the machine-readable
//! output they already emit, loads/saves the committed baseline, and appends to
//! the trend history.
//!
//! ## What we baseline, and why (the opinionated set)
//!
//! Wylde has five eval harnesses (reasoning, index, lexical/BM25, concept-
//! routing, voice). We deliberately gate on a *small* set — the ones that are
//! meaningful, stable enough to threshold, and cheap enough to run every
//! preflight — and justify the exclusions in `benchmarks/README.md`:
//!
//! * **`reasoning_eval` (fast + think arms)** — the fast arm's median wall is
//!   the chat-turn latency users feel; its success rate guards the ReAct path.
//!   The think arm is the reasoning-tier guardrail (roadmap L5): it must not
//!   regress *success* or balloon *tokens*. Ollama-only, seeded, re-runnable.
//! * **`lexical_eval` (BM25 + RRF)** — the retrieval-quality guard for the
//!   RAG/concept-routing work. Its *relative invariants* (fused ≥ dense) are
//!   corpus-independent and get a hard gate; the absolute recall is corpus-
//!   dependent and only warns.
//!
//! Excluded from the gate (assessed, justified in the README): index build time
//! (Ollama-paced + corpus-size-dependent — coarse), concept-routing live eval
//! (post-0.2 surface), voice bench (hardware/ONNX-specific), GUI first-paint
//! (not headlessly measurable), build time (noisy, machine-dependent).

pub mod spec;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use spec::{compare_metric, Compare, Direction, Gate, MetricBaseline, Report};

/// Current baseline-file schema version. Bump on a breaking shape change.
pub const BASELINE_SCHEMA: u32 = 1;

/// Where the committed baseline lives (public Core repo, repo-relative). Public
/// on purpose: useful to contributors, and exposing perf characteristics of an
/// open-source local-first app is low-risk (see the README rationale).
pub const BASELINE_REL_PATH: &str = "benchmarks/baselines/wylde-benchmarks.json";

/// The environment a measurement was taken on — recorded so a baseline is only
/// ever compared like-for-like, and so the numbers mean something to a reader.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostEnv {
    /// Free-text rig label, e.g. "Aaron's dev rig".
    pub label: String,
    pub cpu: String,
    pub gpu: String,
    pub ram: String,
    pub os: String,
    /// The reasoner model + quant the reasoning arms ran against.
    pub model: String,
    /// Ollama server version, best-effort.
    #[serde(default)]
    pub ollama: String,
}

/// Provenance for a recorded baseline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Full commit SHA the baseline was recorded on.
    pub commit: String,
    /// `YYYY-MM-DD` the baseline was recorded (passed in — the tool has no clock
    /// dependency of its own beyond what the caller provides).
    pub date: String,
    /// Reps per arm the medians were taken over.
    pub reps: u32,
    pub host: HostEnv,
}

/// The committed baseline document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub schema: u32,
    pub recorded: Provenance,
    /// Metric key → recorded value + comparison policy. `BTreeMap` so the file
    /// serialises in a stable, diff-friendly order.
    pub metrics: BTreeMap<String, MetricBaseline>,
}

impl Baseline {
    pub fn load(path: &Path) -> Result<Baseline> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading baseline {}", path.display()))?;
        let b: Baseline = serde_json::from_str(&raw)
            .with_context(|| format!("parsing baseline {}", path.display()))?;
        if b.schema != BASELINE_SCHEMA {
            bail!(
                "baseline schema {} != supported {} ({})",
                b.schema,
                BASELINE_SCHEMA,
                path.display()
            );
        }
        Ok(b)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut json = serde_json::to_string_pretty(self)
            .context("serialising baseline")?;
        json.push('\n'); // trailing newline — friendlier diffs
        std::fs::write(path, json)
            .with_context(|| format!("writing baseline {}", path.display()))?;
        Ok(())
    }
}

/// A fresh benchmark run: which metrics were measured, and per-harness skip
/// reasons for the ones that were not.
#[derive(Clone, Debug, Default)]
pub struct Measurements {
    /// Metric key → measured value (already a median where relevant).
    pub values: BTreeMap<String, f64>,
    /// Human notes on any harness that was skipped, for the console + receipt.
    pub skips: Vec<String>,
}

impl Measurements {
    fn merge(&mut self, other: Measurements) {
        self.values.extend(other.values);
        self.skips.extend(other.skips);
    }
}

// ── The metric policy (thresholds live in code, and get written into the
// committed baseline so a reviewer sees them next to the numbers). ──────────

/// The full set of metrics the gate knows about, with each metric's comparison
/// policy. Values are placeholders here (`0.0`); [`record_baseline`] stamps the
/// measured value onto each. This is the single source of truth for *what* is
/// gated and *how hard* — edit a band here (or, once recorded, in the JSON) to
/// tune it.
pub fn default_specs() -> BTreeMap<String, MetricBaseline> {
    let mut m = BTreeMap::new();

    // — Reasoning: fast arm (the chat turn users feel). —
    m.insert(
        "reasoning.fast.wall_ms_median".into(),
        MetricBaseline {
            value: 0.0,
            unit: "ms".into(),
            direction: Direction::LowerIsBetter,
            // Fail-gated but a WIDE band: wall-clock on a shared GPU is ~±25 %
            // noisy, so only a cliff (>40 %) is real; smaller drift only warns.
            gate: Gate::Fail,
            compare: Compare::Relative { warn_pct: 20.0, fail_pct: 40.0 },
            note: "Fast-path chat turn latency — the number users feel. Wide \
                   band: GPU-shared wall-clock is ~±25% noisy, so this catches \
                   a latency cliff, not drift (drift → the trend history)."
                .into(),
        },
    );
    m.insert(
        "reasoning.fast.success_rate".into(),
        MetricBaseline {
            value: 0.0,
            unit: "rate".into(),
            direction: Direction::HigherIsBetter,
            gate: Gate::Fail,
            // Absolute points: one flaky task at n≈12 is ~0.08, so warn early
            // but only fail on a ≥0.10 (multi-task) collapse.
            compare: Compare::Absolute { warn_delta: 0.01, fail_delta: 0.10 },
            note: "Fast ReAct success rate. Absolute-point band; one model \
                   refusal flake (~0.08 at n=12) warns, a ≥0.10 collapse fails."
                .into(),
        },
    );

    // — Reasoning: think arm (the tier guardrail, roadmap L5). —
    m.insert(
        "reasoning.think.success_rate".into(),
        MetricBaseline {
            value: 0.0,
            unit: "rate".into(),
            direction: Direction::HigherIsBetter,
            gate: Gate::Fail,
            compare: Compare::Absolute { warn_delta: 0.01, fail_delta: 0.10 },
            note: "Planning-tier success — the S6 regression class. Must not \
                   drop below the fast control by construction."
                .into(),
        },
    );
    m.insert(
        "reasoning.think.completion_tokens_median".into(),
        MetricBaseline {
            value: 0.0,
            unit: "tok".into(),
            direction: Direction::LowerIsBetter,
            gate: Gate::Fail,
            // Tokens are far less noisy than wall-clock (grammar-constrained,
            // budget-capped) — a tighter band is honest here.
            compare: Compare::Relative { warn_pct: 15.0, fail_pct: 30.0 },
            note: "Planning-tier token cost. Tokens are much steadier than \
                   wall-clock, so a 30% jump means the tier genuinely got \
                   heavier, not noise."
                .into(),
        },
    );
    m.insert(
        "reasoning.think.wall_ms_median".into(),
        MetricBaseline {
            value: 0.0,
            unit: "ms".into(),
            direction: Direction::LowerIsBetter,
            // Warn-only: think-tier wall is the noisiest number we have
            // (deliberating tiers swung ±25% in S6). Watch it, don't gate on it.
            gate: Gate::Warn,
            compare: Compare::Relative { warn_pct: 25.0, fail_pct: 50.0 },
            note: "Planning-tier wall-clock. WARN-only: the noisiest metric we \
                   have; token-cost is the sharp cost gate instead."
                .into(),
        },
    );

    // — Retrieval: lexical/BM25 invariants (corpus-independent → sharp gate). —
    m.insert(
        "retrieval.lexical.fused_ge_dense".into(),
        MetricBaseline {
            value: 1.0,
            unit: "bool".into(),
            direction: Direction::HigherIsBetter,
            gate: Gate::Fail,
            // Boolean invariant: 1.0 holds, 0.0 broke. A 0.5 band makes any
            // break fail regardless of corpus.
            compare: Compare::Absolute { warn_delta: 0.5, fail_delta: 0.5 },
            note: "Invariant: fused (RRF) recall ≥ dense recall on the lexical \
                   class. Corpus-independent, so a hard gate — fusion must \
                   never lose exact-token recall."
                .into(),
        },
    );
    m.insert(
        "retrieval.semantic.fused_ge_dense".into(),
        MetricBaseline {
            value: 1.0,
            unit: "bool".into(),
            direction: Direction::HigherIsBetter,
            gate: Gate::Fail,
            compare: Compare::Absolute { warn_delta: 0.5, fail_delta: 0.5 },
            note: "Guardrail invariant: fusion must not hurt semantic-class \
                   recall vs dense. Corpus-independent hard gate."
                .into(),
        },
    );
    m.insert(
        "retrieval.lexical.fused_recall".into(),
        MetricBaseline {
            value: 0.0,
            unit: "recall".into(),
            direction: Direction::HigherIsBetter,
            // Warn-only: absolute recall depends on the live corpus, which
            // drifts as the index is rebuilt — a real drop should be noticed
            // but not block a release on a corpus change.
            gate: Gate::Warn,
            compare: Compare::Absolute { warn_delta: 0.05, fail_delta: 0.15 },
            note: "Absolute fused recall on the lexical class. WARN-only: \
                   corpus-dependent (drifts with the live index), so it tracks \
                   quality without gating on a legitimate corpus change."
                .into(),
        },
    );

    m
}

/// The metric keys that come from each harness — used to mark a whole harness's
/// metrics as skipped when it can't run.
fn reasoning_metric_keys() -> Vec<&'static str> {
    vec![
        "reasoning.fast.wall_ms_median",
        "reasoning.fast.success_rate",
        "reasoning.think.success_rate",
        "reasoning.think.completion_tokens_median",
        "reasoning.think.wall_ms_median",
    ]
}

fn lexical_metric_keys() -> Vec<&'static str> {
    vec![
        "retrieval.lexical.fused_ge_dense",
        "retrieval.semantic.fused_ge_dense",
        "retrieval.lexical.fused_recall",
    ]
}

// ── Running the harnesses ───────────────────────────────────────────────────

/// Options for a benchmark run.
pub struct RunOpts {
    /// Repo root (contains `rust/`, `benchmarks/`).
    pub repo_root: PathBuf,
    /// Reps per reasoning arm.
    pub reps: u32,
    /// Scratch dir for harness output (JSON the harnesses write).
    pub out_dir: PathBuf,
    /// Skip the reasoning eval (the slow one) — for a fast local dry run.
    pub skip_reasoning: bool,
    /// Skip the lexical eval.
    pub skip_lexical: bool,
    /// Reuse existing harness output in `out_dir` instead of re-running the
    /// (slow, live-Ollama) harness. Lets you re-tune thresholds or re-record a
    /// baseline from a run you already have, without paying the LLM cost again.
    pub reuse: bool,
}

/// Run all enabled harnesses and collect their metrics. Never hard-errors on a
/// harness that couldn't produce output (Ollama down, no index) — those become
/// *skips* recorded in [`Measurements::skips`], surfaced to the operator, and
/// (for a fail-gated metric) enough to keep the preflight from going green.
pub fn run_all(opts: &RunOpts) -> Result<Measurements> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("creating scratch {}", opts.out_dir.display()))?;
    let mut all = Measurements::default();

    if opts.skip_reasoning {
        all.skips.push("reasoning: skipped by --skip-reasoning".into());
    } else {
        all.merge(run_reasoning(opts));
    }

    if opts.skip_lexical {
        all.skips.push("lexical: skipped by --skip-lexical".into());
    } else {
        all.merge(run_lexical(opts));
    }

    Ok(all)
}

/// Run `reasoning_eval` (fast + think arms) and extract per-arm medians.
fn run_reasoning(opts: &RunOpts) -> Measurements {
    let rust_dir = opts.repo_root.join("rust");
    let json_path = opts.out_dir.join("reasoning-eval-results.json");

    // `None` = reused an existing run (no spawn); `Some(status)` = we ran it.
    let status: Option<std::io::Result<std::process::ExitStatus>> =
        if opts.reuse && json_path.exists() {
            println!("▶ reasoning_eval — reusing existing {}", json_path.display());
            None
        } else {
            // Remove any stale output so a failed run can't be read as success.
            let _ = std::fs::remove_file(&json_path);
            println!(
                "▶ reasoning_eval (arms=fast,think reps={}) — this drives live Ollama…",
                opts.reps
            );
            Some(
                Command::new("cargo")
                    .current_dir(&rust_dir)
                    .args([
                        "run",
                        "--release",
                        "--example",
                        "reasoning_eval",
                        "--",
                        "--arms",
                        "fast,think",
                        "--reps",
                        &opts.reps.to_string(),
                        "--out",
                        &opts.out_dir.to_string_lossy(),
                    ])
                    .status(),
            )
        };

    match parse_reasoning(&json_path) {
        Ok(m) => m,
        Err(e) => skipped(reasoning_metric_keys(), &format!("reasoning: {}", spawn_why(status, e))),
    }
}

/// Turn a spawn outcome + a parse error into a one-line skip reason.
fn spawn_why(
    status: Option<std::io::Result<std::process::ExitStatus>>,
    parse_err: anyhow::Error,
) -> String {
    match status {
        None => format!("reused output unparseable: {parse_err}"),
        Some(Ok(s)) if !s.success() => format!("harness exited {s}: {parse_err}"),
        Some(Ok(_)) => format!("no parseable output: {parse_err}"),
        Some(Err(spawn)) => format!("could not spawn cargo: {spawn}"),
    }
}

/// The subset of a `reasoning-eval-results.json` row we care about.
#[derive(Deserialize)]
struct EvalRow {
    arm: String,
    ok: bool,
    wall_ms: f64,
    completion_tokens: f64,
}

#[derive(Deserialize)]
struct EvalFile {
    #[serde(default)]
    rows: Vec<EvalRow>,
}

/// Parse the reasoning JSON into the fast/think metrics. Errors if the file is
/// missing/unparseable or an expected arm has no rows.
fn parse_reasoning(json_path: &Path) -> Result<Measurements> {
    let raw = std::fs::read_to_string(json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let file: EvalFile = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;
    if file.rows.is_empty() {
        bail!("no rows in {}", json_path.display());
    }

    let mut out = Measurements::default();
    for (arm, prefix) in [("fast", "reasoning.fast"), ("think", "reasoning.think")] {
        let rows: Vec<&EvalRow> = file.rows.iter().filter(|r| r.arm == arm).collect();
        if rows.is_empty() {
            bail!("arm '{arm}' produced no rows");
        }
        let n = rows.len() as f64;
        let successes = rows.iter().filter(|r| r.ok).count() as f64;
        let success_rate = successes / n;
        let wall_median = spec::median(&rows.iter().map(|r| r.wall_ms).collect::<Vec<_>>());
        let tok_median =
            spec::median(&rows.iter().map(|r| r.completion_tokens).collect::<Vec<_>>());

        out.values.insert(format!("{prefix}.success_rate"), success_rate);
        out.values.insert(format!("{prefix}.wall_ms_median"), wall_median);
        if arm == "think" {
            out.values
                .insert(format!("{prefix}.completion_tokens_median"), tok_median);
        }
    }
    Ok(out)
}

/// Run the lexical/BM25 eval and extract the retrieval invariants + absolute
/// recall. Asks the harness for a JSON sidecar via `WYLDE_EVAL_JSON`.
fn run_lexical(opts: &RunOpts) -> Measurements {
    let rust_dir = opts.repo_root.join("rust");
    let json_path = opts.out_dir.join("lexical-eval-summary.json");

    let status: Option<std::io::Result<std::process::ExitStatus>> =
        if opts.reuse && json_path.exists() {
            println!("▶ lexical_eval — reusing existing {}", json_path.display());
            None
        } else {
            let _ = std::fs::remove_file(&json_path);
            println!("▶ lexical_eval (BM25 + RRF) — reads the live index + Ollama embeds…");
            Some(
                Command::new("cargo")
                    .current_dir(&rust_dir)
                    .env("WYLDE_EVAL_JSON", &json_path)
                    // Keep the human report in scratch during a gate run; the
                    // JSON sidecar is all the gate needs.
                    .env(
                        "WYLDE_EVAL_OUTPUT",
                        opts.out_dir.join("lexical-bm25-eval-results.md"),
                    )
                    .args([
                        "test",
                        "-p",
                        "wylde-workspaces",
                        "--test",
                        "lexical_eval",
                        "--",
                        "--ignored",
                        "--nocapture",
                    ])
                    .status(),
            )
        };

    match parse_lexical(&json_path) {
        Ok(m) => m,
        Err(e) => skipped(lexical_metric_keys(), &format!("lexical: {}", spawn_why(status, e))),
    }
}

/// The JSON sidecar the lexical harness writes when `WYLDE_EVAL_JSON` is set.
#[derive(Deserialize)]
struct LexicalSummary {
    dense_lexical_recall: f64,
    fused_lexical_recall: f64,
    dense_semantic_recall: f64,
    fused_semantic_recall: f64,
}

fn parse_lexical(json_path: &Path) -> Result<Measurements> {
    let raw = std::fs::read_to_string(json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let s: LexicalSummary = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    // Degenerate-corpus guard. If *both* dense arms recall nothing, the live
    // index does not contain the gold set's files — a stale/empty index (the
    // roadmap-T0.5 move-stale index), not a real measurement. Baselining 0.0
    // here would bake in a permanent false pass, so we refuse and let the
    // metric register as SKIPPED with a clear reason.
    if s.dense_lexical_recall == 0.0 && s.dense_semantic_recall == 0.0 {
        bail!(
            "both dense arms recall 0 — the live index doesn't match the gold set \
             (stale/empty corpus; re-index the current tree, roadmap T0.5)"
        );
    }

    let mut out = Measurements::default();
    // The invariants the live harness itself asserts, lifted into gate metrics.
    let lex_ok = if s.fused_lexical_recall >= s.dense_lexical_recall { 1.0 } else { 0.0 };
    let sem_ok = if s.fused_semantic_recall + 0.001 >= s.dense_semantic_recall { 1.0 } else { 0.0 };
    out.values.insert("retrieval.lexical.fused_ge_dense".into(), lex_ok);
    out.values.insert("retrieval.semantic.fused_ge_dense".into(), sem_ok);
    out.values.insert("retrieval.lexical.fused_recall".into(), s.fused_lexical_recall);
    Ok(out)
}

/// Build a `Measurements` with no values and one skip note (all the harness's
/// metrics will therefore compare as `Skipped`).
fn skipped(_keys: Vec<&'static str>, reason: &str) -> Measurements {
    Measurements {
        values: BTreeMap::new(),
        skips: vec![reason.to_string()],
    }
}

// ── Comparison against a baseline ───────────────────────────────────────────

/// Compare a fresh run against a baseline, producing the full [`Report`].
pub fn compare(base: &Baseline, m: &Measurements) -> Report {
    let comparisons = base
        .metrics
        .iter()
        .map(|(key, mb)| compare_metric(key, mb, m.values.get(key).copied()))
        .collect();
    Report::new(comparisons)
}

/// Record (or re-record) a baseline from a fresh measurement. Preserves the
/// comparison bands from `prior` when present (so re-recording never silently
/// loosens a threshold — the values move, the policy does not), falling back to
/// [`default_specs`] for a first-time record. Only metrics that were actually
/// measured are written; a metric whose benchmark was skipped is left out so a
/// baseline is never seeded from a non-run.
pub fn record_baseline(
    prior: Option<&Baseline>,
    m: &Measurements,
    provenance: Provenance,
) -> Result<Baseline> {
    let specs = default_specs();
    let mut metrics = BTreeMap::new();
    for (key, spec) in &specs {
        let Some(value) = m.values.get(key).copied() else {
            continue; // not measured this run — don't seed a baseline from nothing
        };
        // Prefer the band already in the committed file (reviewer may have
        // tuned it); fall back to the code default.
        let mut mb = prior
            .and_then(|p| p.metrics.get(key).cloned())
            .unwrap_or_else(|| spec.clone());
        mb.value = value;
        metrics.insert(key.clone(), mb);
    }
    if metrics.is_empty() {
        bail!("nothing to baseline — every benchmark was skipped (services down?)");
    }
    Ok(Baseline {
        schema: BASELINE_SCHEMA,
        recorded: provenance,
        metrics,
    })
}

// ── Trend history (append-only, lives in the private planning repo) ──────────

/// One history record appended per run, for drift-over-time analysis.
#[derive(Serialize)]
pub struct HistoryRecord<'a> {
    pub timestamp: &'a str,
    pub commit: &'a str,
    pub green: bool,
    pub values: &'a BTreeMap<String, f64>,
}

/// Append a run to the JSONL trend history if the directory exists. The default
/// location is the private planning repo's `outputs/benchmarks/history.jsonl`
/// (junctioned into Core), so the trend is versioned + backed up but not
/// published. Silently no-ops if the directory isn't mounted.
pub fn append_history(path: &Path, rec: &HistoryRecord) -> Result<bool> {
    let Some(parent) = path.parent() else { return Ok(false) };
    if !parent.exists() {
        // Planning-repo junction not mounted — don't fail the gate over it.
        return Ok(false);
    }
    let line = serde_json::to_string(rec).context("serialising history record")?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening history {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("appending to {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measurements(pairs: &[(&str, f64)]) -> Measurements {
        let mut m = Measurements::default();
        for (k, v) in pairs {
            m.values.insert((*k).into(), *v);
        }
        m
    }

    fn provenance() -> Provenance {
        Provenance {
            commit: "abc123".into(),
            date: "2026-07-15".into(),
            reps: 2,
            host: HostEnv {
                label: "test".into(),
                cpu: "x".into(),
                gpu: "y".into(),
                ram: "z".into(),
                os: "w".into(),
                model: "m".into(),
                ollama: "0".into(),
            },
        }
    }

    #[test]
    fn default_specs_cover_both_harnesses() {
        let s = default_specs();
        for k in reasoning_metric_keys() {
            assert!(s.contains_key(k), "missing spec {k}");
        }
        for k in lexical_metric_keys() {
            assert!(s.contains_key(k), "missing spec {k}");
        }
    }

    #[test]
    fn record_then_compare_is_green() {
        let m = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base = record_baseline(None, &m, provenance()).unwrap();
        // Comparing the same numbers back is trivially green.
        let report = compare(&base, &m);
        assert!(report.is_green(false));
        assert!(!report.any_warned());
    }

    #[test]
    fn a_latency_cliff_fails_the_gate() {
        let m = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base = record_baseline(None, &m, provenance()).unwrap();
        // Fast path doubles → past the 40% fail band.
        let slow = measurements(&[("reasoning.fast.wall_ms_median", 16800.0)]);
        let mut merged = m.clone();
        merged.values.extend(slow.values);
        let report = compare(&base, &merged);
        assert!(report.any_failed());
        assert!(!report.is_green(false));
    }

    #[test]
    fn broken_retrieval_invariant_fails() {
        let m = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base = record_baseline(None, &m, provenance()).unwrap();
        let mut broke = m.clone();
        broke.values.insert("retrieval.lexical.fused_ge_dense".into(), 0.0);
        let report = compare(&base, &broke);
        assert!(report.any_failed());
    }

    #[test]
    fn skipped_reasoning_keeps_preflight_from_green() {
        let full = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base = record_baseline(None, &full, provenance()).unwrap();
        // A run missing the reasoning metrics (Ollama down).
        let partial = measurements(&[
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let report = compare(&base, &partial);
        assert!(!report.required_skipped().is_empty());
        assert!(!report.is_green(false));
        assert!(report.is_green(true)); // operator can allow it explicitly
    }

    #[test]
    fn record_refuses_when_everything_skipped() {
        let empty = Measurements::default();
        assert!(record_baseline(None, &empty, provenance()).is_err());
    }

    #[test]
    fn re_record_preserves_tuned_bands() {
        let m = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let mut base = record_baseline(None, &m, provenance()).unwrap();
        // Reviewer tightens the fast-latency band in the committed file.
        if let Some(mb) = base.metrics.get_mut("reasoning.fast.wall_ms_median") {
            mb.compare = Compare::Relative { warn_pct: 5.0, fail_pct: 10.0 };
        }
        // Re-record with new numbers; the tuned band must survive.
        let m2 = measurements(&[
            ("reasoning.fast.wall_ms_median", 9000.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base2 = record_baseline(Some(&base), &m2, provenance()).unwrap();
        assert_eq!(
            base2.metrics["reasoning.fast.wall_ms_median"].compare,
            Compare::Relative { warn_pct: 5.0, fail_pct: 10.0 }
        );
        assert_eq!(base2.metrics["reasoning.fast.wall_ms_median"].value, 9000.0);
    }

    #[test]
    fn baseline_json_round_trips() {
        let m = measurements(&[
            ("reasoning.fast.wall_ms_median", 8392.0),
            ("reasoning.fast.success_rate", 0.92),
            ("reasoning.think.success_rate", 1.0),
            ("reasoning.think.completion_tokens_median", 1365.0),
            ("reasoning.think.wall_ms_median", 31517.0),
            ("retrieval.lexical.fused_ge_dense", 1.0),
            ("retrieval.semantic.fused_ge_dense", 1.0),
            ("retrieval.lexical.fused_recall", 0.75),
        ]);
        let base = record_baseline(None, &m, provenance()).unwrap();
        let json = serde_json::to_string_pretty(&base).unwrap();
        let back: Baseline = serde_json::from_str(&json).unwrap();
        assert_eq!(back.metrics.len(), base.metrics.len());
        assert_eq!(back.recorded.commit, "abc123");
    }
}
