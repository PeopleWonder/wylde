//! Default-model **resolution** — the persisted star, checked against
//! what is actually on disk, with sensible fallbacks (#235).
//!
//! [`model_state`](super::model_state) owns *persistence* (the starred
//! choice in `default_model.json`, plus the `WYLDE_DEFAULT_MODEL` env
//! fallback). This module owns *resolution*: given that persisted choice
//! and the live on-disk inventory from #131/#132, decide which model the
//! picker should actually land on — and what to say when there is no
//! answer.
//!
//! There is deliberately **no second store here.** `default_model.json`
//! remains the one place a default lives; this is a pure function over
//! it.
//!
//! ## The order (locked, the maintainer 2026-07-22)
//!
//! 1. [`Resolution::Persisted`] — the user's star, **if the model is
//!    still present in the inventory**. The store is the arbiter, not the
//!    file: #131 made deleting a model a one-click operation, so a star
//!    outliving its model is an ordinary event, not a corruption.
//! 2. [`Resolution::FirstAvailable`] — else the first model in the
//!    inventory. This covers both "never chose a default" and "chose one
//!    and later deleted it"; [`Resolution::stale_default`] distinguishes
//!    them for the UI, but the *outcome* is the same and neither errors.
//! 3. [`Resolution::Recommend`] — else (an empty store) a recommendation
//!    of [`RECOMMENDED_MODEL`] carrying [`RECOMMENDATION_WARNINGS`].
//!
//! ## Why arm 3 recommends instead of pulling
//!
//! Same discipline as the locked never-auto-delete decision, pointed the
//! other way: Wylde never moves ~6.6 GB across someone's network, or
//! commits their VRAM, because a picker was empty. The recommend arm is
//! a *statement with warnings* that the user acts on — the panel renders
//! a pull button, and nothing downloads until it is pressed.
//!
//! ## Matching tolerates the implicit `:latest`
//!
//! Ollama reports `nomic-embed-text` as `nomic-embed-text:latest` in
//! `/api/tags`, but a user (or `WYLDE_DEFAULT_MODEL`) may star the bare
//! name. [`tags_match`] treats the two as the same model, matching the
//! slot-labelling rule #131 already established for the Models panel.

/// The model recommended when nothing at all is installed (the
/// maintainer 2026-07-22). The real ~9B on-device Qwen: 6.6 GB on disk,
/// Q4_K_M, 9.7B parameters, 262k context, tools + thinking capable.
///
/// Deliberately **not** [`DEFAULT_REASONER_MODEL`] (the 35B-A3B
/// UD-IQ3_XXS quant locked by the 2026-07-13 planning eval). Different
/// slot, different job: the reasoner is the PLAN brain chosen for
/// schema-valid planning throughput, this is the everyday chat model
/// chosen to be the *first* thing a new install downloads — small enough
/// to finish pulling, good enough to keep.
///
/// [`DEFAULT_REASONER_MODEL`]: crate::turn::reasoning::config::DEFAULT_REASONER_MODEL
pub const RECOMMENDED_MODEL: &str = "qwen3.5:9b";

/// On-disk size of [`RECOMMENDED_MODEL`], for the download warning.
pub const RECOMMENDED_MODEL_SIZE: &str = "6.6 GB";

/// The warnings that MUST travel with [`RECOMMENDED_MODEL`] whenever the
/// recommend arm is surfaced. Ordered most-blocking first: what it costs
/// to get it, whether it will run on this box, and what the first use
/// feels like.
///
/// These are copy, not prose to paraphrase — the panel renders them
/// verbatim so the warning a user sees is the warning in source. Kept
/// here rather than in the GUI so the harness reply is self-describing
/// and a second surface (first-run wizard, CLI) can't drift from it.
pub const RECOMMENDATION_WARNINGS: &[&str] = &[
    "Download is 6.6 GB. It happens once, but on a slow or metered connection that is a real cost — and the pull has to finish before the first chat.",
    "Needs roughly 8 GB of VRAM at default context; comfortable on a 16 GB card with the embedder loaded alongside it. On a smaller card Ollama spills into system RAM — it still answers, just slower.",
    "The first message after a pull is slower than the rest while the weights load into VRAM. That pause is normal and does not repeat.",
    "Nothing is downloaded until you choose to pull it.",
];

/// Where a resolved default came from — the outcome of walking the
/// order above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Arm 1 — the persisted star, confirmed present in the inventory.
    Persisted {
        /// The inventory's spelling of the tag, not the star's. If the
        /// star is `nomic-embed-text` and the store says
        /// `nomic-embed-text:latest`, callers want the latter: it is
        /// what the Ollama API will accept.
        model: String,
    },
    /// Arm 2 — first model in the inventory.
    FirstAvailable {
        model: String,
        /// `Some(tag)` when a persisted star *was* set but no longer
        /// resolves — the model was deleted or renamed out from under
        /// it. Purely explanatory: the fallback is identical either
        /// way, and this never becomes an error.
        stale_default: Option<String>,
    },
    /// Arm 3 — nothing installed. A recommendation with warnings; the
    /// caller pulls only on an explicit user action.
    Recommend {
        model: &'static str,
        size: &'static str,
        warnings: &'static [&'static str],
        /// As in [`Self::FirstAvailable`] — a star that outlived an
        /// emptied store still explains itself.
        stale_default: Option<String>,
    },
}

impl Resolution {
    /// The model to select, or `None` in the recommend arm (where there
    /// is nothing installed to select).
    pub fn selected(&self) -> Option<&str> {
        match self {
            Resolution::Persisted { model } | Resolution::FirstAvailable { model, .. } => {
                Some(model.as_str())
            }
            Resolution::Recommend { .. } => None,
        }
    }

    /// Stable wire/telemetry discriminant: `"default"`, `"first_available"`,
    /// or `"recommend"`.
    pub fn source(&self) -> &'static str {
        match self {
            Resolution::Persisted { .. } => "default",
            Resolution::FirstAvailable { .. } => "first_available",
            Resolution::Recommend { .. } => "recommend",
        }
    }

    /// The star that was set but didn't resolve, if any. `None` in the
    /// [`Resolution::Persisted`] arm by construction.
    pub fn stale_default(&self) -> Option<&str> {
        match self {
            Resolution::Persisted { .. } => None,
            Resolution::FirstAvailable { stale_default, .. }
            | Resolution::Recommend { stale_default, .. } => stale_default.as_deref(),
        }
    }
}

/// Whether two Ollama tags name the same model, treating a bare name as
/// its own `:latest`. Case-insensitive: Ollama tags are lowercased in
/// practice, but a hand-typed `WYLDE_DEFAULT_MODEL` need not be.
pub fn tags_match(a: &str, b: &str) -> bool {
    fn canon(t: &str) -> String {
        let t = t.trim().to_ascii_lowercase();
        match t.split_once(':') {
            Some((base, "latest")) => base.to_owned(),
            _ => t,
        }
    }
    canon(a) == canon(b)
}

/// Resolve the default model against the live inventory.
///
/// `persisted` is the starred choice (`None` when never set);
/// `inventory` is the on-disk model list in the order the store reports
/// it — arm 2 takes `inventory[0]`, so "first available" means whatever
/// the caller considers first, not a re-sort imposed here.
///
/// Never fails. A star pointing at a deleted model falls through to arm
/// 2 (or arm 3) rather than erroring — see the module doc.
pub fn resolve(persisted: Option<&str>, inventory: &[String]) -> Resolution {
    // A blank star is an unset star.
    let persisted = persisted.map(str::trim).filter(|s| !s.is_empty());

    if let Some(star) = persisted {
        if let Some(hit) = inventory.iter().find(|m| tags_match(m, star)) {
            // Prefer the inventory's spelling — that is the tag the
            // Ollama API will accept.
            return Resolution::Persisted { model: hit.clone() };
        }
    }
    // Either no star, or a star whose model is gone. Both land here; only
    // the explanatory field differs.
    let stale_default = persisted.map(str::to_owned);

    match inventory.first() {
        Some(first) => Resolution::FirstAvailable {
            model: first.clone(),
            stale_default,
        },
        None => Resolution::Recommend {
            model: RECOMMENDED_MODEL,
            size: RECOMMENDED_MODEL_SIZE,
            warnings: RECOMMENDATION_WARNINGS,
            stale_default,
        },
    }
}

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::model_registry::actions::{disabled, rust_enabled, OllamaActions};
use crate::model_registry::model_state;

// â”€â”€ models.resolve_default (#235) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// `models.resolve_default` â€” the default model resolved against the
/// **live on-disk inventory**, per the #235 order: persisted star (still
/// installed) â†’ first available â†’ recommend.
///
/// This is the verb a picker should call on mount. [`handle_get_default`]
/// answers "what is starred?" and is deliberately unvalidated (a star may
/// legitimately name a model the user is about to pull); this answers
/// "what should I select right now?", which requires the store.
///
/// Reply:
/// ```json
/// { "model": "qwen3.5:9b"|null,
///   "source": "default"|"first_available"|"recommend",
///   "stale_default": "deepseek-r1:14b"|null,
///   "recommendation": {"model","size","warnings":[â€¦]}|null,
///   "inventory_count": 3 }
/// ```
///
/// **Unreachable is not empty (#132).** If `ollama.list_models` fails we
/// return the upstream error rather than an empty inventory â€” resolving a
/// down daemon to "nothing installed, here's a 6.6 GB download" is
/// exactly the silent-empty bug #132 closed, pointed at the user's
/// bandwidth. The caller keeps its prior selection and retries.
pub async fn handle_resolve_default<O: OllamaActions + ?Sized>(
    _payload: Value,
    ollama: &O,
) -> Reply {
    if !rust_enabled() {
        return disabled();
    }
    let reply = ollama.call("ollama.list_models", json!({})).await;
    if !reply.ok {
        // Surface the outage; never fabricate an empty store.
        return reply.error.map(Reply::err).unwrap_or_else(|| {
            Reply::err_msg(
                "unavailable",
                "ollama.list_models failed â€” the model store is unreachable, \
                 which is NOT the same as empty; not resolving a default",
            )
        });
    }
    let inventory: Vec<String> = reply
        .data
        .get("models")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .filter(|n| !n.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();

    let resolution = resolve(model_state::get_default_model().as_deref(), &inventory);

    let recommendation = match &resolution {
        Resolution::Recommend {
            model,
            size,
            warnings,
            ..
        } => json!({ "model": model, "size": size, "warnings": warnings }),
        _ => Value::Null,
    };

    Reply::ok(json!({
        "model": resolution.selected(),
        "source": resolution.source(),
        "stale_default": resolution.stale_default(),
        "recommendation": recommendation,
        "inventory_count": inventory.len(),
    }))
}

#[cfg(test)]
// The handler tests hold the sync `TEST_ENV_LOCK` across an in-process
// `.await` to serialise env-var mutation against the sibling
// model_registry tests. The handler never acquires `TEST_ENV_LOCK`, so
// there is no deadlock risk and the lint is a false positive here.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use crate::model_registry::actions::handle_set_default;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Enable the Rust handlers + point `model_state` at a fresh tempdir.
    /// Returns the dir guard (keep it alive for the test).
    fn enabled_isolated() -> tempfile::TempDir {
        std::env::set_var("WYLDE_HARNESS_MODELS_IMPL", "rust");
        let td = tempdir().unwrap();
        std::env::set_var("ACTIVE_MODEL_PATH", td.path().join("active_model.json"));
        std::env::set_var("DEFAULT_MODEL_PATH", td.path().join("default_model.json"));
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        model_state::reset_for_tests();
        td
    }

    /// Fake Ollama that replays one queued reply and records what it saw.
    struct FakeOllama {
        reply: Mutex<Option<Reply>>,
        seen: Mutex<Vec<(String, Value)>>,
    }

    impl FakeOllama {
        fn new(reply: Reply) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl OllamaActions for FakeOllama {
        async fn call(&self, action: &str, payload: Value) -> Reply {
            self.seen
                .lock()
                .unwrap()
                .push((action.to_owned(), payload.clone()));
            self.reply
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Reply::ok(json!({})))
        }
    }

    fn inv(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    // ── Arm 1: the persisted star wins when it is still installed ──────

    #[test]
    fn persisted_default_wins_when_present() {
        let r = resolve(Some("llama3.2:3b"), &inv(&["qwen3.5:9b", "llama3.2:3b"]));
        assert_eq!(r.source(), "default");
        assert_eq!(r.selected(), Some("llama3.2:3b"));
        assert_eq!(r.stale_default(), None);
    }

    #[test]
    fn persisted_default_beats_a_different_first_entry() {
        // The whole point of a star: it overrides position in the list.
        let r = resolve(Some("llama3.2:3b"), &inv(&["qwen3.5:9b", "llama3.2:3b"]));
        assert_ne!(r.selected(), Some("qwen3.5:9b"));
    }

    #[test]
    fn star_matches_across_the_implicit_latest() {
        // Bare star, `:latest` in the store.
        let r = resolve(Some("nomic-embed-text"), &inv(&["nomic-embed-text:latest"]));
        assert_eq!(r.source(), "default");
        assert_eq!(
            r.selected(),
            Some("nomic-embed-text:latest"),
            "resolves to the inventory's spelling — the tag Ollama accepts"
        );
        // …and the reverse: `:latest` star, bare name in the store.
        let r = resolve(Some("nomic-embed-text:latest"), &inv(&["nomic-embed-text"]));
        assert_eq!(r.source(), "default");
    }

    #[test]
    fn star_match_is_case_insensitive() {
        let r = resolve(Some("QWEN3.5:9B"), &inv(&["qwen3.5:9b"]));
        assert_eq!(r.source(), "default");
    }

    #[test]
    fn a_non_latest_tag_is_not_confused_with_another() {
        // `:9b` and `:35b` are different models, not a latest-alias pair.
        let r = resolve(Some("qwen3.5:35b"), &inv(&["qwen3.5:9b"]));
        assert_eq!(r.source(), "first_available");
    }

    // ── Arm 2: first available ────────────────────────────────────────

    #[test]
    fn no_default_falls_to_first_available() {
        let r = resolve(None, &inv(&["qwen3.5:9b", "llama3.2:3b"]));
        assert_eq!(r.source(), "first_available");
        assert_eq!(r.selected(), Some("qwen3.5:9b"));
        assert_eq!(r.stale_default(), None, "nothing was starred to go stale");
    }

    #[test]
    fn deleted_default_falls_through_to_first_available() {
        // THE regression this feature exists to prevent: the star
        // outlives its model (#131 made deleting one click) and the
        // picker must land on something real, not a phantom tag.
        let r = resolve(
            Some("deepseek-r1:14b"),
            &inv(&["qwen3.5:9b", "llama3.2:3b"]),
        );
        assert_eq!(r.source(), "first_available");
        assert_eq!(r.selected(), Some("qwen3.5:9b"));
        assert_eq!(
            r.stale_default(),
            Some("deepseek-r1:14b"),
            "the dangling star is reported, not silently dropped"
        );
    }

    #[test]
    fn a_blank_star_is_an_unset_star() {
        for blank in ["", "   ", "\t"] {
            let r = resolve(Some(blank), &inv(&["qwen3.5:9b"]));
            assert_eq!(r.source(), "first_available");
            assert_eq!(r.stale_default(), None, "blank is unset, not stale");
        }
    }

    #[test]
    fn first_available_honours_inventory_order() {
        let r = resolve(None, &inv(&["llama3.2:3b", "qwen3.5:9b"]));
        assert_eq!(
            r.selected(),
            Some("llama3.2:3b"),
            "first means the store's first — no re-sort imposed here"
        );
    }

    // ── Arm 3: the recommend state ────────────────────────────────────

    #[test]
    fn empty_inventory_yields_the_recommend_state() {
        let r = resolve(None, &[]);
        assert_eq!(r.source(), "recommend");
        assert_eq!(r.selected(), None, "nothing installed to select");
        match r {
            Resolution::Recommend {
                model,
                size,
                warnings,
                stale_default,
            } => {
                assert_eq!(model, "qwen3.5:9b");
                assert_eq!(size, "6.6 GB");
                assert!(
                    !warnings.is_empty(),
                    "a recommendation without warnings \
                     is an auto-download with extra steps"
                );
                assert_eq!(stale_default, None);
            }
            other => panic!("expected Recommend, got {other:?}"),
        }
    }

    #[test]
    fn recommend_state_survives_a_stale_star() {
        // Deleted the last model, and it happened to be the starred one.
        let r = resolve(Some("qwen2.5:0.5b"), &[]);
        assert_eq!(r.source(), "recommend");
        assert_eq!(r.stale_default(), Some("qwen2.5:0.5b"));
    }

    #[test]
    fn warnings_cover_size_vram_and_first_run() {
        let joined = RECOMMENDATION_WARNINGS.join(" ").to_ascii_lowercase();
        assert!(joined.contains("6.6 gb"), "download size is stated");
        assert!(joined.contains("vram"), "hardware fit is stated");
        assert!(joined.contains("first message"), "first-run cost is stated");
        assert!(
            joined.contains("nothing is downloaded"),
            "the no-auto-pull promise is explicit"
        );
    }

    #[test]
    fn the_recommended_model_is_qwen35_not_qwen36() {
        // Guard against the recurring typo: `qwen3.6:9b` does not
        // exist. The 3.6 line is the 35B-A3B reasoner; the 9B is 3.5.
        assert_eq!(RECOMMENDED_MODEL, "qwen3.5:9b");
        assert!(!RECOMMENDED_MODEL.contains("3.6"));
    }

    // ── tags_match ────────────────────────────────────────────────────

    #[test]
    fn tags_match_handles_latest_and_whitespace() {
        assert!(tags_match("a", "a:latest"));
        assert!(tags_match("a:latest", "a"));
        assert!(tags_match(" a:latest ", "a"));
        assert!(tags_match("a:1b", "a:1b"));
        assert!(!tags_match("a:1b", "a"));
        assert!(!tags_match("a", "b"));
    }

    #[test]
    fn tags_match_keeps_registry_prefixed_tags_distinct() {
        // hf.co/-prefixed pulls carry slashes and colons; they must not
        // collapse into each other.
        let a = "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS";
        let b = "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:Q4_K_M";
        assert!(tags_match(a, a));
        assert!(!tags_match(a, b));
    }

    // â”€â”€ models.resolve_default (#235) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// `/api/tags`-shaped inventory reply.
    fn tags(names: &[&str]) -> Reply {
        Reply::ok(json!({
            "models": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>()
        }))
    }

    #[tokio::test]
    async fn resolve_default_prefers_a_persisted_star_that_is_installed() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        handle_set_default(json!({ "model": "llama3.2:3b" })).await;

        let fake = FakeOllama::new(tags(&["qwen3.5:9b", "llama3.2:3b"]));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert!(r.ok);
        assert_eq!(r.data["source"], "default");
        assert_eq!(r.data["model"], "llama3.2:3b");
        assert_eq!(r.data["stale_default"], Value::Null);
        assert_eq!(r.data["recommendation"], Value::Null);
        assert_eq!(r.data["inventory_count"], 2);
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_survives_a_restart() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        handle_set_default(json!({ "model": "llama3.2:3b" })).await;

        // Simulate a process restart: drop every in-memory cache so the
        // next read must come off disk.
        model_state::reset_for_tests();

        let fake = FakeOllama::new(tags(&["qwen3.5:9b", "llama3.2:3b"]));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert_eq!(
            r.data["model"], "llama3.2:3b",
            "the star must outlive the process, not just the session"
        );
        assert_eq!(r.data["source"], "default");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_falls_through_when_the_star_was_deleted() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        handle_set_default(json!({ "model": "deepseek-r1:14b" })).await;

        // The store no longer carries it â€” the user deleted it (#131).
        let fake = FakeOllama::new(tags(&["qwen3.5:9b", "llama3.2:3b"]));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert!(r.ok, "a dangling star is an ordinary event, never an error");
        assert_eq!(r.data["source"], "first_available");
        assert_eq!(r.data["model"], "qwen3.5:9b");
        assert_eq!(r.data["stale_default"], "deepseek-r1:14b");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_uses_first_available_when_nothing_is_starred() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let fake = FakeOllama::new(tags(&["qwen3.5:9b", "llama3.2:3b"]));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert_eq!(r.data["source"], "first_available");
        assert_eq!(r.data["model"], "qwen3.5:9b");
        assert_eq!(r.data["stale_default"], Value::Null);
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_recommends_when_the_store_is_empty() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let fake = FakeOllama::new(tags(&[]));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert!(r.ok);
        assert_eq!(r.data["source"], "recommend");
        assert_eq!(r.data["model"], Value::Null);
        assert_eq!(r.data["inventory_count"], 0);
        assert_eq!(r.data["recommendation"]["model"], "qwen3.5:9b");
        assert_eq!(r.data["recommendation"]["size"], "6.6 GB");
        let warnings = r.data["recommendation"]["warnings"]
            .as_array()
            .expect("warnings travel with the recommendation")
            .clone();
        assert!(
            !warnings.is_empty(),
            "a recommendation without warnings is an auto-download with extra steps"
        );
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_never_mistakes_unreachable_for_empty() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        // #132's distinction, applied to resolution: a daemon still
        // restarting after an update must NOT resolve to "nothing
        // installed â€” here is a 6.6 GB download".
        let fake = FakeOllama::new(Reply::err_msg("unavailable", "connection refused"));
        let r = handle_resolve_default(Value::Null, &fake).await;
        assert!(
            !r.ok,
            "an outage surfaces as an error, not a recommendation"
        );
        assert_eq!(r.error.unwrap().code, "unavailable");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }

    #[tokio::test]
    async fn resolve_default_reads_the_inventory_once_from_list_models() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _td = enabled_isolated();
        let fake = FakeOllama::new(tags(&["qwen3.5:9b"]));
        handle_resolve_default(Value::Null, &fake).await;
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "ollama.list_models");
        std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");
    }
}
