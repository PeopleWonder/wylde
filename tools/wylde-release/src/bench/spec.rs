//! The pure benchmark-comparison core — the noise/threshold engine.
//!
//! This is the hard part of the regression gate and it lives here, free of
//! any I/O or process spawning, so it is exhaustively unit-tested. Everything
//! that reads harness output, spawns `cargo`, or writes files is in
//! `super::harness`; everything that *decides pass/fail* is here.
//!
//! ## The design, honestly
//!
//! These benchmarks are **wall-clock measurements on a machine that is also
//! running Ollama on the GPU**. The S6 eval showed `think_harder` swinging
//! 22–38 s across seeds — roughly **±25 % noise** on a latency number. A gate
//! that fires on that noise gets muted and then ignored; a gate loose enough
//! never to false-fire catches nothing. So we do not pretend a single wall
//! number is precise:
//!
//! * **Median of N reps.** The harness runs each arm `reps` times; we baseline
//!   and compare the *median*, which is robust to the odd slow run (a GC pause,
//!   a background compile) in a way the mean is not.
//! * **Per-metric bands calibrated to each metric's real variance**, not one
//!   global threshold. A latency metric gets a *wide* fail band (it has to
//!   clear the ±25 % noise floor to mean anything); a deterministic retrieval
//!   invariant gets a *tight* one (any regression is real). The band lives in
//!   the baseline file next to the number, so the reviewer sees both.
//! * **Two tiers: WARN and FAIL.** Small regressions warn (surfaced, never
//!   block); only a large regression past the noise floor fails the gate. This
//!   is the "fail only on sustained/large regressions, warn on small ones"
//!   rule from the roadmap.
//! * **No fake statistics.** With 2–3 reps a t-test has no power and would lend
//!   false precision. A noise-calibrated percentage band is both more honest
//!   and more legible than a p-value nobody can sanity-check.
//!
//! **What this means for coverage (stated so it is never oversold):** the
//! sharp gates here are the *deterministic* ones — retrieval-quality invariants
//! and success rates. The *latency* gates are deliberately coarse: a sub-fail-
//! band latency regression is invisible by design, because the alternative is a
//! gate that cries wolf. Latency here catches *cliffs* (a 2× blowup), not
//! drift; drift is what the committed trend history is for.

use serde::{Deserialize, Serialize};

/// Whether a smaller or larger measured value is the *good* direction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Latency, token cost — down is good.
    LowerIsBetter,
    /// Success rate, recall — up is good.
    HigherIsBetter,
}

/// How hard a metric bites when it regresses.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// A large regression fails the whole preflight (non-zero exit).
    Fail,
    /// A regression is surfaced but never blocks — for the coarse/noisy
    /// signals we still want to watch but won't gate on.
    Warn,
    /// Recorded and trended, but neither warns nor fails.
    Off,
}

/// The comparison rule for a metric. `relative` (percentage band) suits
/// unbounded magnitudes like latency and tokens; `absolute` (delta band) suits
/// rates and invariants that live in a fixed \[0, 1] range where a *percentage*
/// swing is misleading (a 0.92 → 0.84 success drop is "8 points", not "9 %").
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "compare", rename_all = "snake_case")]
pub enum Compare {
    /// Percentage bands, relative to the baseline value.
    Relative { warn_pct: f64, fail_pct: f64 },
    /// Absolute-delta bands, in the metric's own units.
    Absolute { warn_delta: f64, fail_delta: f64 },
}

/// One baselined metric: the recorded value plus the policy for judging a new
/// measurement against it. Bands live *with* the number so `--accept-baseline`
/// updates only `value` and never silently loosens a threshold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricBaseline {
    /// The recorded baseline value (already a median over reps where relevant).
    pub value: f64,
    /// Human unit for display: `ms`, `tokens`, `rate`, `bool`.
    pub unit: String,
    pub direction: Direction,
    pub gate: Gate,
    #[serde(flatten)]
    pub compare: Compare,
    /// One-line note on what the metric is and why its band is what it is.
    #[serde(default)]
    pub note: String,
}

/// The verdict for a single metric after comparing a fresh measurement.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Within band — no regression worth mentioning.
    Ok,
    /// Regressed past the warn band but not the fail band. Never blocks.
    Warn,
    /// Regressed past the fail band. Blocks the preflight if `gate == Fail`.
    Fail,
    /// Improved past the *fail* band in the good direction — the baseline is
    /// now pessimistic and should be re-recorded (`--accept-baseline`).
    Improved,
    /// The benchmark that produces this metric could not run (service down,
    /// no index). Surfaced explicitly — never silently treated as a pass.
    Skipped,
}

impl Status {
    /// Short uppercase tag for the console/receipt.
    pub fn tag(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Improved => "IMPROVED",
            Status::Skipped => "SKIP",
        }
    }
}

/// The result of judging one metric.
///
/// `unit`/`regression`/`relative` are part of the structured comparison record
/// (consumed by the receipt/history serialisers and useful to future callers);
/// the console printer uses the pre-rendered `detail`, so a build that only
/// prints won't read them — hence the `allow`.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Comparison {
    /// Dotted metric key, e.g. `reasoning.fast.wall_ms_median`.
    pub key: String,
    pub baseline: f64,
    /// `None` when the producing benchmark was skipped.
    pub current: Option<f64>,
    pub unit: String,
    pub gate: Gate,
    pub status: Status,
    /// Signed "how much worse" for display: positive = regressed. For
    /// `Relative` this is a percentage; for `Absolute` it is a raw delta.
    pub regression: f64,
    pub relative: bool,
    /// Human-readable one-liner.
    pub detail: String,
}

impl Comparison {
    /// Does this comparison, given its gate, block the preflight?
    ///
    /// Only a `Fail`-gated metric that actually `Fail`ed blocks. A `Skipped`
    /// metric does **not** block here — skip handling is a preflight-level
    /// policy decision (see [`Report::is_green`]) so the operator can choose to
    /// allow a genuinely-unavailable benchmark, but never by accident.
    pub fn blocks(&self) -> bool {
        matches!(self.gate, Gate::Fail) && matches!(self.status, Status::Fail)
    }
}

/// Compare one fresh measurement (`current`) against a baseline. `None` current
/// means the benchmark was skipped.
pub fn compare_metric(key: &str, base: &MetricBaseline, current: Option<f64>) -> Comparison {
    let Some(cur) = current else {
        return Comparison {
            key: key.to_string(),
            baseline: base.value,
            current: None,
            unit: base.unit.clone(),
            gate: base.gate,
            status: Status::Skipped,
            regression: 0.0,
            relative: matches!(base.compare, Compare::Relative { .. }),
            detail: "benchmark did not run".to_string(),
        };
    };

    // "Signed delta in the WORSE direction." For lower-is-better a positive
    // raw delta (got bigger) is a regression; for higher-is-better a negative
    // raw delta (got smaller) is a regression. We fold direction in here so the
    // band logic below is direction-agnostic: positive `regression` always
    // means "worse".
    let raw_delta = cur - base.value; // + = value went up
    let worse_sign = match base.direction {
        Direction::LowerIsBetter => 1.0, // up is worse
        Direction::HigherIsBetter => -1.0, // down is worse
    };

    let (regression, relative, status) = match base.compare {
        Compare::Relative { warn_pct, fail_pct } => {
            // Percentage change off the baseline, signed so + = worse.
            // Guard a zero baseline (shouldn't happen for latency/tokens, but
            // never divide by zero): treat any change as a large regression.
            let pct = if base.value.abs() < f64::EPSILON {
                if raw_delta.abs() < f64::EPSILON {
                    0.0
                } else {
                    f64::INFINITY * raw_delta.signum()
                }
            } else {
                (raw_delta / base.value) * 100.0
            };
            let reg = pct * worse_sign;
            let status = classify(reg, warn_pct, fail_pct);
            (reg, true, status)
        }
        Compare::Absolute { warn_delta, fail_delta } => {
            let reg = raw_delta * worse_sign;
            let status = classify(reg, warn_delta, fail_delta);
            (reg, false, status)
        }
    };

    let detail = render_detail(base, cur, regression, relative, status);
    Comparison {
        key: key.to_string(),
        baseline: base.value,
        current: Some(cur),
        unit: base.unit.clone(),
        gate: base.gate,
        status,
        regression,
        relative,
        detail,
    }
}

/// Turn a signed "worse-ness" number into a status against a warn/fail band.
/// Positive = worse; a large *negative* (improved past the fail band) is
/// flagged so the baseline can be re-recorded.
fn classify(regression: f64, warn: f64, fail: f64) -> Status {
    if regression >= fail {
        Status::Fail
    } else if regression >= warn {
        Status::Warn
    } else if regression <= -fail {
        Status::Improved
    } else {
        Status::Ok
    }
}

fn render_detail(
    base: &MetricBaseline,
    cur: f64,
    regression: f64,
    relative: bool,
    status: Status,
) -> String {
    let arrow = match status {
        Status::Ok => "≈",
        Status::Warn | Status::Fail => "↑worse",
        Status::Improved => "↓better",
        Status::Skipped => "—",
    };
    if relative {
        format!(
            "{:.0}{} → {:.0}{} ({:+.1}% {})",
            base.value, base.unit, cur, base.unit, regression, arrow
        )
    } else {
        format!(
            "{:.3}{} → {:.3}{} ({:+.3} {})",
            base.value, base.unit, cur, base.unit, regression, arrow
        )
    }
}

/// A whole comparison run: every metric judged, plus the roll-up.
#[derive(Clone, Debug)]
pub struct Report {
    pub comparisons: Vec<Comparison>,
}

impl Report {
    pub fn new(comparisons: Vec<Comparison>) -> Self {
        Report { comparisons }
    }

    pub fn any_failed(&self) -> bool {
        self.comparisons.iter().any(Comparison::blocks)
    }

    /// Any soft (non-blocking) regression this run — used by the roll-up.
    #[allow(dead_code)] // exercised in unit tests; part of the report surface
    pub fn any_warned(&self) -> bool {
        self.comparisons.iter().any(|c| matches!(c.status, Status::Warn))
    }

    /// Any metric that improved past its fail band (baseline is now pessimistic).
    #[allow(dead_code)] // exercised in unit tests; part of the report surface
    pub fn any_improved(&self) -> bool {
        self.comparisons.iter().any(|c| matches!(c.status, Status::Improved))
    }

    /// Fail-gated metrics whose benchmark did not run. These are the dangerous
    /// case: a required signal we have *no* reading for. They do not "fail" the
    /// band logic, but a green light while blind to a required benchmark is
    /// exactly the false-confidence the gate exists to prevent.
    pub fn required_skipped(&self) -> Vec<&Comparison> {
        self.comparisons
            .iter()
            .filter(|c| matches!(c.gate, Gate::Fail) && matches!(c.status, Status::Skipped))
            .collect()
    }

    /// Is the run green? A run is green when no fail-gated metric failed **and**
    /// (unless `allow_skips`) no fail-gated metric was skipped. Warnings and
    /// improvements never turn it red.
    pub fn is_green(&self, allow_skips: bool) -> bool {
        if self.any_failed() {
            return false;
        }
        if !allow_skips && !self.required_skipped().is_empty() {
            return false;
        }
        true
    }
}

/// Median of a slice of f64, robust to the odd slow rep. Returns 0.0 for an
/// empty slice (callers guard emptiness before baselining).
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(value: f64, dir: Direction, gate: Gate, warn: f64, fail: f64) -> MetricBaseline {
        MetricBaseline {
            value,
            unit: "ms".into(),
            direction: dir,
            gate,
            compare: Compare::Relative { warn_pct: warn, fail_pct: fail },
            note: String::new(),
        }
    }

    fn abs(value: f64, dir: Direction, gate: Gate, warn: f64, fail: f64) -> MetricBaseline {
        MetricBaseline {
            value,
            unit: "rate".into(),
            direction: dir,
            gate,
            compare: Compare::Absolute { warn_delta: warn, fail_delta: fail },
            note: String::new(),
        }
    }

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
        // Robust to an outlier slow rep.
        assert_eq!(median(&[8.0, 8.5, 40.0]), 8.5);
    }

    #[test]
    fn latency_within_noise_is_ok() {
        // Baseline 8392 ms, warn 15 %, fail 35 %. A +10 % run is noise → OK.
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let c = compare_metric("lat", &b, Some(8392.0 * 1.10));
        assert_eq!(c.status, Status::Ok);
        assert!(!c.blocks());
    }

    #[test]
    fn latency_small_regression_warns_not_fails() {
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let c = compare_metric("lat", &b, Some(8392.0 * 1.20)); // +20 %
        assert_eq!(c.status, Status::Warn);
        assert!(!c.blocks()); // warns never block
    }

    #[test]
    fn latency_large_regression_fails_and_blocks() {
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let c = compare_metric("lat", &b, Some(8392.0 * 1.50)); // +50 %
        assert_eq!(c.status, Status::Fail);
        assert!(c.blocks());
    }

    #[test]
    fn latency_big_improvement_flags_rebaseline_but_never_blocks() {
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let c = compare_metric("lat", &b, Some(8392.0 * 0.5)); // 2× faster
        assert_eq!(c.status, Status::Improved);
        assert!(!c.blocks());
    }

    #[test]
    fn warn_gated_latency_never_blocks_even_on_fail() {
        // A Warn-gated metric that blows past the fail band still does not block.
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Warn, 15.0, 35.0);
        let c = compare_metric("lat", &b, Some(8392.0 * 3.0));
        assert_eq!(c.status, Status::Fail);
        assert!(!c.blocks());
    }

    #[test]
    fn success_rate_drop_is_absolute_points() {
        // Baseline 0.92, warn 0.01 pts, fail 0.10 pts, higher-is-better.
        let b = abs(0.92, Direction::HigherIsBetter, Gate::Fail, 0.01, 0.10);
        // One flaky task at n=12 ≈ 0.08 drop → warns, does not fail.
        let one_flake = compare_metric("succ", &b, Some(0.92 - 0.08));
        assert_eq!(one_flake.status, Status::Warn);
        // A two-task collapse (0.17) fails.
        let collapse = compare_metric("succ", &b, Some(0.92 - 0.17));
        assert_eq!(collapse.status, Status::Fail);
        assert!(collapse.blocks());
    }

    #[test]
    fn success_rate_improvement_flags_rebaseline() {
        let b = abs(0.83, Direction::HigherIsBetter, Gate::Fail, 0.01, 0.10);
        let c = compare_metric("succ", &b, Some(1.0)); // +0.17
        assert_eq!(c.status, Status::Improved);
    }

    #[test]
    fn invariant_modeled_as_absolute_boolean() {
        // fused≥dense invariant: baseline 1.0 (holds), higher-is-better, any
        // drop to 0.0 must fail. warn 0.5, fail 0.5.
        let b = abs(1.0, Direction::HigherIsBetter, Gate::Fail, 0.5, 0.5);
        let holds = compare_metric("inv", &b, Some(1.0));
        assert_eq!(holds.status, Status::Ok);
        let broke = compare_metric("inv", &b, Some(0.0));
        assert_eq!(broke.status, Status::Fail);
        assert!(broke.blocks());
    }

    #[test]
    fn tokens_up_is_worse_lower_is_better() {
        // Completion tokens: 1365 baseline, +25 % fails.
        let b = rel(1365.0, Direction::LowerIsBetter, Gate::Fail, 10.0, 25.0);
        let c = compare_metric("tok", &b, Some(1365.0 * 1.30));
        assert_eq!(c.status, Status::Fail);
    }

    #[test]
    fn skipped_metric_is_surfaced_not_passed() {
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let c = compare_metric("lat", &b, None);
        assert_eq!(c.status, Status::Skipped);
        assert!(!c.blocks());
    }

    #[test]
    fn report_green_logic() {
        let ok = rel(100.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let report = Report::new(vec![
            compare_metric("a", &ok, Some(105.0)), // OK
            compare_metric("b", &ok, Some(120.0)), // WARN
        ]);
        assert!(report.is_green(false));
        assert!(report.any_warned());
        assert!(!report.any_failed());

        // A failure turns it red.
        let red = Report::new(vec![compare_metric("a", &ok, Some(200.0))]);
        assert!(!red.is_green(false));
        assert!(red.any_failed());
    }

    #[test]
    fn required_skip_blocks_green_unless_allowed() {
        let fail_gated = rel(100.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let report = Report::new(vec![compare_metric("a", &fail_gated, None)]);
        assert_eq!(report.required_skipped().len(), 1);
        assert!(!report.is_green(false)); // blind to a required benchmark → not green
        assert!(report.is_green(true)); // …unless the operator explicitly allows it
    }

    #[test]
    fn warn_gated_skip_does_not_block_green() {
        // A Warn-gated benchmark that couldn't run is not a required signal, so
        // its skip does not turn the run red.
        let warn_gated = rel(100.0, Direction::LowerIsBetter, Gate::Warn, 15.0, 35.0);
        let report = Report::new(vec![compare_metric("a", &warn_gated, None)]);
        assert!(report.required_skipped().is_empty());
        assert!(report.is_green(false));
    }

    #[test]
    fn serde_round_trips_a_metric() {
        let b = rel(8392.0, Direction::LowerIsBetter, Gate::Fail, 15.0, 35.0);
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"compare\":\"relative\""));
        assert!(json.contains("\"lower_is_better\""));
        let back: MetricBaseline = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn serde_absolute_variant_tag() {
        let b = abs(0.92, Direction::HigherIsBetter, Gate::Fail, 0.01, 0.10);
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"compare\":\"absolute\""));
        let back: MetricBaseline = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
    }
}
