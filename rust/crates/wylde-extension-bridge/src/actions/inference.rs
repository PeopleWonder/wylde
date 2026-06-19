//! The bridge inference gate (`inference.embed` / `inference.chat`).
//!
//! Phase 2 of the WyldeStudy security boundary (see
//! `outputs/wylde-study-security-boundary.md` §1 +
//! `outputs/extension-inference-path.md`, Option B "BUILD-THEN-REWIRE").
//!
//! The directive: extensions reach inference **through the bridge**,
//! brokered. The brokered / resident-model-reuse / per-request-swap path
//! already exists one hop away on `wylde-ollama`'s pipe (`ollama.embed`
//! / `ollama.chat`), so these handlers are a **thin, policy-bearing
//! forwarder**, not new inference machinery — exactly analogous to how
//! `wylde-gateway` mediates egress. `wylde-ollama` needs no change: it
//! leases VRAM and reuses the keep-alive'd resident model itself.
//!
//! Each call passes three gates before it is forwarded:
//!
//! 1. **Capability** — only an extension that declares the inference
//!    capability in its manifest `capabilities[]` may call these
//!    verbs; otherwise `capability_denied`. This turns the bridge's
//!    previously-decorative `capabilities[]` field into a real gate.
//! 2. **Rate-limit** — a per-extension token bucket (configurable);
//!    over-limit is `inference_rate_limited` with a `retry_after_ms`
//!    detail.
//! 3. **Forward + audit** — forward to `wylde-ollama` over its pipe,
//!    then emit one structured audit line (extension, verb, model,
//!    size, elapsed, outcome). Audit is best-effort `tracing` and never
//!    fails the call.
//!
//! ## Trust caveat (stated once)
//!
//! The Wylde IPC action layer hands handlers only the **payload** — the
//! handshake-authenticated caller is not delivered. So the `extension`
//! identity here is **self-asserted**, the same soft model egress uses
//! on the loopback first-party pipe: adequate against bugs/misconfig,
//! not against a malicious in-process extension that lies about its
//! name. The robust upgrade (thread the authenticated caller into a
//! `CallContext`) is a separate platform track (security design §5.3).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wylde_shared::ipc::{self, IpcError, Reply};

use crate::host::Host;
use std::sync::Arc;

/// The pipe service that actually performs (brokered) inference.
const OLLAMA_SERVICE: &str = "wylde-ollama";

/// Canonical capability token an extension declares in `capabilities[]`
/// to be allowed onto the inference gate. The legacy short form
/// `inference` is also accepted so the WyldeStudy rewire (Phase 3) can
/// declare either; the security design references both spellings.
const INFERENCE_CAP: &str = "inference.local";
const INFERENCE_CAP_LEGACY: &str = "inference";

/// Which inference verb a call is for. Keeps the embed/chat handlers a
/// single shared body that only branches on the few real differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Embed,
    Chat,
}

impl Verb {
    /// The `wylde-ollama` action this verb forwards to.
    fn ollama_action(self) -> &'static str {
        match self {
            Verb::Embed => "ollama.embed",
            Verb::Chat => "ollama.chat",
        }
    }

    /// Short label for audit lines.
    fn label(self) -> &'static str {
        match self {
            Verb::Embed => "inference.embed",
            Verb::Chat => "inference.chat",
        }
    }

    /// Env var holding the default model when the caller omits `model`.
    fn default_model_env(self) -> &'static str {
        match self {
            Verb::Embed => "WYLDE_BRIDGE_INFERENCE_EMBED_MODEL",
            Verb::Chat => "WYLDE_BRIDGE_INFERENCE_CHAT_MODEL",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Public handlers (registered in service.rs)
// ─────────────────────────────────────────────────────────────────────

pub async fn handle_inference_embed(host: Arc<Host>, payload: Value) -> Reply {
    handle_inference(host, payload, Verb::Embed).await
}

pub async fn handle_inference_chat(host: Arc<Host>, payload: Value) -> Reply {
    handle_inference(host, payload, Verb::Chat).await
}

// ─────────────────────────────────────────────────────────────────────
// Shared gate + forward
// ─────────────────────────────────────────────────────────────────────

async fn handle_inference(host: Arc<Host>, payload: Value, verb: Verb) -> Reply {
    let started = Instant::now();

    let Some(obj) = payload.as_object() else {
        return Reply::err(IpcError::new("bad_request", "payload must be a map"));
    };
    let extension = obj
        .get("extension")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if extension.is_empty() {
        return Reply::err(IpcError::new(
            "bad_request",
            "`extension` (string) is required so the bridge can authorize the inference call",
        ));
    }

    // ── Gate 1: capability ──────────────────────────────────────────
    let allowed = host
        .extension_has_capability(&extension, INFERENCE_CAP)
        .await
        || host
            .extension_has_capability(&extension, INFERENCE_CAP_LEGACY)
            .await;
    if !allowed {
        audit(
            &extension,
            verb,
            "-",
            0,
            started,
            "denied",
            "capability_denied",
        );
        return Reply::err(IpcError::new(
            "capability_denied",
            format!(
                "extension `{extension}` does not declare the `{INFERENCE_CAP}` capability; \
                 add it to the extension's mcp-server.json `capabilities[]` to use {}",
                verb.label()
            ),
        ));
    }

    // ── Gate 2: rate-limit ──────────────────────────────────────────
    if let Err(retry) = check_rate_limit(&extension) {
        let retry_after_ms = retry.as_millis() as u64;
        let mut err = IpcError::new(
            "inference_rate_limited",
            format!(
                "extension `{extension}` exceeded the inference rate limit; \
                 retry in {retry_after_ms} ms"
            ),
        );
        err.details = Some(json!({ "retry_after_ms": retry_after_ms }));
        audit(
            &extension,
            verb,
            "-",
            0,
            started,
            "denied",
            "inference_rate_limited",
        );
        return Reply::err(err);
    }

    // ── Shape validation (clean bridge-level error before forwarding) ─
    if let Err(e) = validate_shape(obj, verb) {
        audit(&extension, verb, "-", 0, started, "denied", "bad_request");
        return Reply::err(e);
    }

    // ── Build the forwarded body ────────────────────────────────────
    // Strip the bridge-only `extension` selector; default the model from
    // env if the caller omitted it (so the gate is transparent to model
    // selection / in-window swap — the caller's explicit `model` always
    // passes through verbatim and Ollama keep-alive + the broker lease
    // handle resident reuse / swap).
    let mut body = payload.clone();
    if let Some(m) = body.as_object_mut() {
        m.remove("extension");
    }
    let model = resolve_model(&body, verb);
    if body.get("model").is_none() {
        if let Some(m) = &model {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("model".into(), json!(m));
            }
        }
    }
    let model_label = model.clone().unwrap_or_else(|| "(unspecified)".to_owned());
    let n = count_units(obj, verb);

    // ── Gate 3: forward + audit ─────────────────────────────────────
    match ipc::call_action(OLLAMA_SERVICE, verb.ollama_action(), body).await {
        Ok(mut v) => {
            // Embed responses from ollama are `{embeddings}` only; carry
            // the resolved model back so the documented shape is
            // `{embeddings, model}`. Chat already echoes `model`.
            if verb == Verb::Embed && v.get("model").is_none() {
                if let (Some(obj), Some(m)) = (v.as_object_mut(), &model) {
                    obj.insert("model".into(), json!(m));
                }
            }
            audit(&extension, verb, &model_label, n, started, "ok", "ok");
            Reply::ok(v)
        }
        Err(e) => {
            // Preserve the upstream error code/message so callers keep
            // their existing classification (e.g. model_not_found,
            // ollama_unreachable, vram_admission_denied).
            audit(&extension, verb, &model_label, n, started, "error", &e.code);
            Reply::err(e)
        }
    }
}

/// Light request-shape validation so a bad request fails at the gate
/// with a clear `bad_request`, rather than being forwarded as garbage.
/// (`wylde-ollama` also validates — this is the boundary being explicit.)
fn validate_shape(obj: &serde_json::Map<String, Value>, verb: Verb) -> Result<(), IpcError> {
    match verb {
        Verb::Embed => match obj.get("input") {
            Some(v) if v.is_string() || v.is_array() => Ok(()),
            _ => Err(IpcError::new(
                "bad_request",
                "`input` (string or array of strings) is required for inference.embed",
            )),
        },
        Verb::Chat => match obj.get("messages") {
            Some(v) if v.is_array() => Ok(()),
            _ => Err(IpcError::new(
                "bad_request",
                "`messages` (array) is required for inference.chat",
            )),
        },
    }
}

/// Resolve the effective model: the caller's explicit `model` wins;
/// otherwise the per-verb env default if set. `None` means "no model" —
/// forwarded as-is, and `wylde-ollama` will reject with `invalid_request`
/// (transparent, no guessing a wrong model).
fn resolve_model(body: &Value, verb: Verb) -> Option<String> {
    if let Some(m) = body.get("model").and_then(Value::as_str) {
        if !m.is_empty() {
            return Some(m.to_owned());
        }
    }
    std::env::var(verb.default_model_env())
        .ok()
        .filter(|s| !s.is_empty())
}

/// Count the audit-relevant unit (# embed inputs / # chat messages).
fn count_units(obj: &serde_json::Map<String, Value>, verb: Verb) -> usize {
    match verb {
        Verb::Embed => match obj.get("input") {
            Some(Value::Array(a)) => a.len(),
            Some(Value::String(_)) => 1,
            _ => 0,
        },
        Verb::Chat => obj
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0),
    }
}

/// One structured audit line per inference call. Best-effort: `tracing`
/// never fails, so auditing can never fail the call (egress's rule).
#[allow(clippy::too_many_arguments)]
fn audit(
    extension: &str,
    verb: Verb,
    model: &str,
    n: usize,
    started: Instant,
    status: &str,
    code: &str,
) {
    let dur_ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        target: "wylde_extension_bridge::audit::inference",
        extension = extension,
        verb = verb.label(),
        model = model,
        units = n,
        dur_ms = dur_ms,
        status = status,
        code = code,
        "inference gate"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Rate limiter — per-extension token bucket
// ─────────────────────────────────────────────────────────────────────

/// One extension's bucket. `tokens` accrue at `refill_per_sec` up to
/// `capacity`; each call costs one token.
#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

/// Per-extension token-bucket rate limiter. Buckets are created lazily,
/// pre-filled to capacity so a first call is never throttled.
#[derive(Debug)]
struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: HashMap<String, TokenBucket>,
}

impl RateLimiter {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            buckets: HashMap::new(),
        }
    }

    /// Read the configured limits once from env.
    ///
    /// * `WYLDE_BRIDGE_INFERENCE_BURST`   — bucket capacity (default 30).
    /// * `WYLDE_BRIDGE_INFERENCE_RPS`     — sustained refill, tokens/sec
    ///   (default 5.0). 30 burst at 5/s is a sane default for a study
    ///   extension that batch-embeds a corpus then idles.
    fn from_env() -> Self {
        let capacity = env_f64("WYLDE_BRIDGE_INFERENCE_BURST", 30.0).max(0.0);
        let refill = env_f64("WYLDE_BRIDGE_INFERENCE_RPS", 5.0).max(0.0);
        Self::new(capacity, refill)
    }

    /// Try to spend one token for `ext` as of `now`. On success returns
    /// `Ok(())`; on exhaustion returns `Err(retry_after)` — the wait
    /// until one token has accrued (capped at 1h, and finite even when
    /// the refill rate is 0).
    fn try_acquire(&mut self, ext: &str, now: Instant) -> Result<(), Duration> {
        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let bucket = self.buckets.entry(ext.to_owned()).or_insert(TokenBucket {
            tokens: capacity,
            last: now,
        });
        // Refill for elapsed time, clamped to capacity.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill).min(capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let needed = 1.0 - bucket.tokens;
            let secs = if refill > 0.0 {
                needed / refill
            } else {
                f64::INFINITY
            };
            // .min(3600) keeps INFINITY finite so from_secs_f64 can't panic.
            Err(Duration::from_secs_f64(secs.min(3600.0)))
        }
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

fn rate_limiter() -> &'static Mutex<RateLimiter> {
    static LIMITER: OnceLock<Mutex<RateLimiter>> = OnceLock::new();
    LIMITER.get_or_init(|| Mutex::new(RateLimiter::from_env()))
}

/// Spend one token for `ext` now. `Err(retry_after)` ⇒ throttled.
fn check_rate_limit(ext: &str) -> Result<(), Duration> {
    let mut l = rate_limiter().lock().unwrap_or_else(|p| p.into_inner());
    l.try_acquire(ext, Instant::now())
}

/// Test-only: replace the process-wide limiter with explicit limits so
/// gate-ordering tests are deterministic and isolated from env.
#[cfg(test)]
fn set_rate_limiter_for_test(capacity: f64, refill_per_sec: f64) {
    let mut l = rate_limiter().lock().unwrap_or_else(|p| p.into_inner());
    *l = RateLimiter::new(capacity, refill_per_sec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::host::Host;

    /// Serialize the handler tests: they mutate the process-wide rate
    /// limiter via `set_rate_limiter_for_test`, so they must not run
    /// concurrently or they'd stomp each other's configured limits.
    async fn gate_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    // ── pure rate-limiter logic ─────────────────────────────────────

    #[test]
    fn bucket_allows_up_to_capacity_then_denies() {
        let mut rl = RateLimiter::new(3.0, 0.0); // no refill
        let t0 = Instant::now();
        assert!(rl.try_acquire("x", t0).is_ok());
        assert!(rl.try_acquire("x", t0).is_ok());
        assert!(rl.try_acquire("x", t0).is_ok());
        // 4th call exhausts the burst.
        assert!(rl.try_acquire("x", t0).is_err());
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut rl = RateLimiter::new(1.0, 1.0); // 1 token, +1/sec
        let t0 = Instant::now();
        assert!(rl.try_acquire("x", t0).is_ok());
        assert!(rl.try_acquire("x", t0).is_err());
        // After ~1.1s a token has accrued.
        let t1 = t0 + Duration::from_millis(1100);
        assert!(rl.try_acquire("x", t1).is_ok());
    }

    #[test]
    fn buckets_are_per_extension() {
        let mut rl = RateLimiter::new(1.0, 0.0);
        let t0 = Instant::now();
        assert!(rl.try_acquire("a", t0).is_ok());
        // `a` is now empty, but `b` has its own full bucket.
        assert!(rl.try_acquire("a", t0).is_err());
        assert!(rl.try_acquire("b", t0).is_ok());
    }

    #[test]
    fn retry_after_is_finite_with_zero_refill() {
        let mut rl = RateLimiter::new(0.0, 0.0);
        let t0 = Instant::now();
        let err = rl.try_acquire("x", t0).unwrap_err();
        assert!(err <= Duration::from_secs(3600));
    }

    // ── model resolution ────────────────────────────────────────────

    #[test]
    fn resolve_model_prefers_explicit() {
        let body = json!({"model": "explicit-model", "input": ["x"]});
        assert_eq!(
            resolve_model(&body, Verb::Embed).as_deref(),
            Some("explicit-model")
        );
    }

    #[test]
    fn resolve_model_none_when_absent_and_no_env() {
        // Ensure no env default leaks in.
        std::env::remove_var("WYLDE_BRIDGE_INFERENCE_CHAT_MODEL");
        let body = json!({"messages": []});
        assert_eq!(resolve_model(&body, Verb::Chat), None);
    }

    #[test]
    fn count_units_counts_inputs_and_messages() {
        let embed = json!({"input": ["a", "b", "c"]});
        assert_eq!(count_units(embed.as_object().unwrap(), Verb::Embed), 3);
        let embed_str = json!({"input": "solo"});
        assert_eq!(count_units(embed_str.as_object().unwrap(), Verb::Embed), 1);
        let chat = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(count_units(chat.as_object().unwrap(), Verb::Chat), 1);
    }

    // ── shape validation ────────────────────────────────────────────

    #[test]
    fn validate_shape_requires_input_for_embed() {
        let bad = json!({"model": "m"});
        assert!(validate_shape(bad.as_object().unwrap(), Verb::Embed).is_err());
        let good = json!({"input": ["x"]});
        assert!(validate_shape(good.as_object().unwrap(), Verb::Embed).is_ok());
    }

    #[test]
    fn validate_shape_requires_messages_for_chat() {
        let bad = json!({"model": "m"});
        assert!(validate_shape(bad.as_object().unwrap(), Verb::Chat).is_err());
        let good = json!({"messages": []});
        assert!(validate_shape(good.as_object().unwrap(), Verb::Chat).is_ok());
    }

    // ── handler gate behaviour (capability + rate-limit), no forward ─

    #[tokio::test]
    async fn embed_denied_without_capability() {
        let _g = gate_guard().await;
        set_rate_limiter_for_test(100.0, 100.0);
        let host = Arc::new(Host::new(Config::get()));
        host.seed_capabilities_for_tests("NoInfer", vec!["egress.web".into()])
            .await;
        let r = handle_inference_embed(
            host,
            json!({"extension": "NoInfer", "model": "m", "input": ["x"]}),
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "capability_denied");
    }

    #[tokio::test]
    async fn embed_denied_for_unknown_extension() {
        let _g = gate_guard().await;
        set_rate_limiter_for_test(100.0, 100.0);
        let host = Arc::new(Host::new(Config::get()));
        let r = handle_inference_embed(
            host,
            json!({"extension": "Ghost", "model": "m", "input": ["x"]}),
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "capability_denied");
    }

    #[tokio::test]
    async fn embed_requires_extension_field() {
        let _g = gate_guard().await;
        set_rate_limiter_for_test(100.0, 100.0);
        let host = Arc::new(Host::new(Config::get()));
        let r = handle_inference_embed(host, json!({"model": "m", "input": ["x"]})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn capability_pass_then_rate_limit_trips_before_forward() {
        // Capacity 0 + no refill ⇒ a capable extension is immediately
        // throttled, proving the gate ordering (capability OK → rate
        // limited) without ever touching wylde-ollama.
        let _g = gate_guard().await;
        set_rate_limiter_for_test(0.0, 0.0);
        let host = Arc::new(Host::new(Config::get()));
        host.seed_capabilities_for_tests("Infer", vec!["inference.local".into()])
            .await;
        let r = handle_inference_embed(
            host,
            json!({"extension": "Infer", "model": "m", "input": ["x"]}),
        )
        .await;
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "inference_rate_limited");
        let retry = err
            .details
            .as_ref()
            .and_then(|d| d.get("retry_after_ms"))
            .and_then(Value::as_u64);
        assert!(retry.is_some(), "retry_after_ms detail must be present");
    }

    #[tokio::test]
    async fn legacy_inference_capability_is_accepted() {
        // The short `inference` token also opens the gate. Rate limit is
        // generous; the call passes capability + rate gates and proceeds
        // to forward (which fails because no wylde-ollama pipe exists in
        // the test) — the point is it is NOT capability_denied.
        let _g = gate_guard().await;
        set_rate_limiter_for_test(100.0, 100.0);
        let host = Arc::new(Host::new(Config::get()));
        host.seed_capabilities_for_tests("LegacyInfer", vec!["inference".into()])
            .await;
        let r = handle_inference_embed(
            host,
            json!({"extension": "LegacyInfer", "model": "m", "input": ["x"]}),
        )
        .await;
        assert!(!r.ok);
        let code = r.error.unwrap().code;
        assert_ne!(code, "capability_denied");
        assert_ne!(code, "inference_rate_limited");
        assert_ne!(code, "bad_request");
    }

    #[tokio::test]
    async fn shape_validation_runs_after_gates() {
        // Capable + within rate limit, but missing `input` ⇒ bad_request
        // (not forwarded).
        let _g = gate_guard().await;
        set_rate_limiter_for_test(100.0, 100.0);
        let host = Arc::new(Host::new(Config::get()));
        host.seed_capabilities_for_tests("Infer2", vec!["inference.local".into()])
            .await;
        let r = handle_inference_embed(host, json!({"extension": "Infer2", "model": "m"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }
}
