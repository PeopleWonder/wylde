//! Conservative footprint estimates for first-seen models.
//!
//! Phase 0.5 lets callers omit a precise `bytes` value on `vram.reserve` —
//! when that happens the broker estimates rather than refusing. The estimate
//! is intentionally conservative so the grant path can still admit a sensible
//! lease while the Ollama poller's next tick swaps the synthetic lease in
//! with real numbers (`/api/ps` reports both `size_vram` and `size`).
//!
//! Lookup order (first match wins):
//!   1. The keep-warm [`model_cache`] (most accurate — derived from a prior
//!      real lease for the same (service, model)).
//!   2. A live synthetic lease for the model (Ollama poller observation).
//!   3. A name-heuristic table keyed off the standard `:NNb` suffix.
//!   4. The absolute defaults in [`Config`].

use crate::config::Config;
use crate::model_cache::model_cache;
use crate::registry::registry;

/// One gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;

/// VRAM and DRAM estimate, in bytes. `dram` is 0 unless the heuristic
/// expects spillover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub vram: u64,
    pub dram: u64,
}

/// Estimate the (vram, dram) footprint for `model`. `service` lets us look
/// up the keep-warm cache by the same (service, model) key the rest of the
/// broker uses. `hint` is the caller's claim about size — if it's > 0 the
/// caller knows what they want; we still produce an estimate for the DRAM
/// portion when the hint exceeds VRAM headroom (handled in `try_grant`).
pub fn estimate_for(service: &str, model: &str, hint: Option<i64>) -> Estimate {
    // Caller gave us a precise number — believe them. The grant path will
    // split this into vram/dram if it doesn't fit pure-VRAM.
    if let Some(h) = hint {
        if h > 0 {
            return Estimate {
                vram: h as u64,
                dram: 0,
            };
        }
    }

    // 1) Keep-warm cache.
    if let Some(bytes) = model_cache_bytes(service, model) {
        return Estimate {
            vram: bytes,
            dram: 0,
        };
    }

    // 2) Live synthetic lease for the same model name (Ollama poller).
    if let Some((vram, dram)) = synthetic_bytes_for(model) {
        return Estimate { vram, dram };
    }

    // 3) Name heuristic. These are *conservative starting points*: the
    //    Ollama poller refines them within seconds of the first real load
    //    via the synthetic-lease swap. Numbers chosen to err on the side of
    //    "request enough headroom to actually run" rather than "request the
    //    bare minimum and OOM".
    if let Some(bytes) = name_heuristic(model) {
        return Estimate {
            vram: bytes,
            dram: 0,
        };
    }

    // 4) Absolute defaults.
    let cfg = Config::get();
    Estimate {
        vram: cfg.estimate_default_vram,
        dram: cfg.estimate_default_dram,
    }
}

fn model_cache_bytes(service: &str, model: &str) -> Option<u64> {
    let cache = model_cache();
    if !cache.warm_for(service, model) {
        return None;
    }
    cache
        .all()
        .into_iter()
        .find(|e| e.service == service && e.model == model)
        .map(|e| e.bytes)
}

fn synthetic_bytes_for(model: &str) -> Option<(u64, u64)> {
    let needle = normalize_model_name(model);
    registry()
        .all_leases()
        .into_iter()
        .filter(|l| l.synthetic)
        .find(|l| normalize_model_name(&l.model) == needle)
        .map(|l| (l.bytes, l.dram_bytes))
}

/// Strip Ollama's `:tag` suffix and lowercase so `qwen2.5:7b` matches
/// `Qwen2.5:7B`. Matching by base name only would conflate `:7b` and `:14b`
/// — we keep the tag in the comparison.
fn normalize_model_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Name-heuristic table. The standard `<base>:<Nb>` Ollama suffix gives us
/// the parameter count; we map it to a conservative VRAM size assuming
/// roughly 4-bit quantisation (Ollama's default).
///
/// Refinement happens automatically: as soon as the model loads, the Ollama
/// poller writes a synthetic lease with the real `size_vram` / `size`, and
/// the next estimation call routes through the synthetic-lease branch above.
fn name_heuristic(model: &str) -> Option<u64> {
    let lower = model.to_ascii_lowercase();
    let suffix = lower.rsplit(':').next()?;
    // Strip a leading alphabetic prefix like "q4_" or "fp16-" then look for
    // a trailing parameter-count token like "7b" / "13b" / "70b".
    let token = suffix
        .split(['-', '_'])
        .next_back()
        .unwrap_or(suffix)
        .trim_end_matches('b');
    let n: f64 = token.parse().ok()?;
    // Q4 quantised: ~0.6 GB per billion parameters + ~1 GB activations/kv.
    let gib = (n * 0.6 + 1.0).clamp(1.0, 80.0);
    Some((gib * GIB as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Lease;
    use crate::test_lock::guard;

    fn reset() {
        registry().reset();
        model_cache().reset();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn precise_hint_wins() {
        let _g = guard().await;
        reset();
        let e = estimate_for("svc", "m", Some(7 * GIB as i64));
        assert_eq!(e.vram, 7 * GIB);
        assert_eq!(e.dram, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cache_hit_used() {
        let _g = guard().await;
        reset();
        model_cache().touch("svc", "qwen2.5:7b", 5 * GIB, 40);
        let e = estimate_for("svc", "qwen2.5:7b", None);
        assert_eq!(e.vram, 5 * GIB);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn synthetic_lease_used() {
        let _g = guard().await;
        reset();
        registry().add(Lease {
            lease_id: "ollama:x".into(),
            service: "ollama".into(),
            model: "llama3.1:8b".into(),
            bytes: 6 * GIB,
            dram_bytes: 0,
            priority: 100,
            granted_at: 0.0,
            expires_at: 1e9,
            heartbeat_at: 0.0,
            pid: 0,
            synthetic: true,
            estimated: false,
            client_nonce: "".into(),
        });
        // Note: cache lookup is by (service, model), so a synthetic lease
        // under "ollama" doesn't satisfy a request from a different service
        // unless the synthetic-lookup path is name-based. That path *is*
        // name-based — proves the synthetic lookup runs even for an unrelated
        // requesting service.
        let e = estimate_for("anything", "Llama3.1:8b", None);
        assert_eq!(e.vram, 6 * GIB);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn name_heuristic_used() {
        let _g = guard().await;
        reset();
        // "qwen2.5:7b" → roughly 7 * 0.6 + 1 = 5.2 GiB
        let e = estimate_for("svc", "qwen2.5:7b", None);
        let gib = e.vram as f64 / GIB as f64;
        assert!(
            (4.5..6.0).contains(&gib),
            "expected ~5.2 GiB for :7b, got {gib} GiB"
        );
        assert_eq!(e.dram, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_used_for_unknown_name() {
        let _g = guard().await;
        reset();
        let e = estimate_for("svc", "totally-novel-thing", None);
        let cfg = Config::get();
        assert_eq!(e.vram, cfg.estimate_default_vram);
        assert_eq!(e.dram, cfg.estimate_default_dram);
    }
}
