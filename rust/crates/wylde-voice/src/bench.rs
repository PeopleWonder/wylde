//! Encoder backend benchmark + shipped-default decision (Slice 5 —
//! `docs/plans/voice-rust-port.md`, "NPU default + DLL bundling decision").
//!
//! The Phase 10 NPU spike turned up a surprise
//! (`docs/wylde-voice-npu-spike-findings.md` §"What the spike did NOT
//! prove" + recommendation #5): for *whisper-tiny.en* on the Wylde user's
//! Arrow Lake NPU 3720 the **CPU EP is ~1.8× faster** than the NPU
//! (80 ms vs 143 ms p50), because NPU dispatch + per-cold-start compile
//! overhead dominates at that model size. The crossover to NPU is
//! *expected* at whisper-small (the configured model, ~6× the params)
//! but was never measured — the spike time-boxed itself to tiny.
//!
//! Slice 5 therefore ships **CPU as the proven default** and provides
//! this harness so the crossover can be measured on real hardware
//! (`wylde-voice-bench` binary) rather than guessed. [`recommend`] is the
//! decision rule the spike asked for ("runs encoder both ways … and
//! picks"); it stays pure so it's unit-testable without touching ONNX.
//!
//! Timing uses [`std::time::Instant`] — the same monotonic clock the
//! spike driver used. Nothing here loads model weights on its own; the
//! caller supplies an already-loaded [`WhisperEncoder`] and a mel buffer.

use std::time::Instant;

use crate::config::{Config, SttBackend};
use crate::transcribe::mel::{N_FRAMES, N_MELS};
use crate::transcribe::whisper::{WhisperEncoder, WhisperInferError};

/// How many passes the bench runs. `warmup` passes are discarded (they
/// pay one-time costs: NPU graph compile on a cold cache, CPU page-in,
/// allocator warmup). `iters` timed passes feed the summary.
#[derive(Debug, Clone, Copy)]
pub struct BenchConfig {
    pub warmup: usize,
    pub iters: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        // 2 warmups covers the NPU cold-compile + a cache-hit reload; 10
        // timed passes is enough to get a stable p50/p90 at the ~100 ms
        // latencies we're measuring without making first-launch slow.
        Self {
            warmup: 2,
            iters: 10,
        }
    }
}

/// Distribution summary of a set of latency samples (milliseconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub mean_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
}

/// One backend's full bench outcome: the one-time load cost plus the
/// steady-state inference distribution.
#[derive(Debug, Clone)]
pub struct BenchResult {
    pub backend: SttBackend,
    pub device: String,
    pub load_ms: f64,
    pub summary: Summary,
    pub iters: usize,
}

/// Linear-interpolated percentile over `samples` (any order; copied and
/// sorted internally). `pct` is in `[0, 100]`. Empty input → `0.0`.
pub fn percentile(samples: &[f64], pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.len() == 1 {
        return v[0];
    }
    let pct = pct.clamp(0.0, 100.0);
    // Rank on the [0, n-1] index space (the "linear interpolation between
    // closest ranks" / NIST type-7 estimator, matching numpy's default).
    let rank = pct / 100.0 * (v.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return v[lo];
    }
    let frac = rank - lo as f64;
    v[lo] + (v[hi] - v[lo]) * frac
}

/// Reduce raw latency samples to a [`Summary`]. Returns `None` on empty
/// input so callers don't have to special-case a zero-iteration bench.
pub fn summarize(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(Summary {
        p50_ms: percentile(samples, 50.0),
        p90_ms: percentile(samples, 90.0),
        mean_ms: mean,
        min_ms: min,
        max_ms: max,
    })
}

/// Run `cfg.warmup` discarded passes then `cfg.iters` timed passes of the
/// encoder against `mel`, returning the timing distribution. `load_ms` is
/// passed through from the caller's session-build timing so a single
/// [`BenchResult`] carries both the cold-load and steady-state costs.
///
/// `mel` must be exactly `N_MELS * N_FRAMES` long (the encoder validates
/// this and returns [`WhisperInferError`] otherwise).
pub fn bench_encoder(
    encoder: &WhisperEncoder,
    mel: &[f32],
    load_ms: f64,
    cfg: &BenchConfig,
) -> Result<BenchResult, WhisperInferError> {
    for _ in 0..cfg.warmup {
        encoder.run_encoder(mel)?;
    }
    let mut samples = Vec::with_capacity(cfg.iters);
    for _ in 0..cfg.iters {
        let t0 = Instant::now();
        encoder.run_encoder(mel)?;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    // `iters == 0` is a caller error, but degrade gracefully rather than
    // unwrap a None: report a zeroed summary so the bench still completes.
    let summary = summarize(&samples).unwrap_or(Summary {
        p50_ms: 0.0,
        p90_ms: 0.0,
        mean_ms: 0.0,
        min_ms: 0.0,
        max_ms: 0.0,
    });
    Ok(BenchResult {
        backend: encoder.backend(),
        device: encoder.device().to_owned(),
        load_ms,
        summary,
        iters: cfg.iters,
    })
}

/// A flat all-zeros mel buffer of the exact `[1, N_MELS, N_FRAMES]` shape
/// the encoder expects — the same zero-input the spike used to measure
/// pure compute latency without depending on a captured audio fixture.
pub fn zero_mel() -> Vec<f32> {
    vec![0.0; N_MELS * N_FRAMES]
}

/// The decision rule the spike asked for: given the CPU steady-state
/// summary and (optionally) the NPU one, pick the backend that should be
/// the shipped default and explain why. Pure — no I/O, no inference — so
/// the policy is unit-tested directly.
///
/// Rule: NPU wins only if it was actually measured *and* its p50 beats
/// CPU's p50 by a margin wider than run-to-run noise (10%). Otherwise CPU
/// stays the default — matching risk-register #2 ("Default to the proven
/// CPU path. Benchmark small before bundling any DLLs.").
pub fn recommend(cpu: &Summary, npu: Option<&Summary>) -> (SttBackend, String) {
    match npu {
        None => (
            SttBackend::Cpu,
            format!(
                "NPU not benchmarked (no openvino feature or no NPU encoder); \
                 keeping CPU default at p50 {:.1} ms",
                cpu.p50_ms
            ),
        ),
        Some(npu) => {
            // 10% guard band: an NPU edge inside noise isn't worth the
            // 23-DLL bundle + version-pin fragility.
            if npu.p50_ms < cpu.p50_ms * 0.90 {
                (
                    SttBackend::Npu,
                    format!(
                        "NPU faster: p50 {:.1} ms vs CPU {:.1} ms ({:.2}×) — \
                         enable the openvino build + DLL bundle",
                        npu.p50_ms,
                        cpu.p50_ms,
                        cpu.p50_ms / npu.p50_ms
                    ),
                )
            } else {
                (
                    SttBackend::Cpu,
                    format!(
                        "CPU at least as fast within noise: CPU p50 {:.1} ms vs \
                         NPU {:.1} ms — keep CPU default, NPU stays opt-in",
                        cpu.p50_ms, npu.p50_ms
                    ),
                )
            }
        }
    }
}

/// One-line description of the *shipped* inference default for the startup
/// log — Slice 5's "**`log()` the choice**" deliverable. Reports the
/// active backend, whether the openvino EP was compiled in, and the OV
/// device hint that would be used if NPU were selected.
pub fn describe_default(cfg: &Config, openvino_compiled: bool) -> String {
    let ep = if openvino_compiled {
        "openvino EP compiled in (NPU available at runtime if DLL bundle present)"
    } else {
        "CPU-only build (no openvino feature; rebuild with --features openvino for NPU)"
    };
    format!(
        "inference default: stt_backend={} model={} ov_device={} — {}",
        cfg.stt_backend.as_str(),
        cfg.stt_model,
        cfg.ov_device,
        ep
    )
}

/// `true` when the crate was compiled with the `openvino` cargo feature,
/// i.e. the OpenVINO EP register codepath is present in this binary.
/// Resolved at compile time so the startup log + bench reflect the actual
/// build, not a runtime guess.
pub const fn openvino_compiled() -> bool {
    cfg!(feature = "openvino")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summ(p50: f64) -> Summary {
        Summary {
            p50_ms: p50,
            p90_ms: p50 * 1.1,
            mean_ms: p50,
            min_ms: p50 * 0.9,
            max_ms: p50 * 1.2,
        }
    }

    #[test]
    fn percentile_basic() {
        let xs = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile(&xs, 0.0), 10.0);
        assert_eq!(percentile(&xs, 100.0), 50.0);
        assert_eq!(percentile(&xs, 50.0), 30.0);
    }

    #[test]
    fn percentile_interpolates_and_is_order_independent() {
        let xs = [50.0, 10.0, 40.0, 20.0, 30.0];
        // p25 over 0..4 index space → rank 1.0 → exactly the 2nd value.
        assert_eq!(percentile(&xs, 25.0), 20.0);
        // p75 → rank 3.0 → 4th value.
        assert_eq!(percentile(&xs, 75.0), 40.0);
    }

    #[test]
    fn percentile_edge_cases() {
        assert_eq!(percentile(&[], 50.0), 0.0);
        assert_eq!(percentile(&[42.0], 50.0), 42.0);
        assert_eq!(percentile(&[42.0], 99.0), 42.0);
    }

    #[test]
    fn summarize_empty_is_none() {
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn summarize_computes_stats() {
        let s = summarize(&[10.0, 20.0, 30.0]).unwrap();
        assert_eq!(s.min_ms, 10.0);
        assert_eq!(s.max_ms, 30.0);
        assert!((s.mean_ms - 20.0).abs() < 1e-9);
        assert_eq!(s.p50_ms, 20.0);
    }

    #[test]
    fn recommend_keeps_cpu_when_npu_absent() {
        let (backend, _why) = recommend(&summ(80.0), None);
        assert_eq!(backend, SttBackend::Cpu);
    }

    #[test]
    fn recommend_tiny_case_keeps_cpu() {
        // The spike's actual tiny.en numbers: CPU 80, NPU 143 → CPU wins.
        let (backend, why) = recommend(&summ(80.4), Some(&summ(143.0)));
        assert_eq!(backend, SttBackend::Cpu);
        assert!(why.contains("opt-in"));
    }

    #[test]
    fn recommend_flips_to_npu_when_clearly_faster() {
        // Hypothetical whisper-small crossover: NPU 90 vs CPU 140.
        let (backend, why) = recommend(&summ(140.0), Some(&summ(90.0)));
        assert_eq!(backend, SttBackend::Npu);
        assert!(why.contains("NPU faster"));
    }

    #[test]
    fn recommend_noise_band_keeps_cpu() {
        // NPU only 5% faster — inside the 10% guard band → not worth it.
        let (backend, _why) = recommend(&summ(100.0), Some(&summ(96.0)));
        assert_eq!(backend, SttBackend::Cpu);
    }

    #[test]
    fn zero_mel_is_correct_shape() {
        assert_eq!(zero_mel().len(), N_MELS * N_FRAMES);
    }

    #[test]
    fn describe_default_mentions_backend_and_ep() {
        let cfg = Config::get();
        let line = describe_default(cfg, false);
        assert!(line.contains("stt_backend=cpu"));
        assert!(line.contains("CPU-only build"));
        let line_ov = describe_default(cfg, true);
        assert!(line_ov.contains("openvino EP compiled in"));
    }
}
