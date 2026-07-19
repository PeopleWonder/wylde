//! Reclaim superseded model slots — wire the keep-only-referenced GC to
//! the reference-changing seam (0.2 stability finding E, issue #100).
//!
//! ## Why here
//!
//! The set of models Wylde references is fully derivable from
//! [`ReasoningConfig`]: the slot tags `{reasoner, fast}` plus the
//! effective embedder ([`crate::memory::common::embed_model`], env
//! override aware). When `settings.reasoning.set` commits a new config,
//! any tag the old config referenced that the new one does not has just
//! been **superseded** — the exact structural signal that a model is now
//! reclaim-eligible. Computing `referenced(prev) − referenced(next)` at
//! the commit is the whole mechanism: no per-model cleanup list, and a
//! future slot kind is covered the moment [`referenced_models`] includes
//! it. This mirrors [`residency::spawn_warm_slots`](super::residency),
//! which already hangs off the same commit to warm the *new* slots — we
//! reclaim the *old* ones on the same beat.
//!
//! ## Safety policy (this deletes user disk data)
//!
//! Conservative by construction and by default:
//!
//! * **Superseded-only.** We pass the GC the exact `superseded` set, so it
//!   only ever considers tags this commit dereferenced. A model the user
//!   pulled by hand was never a slot value, so it is never a candidate.
//! * **Announce, don't delete, by default.** `dry_run` is true unless
//!   `WYLDE_OLLAMA_RECLAIM_SUPERSEDED` is explicitly enabled. Out of the
//!   box a slot change *logs* the superseded model and its size and
//!   deletes nothing — the operator opts in to actual reclaim. This is
//!   Aaron's consent-gate ethos: a self-hosted app never silently deletes
//!   a model the user pulled.
//! * **Pins always win.** `WYLDE_OLLAMA_GC_PINS` (comma-separated tags)
//!   are passed as protected; the GC engine keeps referenced ∪ pinned out
//!   of the reclaim set unconditionally.
//!
//! The fuller "sweep every unreferenced model" policy is left to an
//! explicit operator-driven `ollama.gc` call (no `superseded` field) and
//! is deliberately NOT auto-wired here — flagged for Aaron.

use std::collections::BTreeSet;

use serde_json::json;
use wylde_shared::ipc::{self, IpcError};

use super::config::ReasoningConfig;

/// Env flag that opts in to *performing* the reclaim (vs. announce-only).
const RECLAIM_OPT_IN_ENV: &str = "WYLDE_OLLAMA_RECLAIM_SUPERSEDED";
/// Env list (comma-separated) of tags the user has pinned — always
/// protected from GC regardless of reference state.
const PINS_ENV: &str = "WYLDE_OLLAMA_GC_PINS";

/// The config-derivable referenced set: the reasoner and fast slot tags
/// plus the effective embedder (env override wins, per S2's
/// one-definition-of-the-embedder rule). Empty tags are skipped. These
/// are the models Wylde is actively configured to use — the protected set
/// a GC must never reclaim.
pub fn referenced_models(cfg: &ReasoningConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tag in [
        cfg.slots.reasoner.trim(),
        cfg.slots.fast.trim(),
        crate::memory::common::embed_model().trim(),
    ] {
        if !tag.is_empty() {
            out.insert(tag.to_owned());
        }
    }
    out
}

/// Tags the transition `prev → next` dereferenced: referenced by the old
/// config but not the new one. This is the superseded set — the ONLY
/// models a slot-change GC pass will consider.
pub fn superseded_models(prev: &ReasoningConfig, next: &ReasoningConfig) -> BTreeSet<String> {
    let before = referenced_models(prev);
    let after = referenced_models(next);
    before.difference(&after).cloned().collect()
}

/// Parse the pins env list into a set (comma-separated, trimmed, empties
/// dropped).
fn pins_from_env() -> BTreeSet<String> {
    std::env::var(PINS_ENV)
        .ok()
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether actual deletion is opted in (else announce-only / dry-run).
fn reclaim_opted_in() -> bool {
    matches!(
        std::env::var(RECLAIM_OPT_IN_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Issue the GC call through `call`, superseded-mode. Generic over the
/// transport so the seam logic is unit-testable without a pipe. Returns
/// the GC reply value on success (for logging/inspection), or `None` when
/// there is nothing superseded (the call is skipped entirely).
pub async fn reclaim_superseded_via<F, Fut>(
    call: F,
    keep: &BTreeSet<String>,
    pins: &BTreeSet<String>,
    superseded: &BTreeSet<String>,
    dry_run: bool,
) -> Option<Result<serde_json::Value, IpcError>>
where
    F: Fn(serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, IpcError>>,
{
    if superseded.is_empty() {
        // Nothing was dereferenced — no GC to run.
        return None;
    }
    let payload = json!({
        "keep": keep.iter().collect::<Vec<_>>(),
        "pins": pins.iter().collect::<Vec<_>>(),
        "superseded": superseded.iter().collect::<Vec<_>>(),
        "dry_run": dry_run,
    });
    Some(call(payload).await)
}

/// Production reclaim pass: compute the superseded set for `prev → next`
/// and call `ollama.gc`. Announce-only unless opted in. Fail-soft — a GC
/// error is logged, never propagated (a slot commit must always succeed
/// even if the daemon is down).
pub async fn reclaim_superseded(prev: &ReasoningConfig, next: &ReasoningConfig) {
    let superseded = superseded_models(prev, next);
    if superseded.is_empty() {
        return;
    }
    let keep = referenced_models(next);
    let pins = pins_from_env();
    let dry_run = !reclaim_opted_in();
    let service = crate::config::Config::get().ollama_service.clone();

    tracing::info!(
        "reasoning: slot change superseded {:?}; running {} model GC (keep {:?}, pins {:?})",
        superseded,
        if dry_run { "announce-only" } else { "reclaim" },
        keep,
        pins,
    );

    let result = reclaim_superseded_via(
        |payload| ipc::call_action(&service, "ollama.gc", payload),
        &keep,
        &pins,
        &superseded,
        dry_run,
    )
    .await;
    match result {
        Some(Ok(v)) => tracing::info!(
            "reasoning: superseded-model GC ({}): reclaimable={} bytes, deleted={}",
            v.get("mode").and_then(|m| m.as_str()).unwrap_or("?"),
            v.get("reclaimable_bytes").cloned().unwrap_or(json!(0)),
            v.get("deleted")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        ),
        Some(Err(e)) => tracing::warn!(
            "reasoning: superseded-model GC failed ({}: {}) — models left in place",
            e.code,
            e.message
        ),
        None => {}
    }
}

/// Fire-and-forget reclaim: spawns [`reclaim_superseded`] when an async
/// runtime exists (the same guard as [`residency::spawn_warm_slots`] —
/// sync test callers fall through). Returns whether a task was spawned.
pub fn spawn_reclaim(prev: ReasoningConfig, next: ReasoningConfig) -> bool {
    if superseded_models(&prev, &next).is_empty() {
        return false;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!("reasoning: no async runtime; superseded-model GC not started");
        return false;
    }
    tokio::spawn(async move {
        reclaim_superseded(&prev, &next).await;
    });
    true
}

#[cfg(test)]
// These async tests hold the sync `TEST_ENV_LOCK` across the
// `reclaim_superseded_via` `.await` to serialise `WYLDE_EMBED_MODEL`
// mutation against sibling env-mutating suites. The awaited closures never
// take `TEST_ENV_LOCK`, so there is no deadlock and the lint is a false
// positive here.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use crate::turn::reasoning::config::{ModelSlots, DEFAULT_EMBED_MODEL, DEFAULT_REASONER_MODEL};
    use std::sync::Mutex;

    fn cfg_with(reasoner: &str, fast: &str, embedder: &str) -> ReasoningConfig {
        ReasoningConfig {
            enabled: true,
            slots: ModelSlots {
                embedder: embedder.to_owned(),
                fast: fast.to_owned(),
                reasoner: reasoner.to_owned(),
            },
            ..ReasoningConfig::default()
        }
    }

    #[test]
    fn referenced_set_is_the_slots_plus_embedder() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let cfg = ReasoningConfig::default();
        let refs = referenced_models(&cfg);
        // Default single-mode: reasoner == fast ⇒ two distinct tags.
        assert!(refs.contains(DEFAULT_REASONER_MODEL));
        assert!(refs.contains(DEFAULT_EMBED_MODEL));
        assert_eq!(refs.len(), 2, "shared brain dedupes: {refs:?}");
    }

    #[test]
    fn superseded_is_old_minus_new_reference() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let prev = cfg_with("oldR", "oldR", "nomic");
        let next = cfg_with("newR", "newR", "nomic");
        let sup = superseded_models(&prev, &next);
        assert_eq!(sup, set(&["oldR"]));
        // The still-referenced embedder is NOT superseded.
        assert!(!sup.contains("nomic"));
    }

    #[test]
    fn no_change_supersedes_nothing() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        let cfg = cfg_with("R", "R", "nomic");
        assert!(superseded_models(&cfg, &cfg).is_empty());
    }

    #[test]
    fn a_tag_still_referenced_by_another_slot_is_not_superseded() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("WYLDE_EMBED_MODEL");
        // reasoner changes R→R2 but `fast` still points at R ⇒ R stays
        // referenced, nothing superseded.
        let prev = cfg_with("R", "R", "nomic");
        let next = cfg_with("R2", "R", "nomic");
        let sup = superseded_models(&prev, &next);
        assert!(!sup.contains("R"), "R still referenced by fast: {sup:?}");
        assert!(sup.is_empty());
    }

    #[tokio::test]
    async fn via_skips_the_call_when_nothing_superseded() {
        let called = Mutex::new(0u32);
        let out = reclaim_superseded_via(
            |_p| {
                *called.lock().unwrap() += 1;
                async { Ok(json!({})) }
            },
            &set(&["Y"]),
            &BTreeSet::new(),
            &BTreeSet::new(), // empty superseded
            true,
        )
        .await;
        assert!(out.is_none());
        assert_eq!(*called.lock().unwrap(), 0, "no GC call with empty set");
    }

    #[tokio::test]
    async fn via_passes_superseded_keep_pins_and_dry_run() {
        let seen = Mutex::new(None);
        let _ = reclaim_superseded_via(
            |p| {
                *seen.lock().unwrap() = Some(p);
                async { Ok(json!({"ok": true})) }
            },
            &set(&["Ynew", "nomic"]),
            &set(&["pinned-model"]),
            &set(&["Xold"]),
            true,
        )
        .await;
        let p = seen.lock().unwrap().clone().unwrap();
        assert_eq!(p["dry_run"], true);
        assert_eq!(p["superseded"], json!(["Xold"]));
        // keep + pins carried through (order is BTreeSet-sorted).
        assert_eq!(p["keep"], json!(["Ynew", "nomic"]));
        assert_eq!(p["pins"], json!(["pinned-model"]));
    }

    pub(super) fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }
}
