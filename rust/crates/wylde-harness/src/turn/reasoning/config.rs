//! The agentic-reasoning master toggle + model slots — `ReasoningConfig`
//! (agentic-reasoning plan §6.2 / scope §3.1, slice S1).
//!
//! **Harness-owned, read in-process.** The *consumer* is the turn driver
//! (the `Deep && enabled` gate, S3) and the fit picker; the *writer* is the
//! GUI through the `settings.reasoning.{get,set}` facade verbs. One store
//! read in-process by both — deliberately **not** a second source of truth —
//! the same no-TCP↔pipe-drift discipline as `RoutingConfig`
//! (memory `wylde-settings-ollama-defaults-ux-scope`).
//!
//! The cache + persistence shape is a faithful clone of
//! `wylde_concept_routing::RoutingConfig`: a process-global
//! `OnceLock<Mutex<_>>` lazily seeded from disk, a `current()` snapshot read,
//! and an optimistic `persist()` that updates the cache even when the disk
//! write fails. The on-disk file is `<data_dir>/settings/reasoning.json`.
//!
//! **Fail-safe direction is OFF.** A missing file, a corrupt file, or a
//! malformed value all resolve to [`ReasoningConfig::default`], whose
//! [`ReasoningConfig::enabled`] is `false` — i.e. today's exact fast-path
//! ReAct behaviour, byte-identical. Deep reasoning can only ever be *added*
//! by an explicit, persisted opt-in **and** a per-turn planning-tier
//! `depth` flag (`think` / `think_harder` / `ultrathink`).
//!
//! ## The maintainer's locked slot decisions (2026-07-13)
//!
//! 1. **Default reasoner = the strongest OFFICIAL-WEIGHTS Qwen that FITS
//!    16 GB VRAM** ([`DEFAULT_REASONER_MODEL`], `qwen3.6:35b-a3b` at
//!    unsloth's UD-IQ3_XXS quant, ~13.1 GiB on disk). The maintainer's rulings, in
//!    order (all 2026-07-13): official *weights* only — the abliterated
//!    finetune S1 initially wired is out, but a community GGUF
//!    **quantization of unmodified official weights** is the same model
//!    (provenance: `unsloth/Qwen3.6-35B-A3B-GGUF`, model card
//!    `base_model: Qwen/Qwen3.6-35B-A3B`, no finetune). Fit-in-VRAM beats
//!    parameter count (the official Q4 build of this model, ~26.8 GiB
//!    est., spills badly). The interim `qwen3.5:9b` default was replaced
//!    after the 2026-07-13 planning eval (15 PlanDag prompts, real serde
//!    schema): 9b = 46.7% JSON-valid @128 tok/s vs this quant = 93.3%
//!    freehand / 100% grammar-constrained @166 tok/s, 100% GPU-resident
//!    at 32k ctx (12.93 GiB incl. embedder; spills above 65k — cap
//!    reasoner num_ctx accordingly). 27B@Q3_K_M scored 100% freehand but
//!    never fits (89% GPU @16k, 27 tok/s); 35b-a3b@UD-Q2_K_XL melts down
//!    intermittently (73.3%) — IQ3_XXS is the quant floor for this job.
//! 2. **PLAN and EXECUTE run on the SAME model.** The `fast` slot defaults
//!    to the same tag as `reasoner`, and `fast == reasoner ⇒ Single` is
//!    The maintainer's confirmed derivation rule (scope DECISION #11), so
//!    [`ReasoningConfig::default`] carries `mode: Single`. The
//!    [`ModelSlots`] *structure* keeps all three user-swappable slots —
//!    a user can re-split later — but the default is deliberately NOT a
//!    fast/reasoner split; do not "restore" one.
//! 3. **PLAN may read long-term memory / lessons on bound conversations.**
//!    A deliberate, maintainer-authorized (2026-07-13) relaxation of the D2
//!    privacy rule (which confines long-term memory to *unbound*
//!    conversations in the normal gather). The S3 `PlanInputs.lessons`
//!    selector reads the long-term reflection store directly, without the
//!    D2 workspace filter. Documented here so a future reader doesn't
//!    "fix" it back — see the implementation plan's R7 resolution.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The default reasoner tag (the maintainer's decision 1, 2026-07-13, thrice
/// revised the same day — see the module doc for the eval that locked
/// this): official Qwen3.6-35B-A3B weights, unsloth UD-IQ3_XXS dynamic
/// quant pulled via Ollama's hf.co bridge. ~13.1 GiB on disk, fully
/// GPU-resident on the RTX 5080 with the embedder co-loaded at ≤32k ctx.
/// A plain quantization of official weights — NOT a finetune; abliterated
/// variants remain excluded.
pub const DEFAULT_REASONER_MODEL: &str = "hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-IQ3_XXS";

/// Default embedder. Since S2 the slot IS the settings-backed source of
/// `crate::memory::common::embed_model()` (`WYLDE_EMBED_MODEL` stays the
/// env override) — one definition of "the embedder", and this constant is
/// its final fallback.
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";

/// Per-turn reasoning depth — the thinking TIERS (modelled on Claude's
/// think / think-harder / ultrathink levels, the maintainer 2026-07-14). `Fast` =
/// today's ReAct loop, byte-identical, no planning. Every other tier runs
/// the gated PLAN pipeline with an escalating deliberation budget:
///
/// | tier | reasoner `<think>` | budget | measured (15-prompt eval) |
/// |---|---|---|---|
/// | `Fast` | — (no PLAN call) | — | 0 s added |
/// | `Think` | **disabled** (`think:false`) | JSON output only | ~2–6 s, 100% valid |
/// | `ThinkHarder` | enabled | [`TierBudgets::think_harder`] | tens of seconds |
/// | `Ultrathink` | enabled | [`TierBudgets::ultrathink`] | up to ~1 min |
///
/// **Why the tight tiers disable thinking instead of capping it** (live
/// finding, 2026-07-14): Ollama's `num_predict` caps think + content
/// TOGETHER and a generation that hits the cap mid-`<think>` produces
/// ZERO content — the grammar constrains `message.content` only and
/// cannot force the model out of the think channel. A "tight cap on
/// thinking" is therefore an empty-plan machine; the honest tight tier
/// is deliberation OFF, where the grammar guarantees the JSON.
///
/// Never planning-by-default (locked — the *Illusion-of-Thinking* tax):
/// `default_depth` stays `Fast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    #[default]
    Fast,
    Think,
    ThinkHarder,
    Ultrathink,
}

impl Depth {
    pub fn as_str(self) -> &'static str {
        match self {
            Depth::Fast => "fast",
            Depth::Think => "think",
            Depth::ThinkHarder => "think_harder",
            Depth::Ultrathink => "ultrathink",
        }
    }

    /// Tolerant parse: unknown/empty strings are `None` so the resolution
    /// chain (payload → config → Fast) can fall through, never fail a turn.
    /// The pre-tier wire value `"deep"` still parses — it maps to
    /// [`Depth::ThinkHarder`], which carries the old Deep semantics
    /// (thinking on, the S3-era 4096-token budget).
    pub fn parse(s: &str) -> Option<Depth> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fast" => Some(Depth::Fast),
            "think" => Some(Depth::Think),
            "think_harder" | "think-harder" | "deep" => Some(Depth::ThinkHarder),
            "ultrathink" => Some(Depth::Ultrathink),
            _ => None,
        }
    }

    /// Whether this tier runs the PLAN phase at all.
    pub fn plans(self) -> bool {
        self != Depth::Fast
    }

    /// Whether the PLAN call lets the reasoner deliberate (`<think>`).
    /// `Think` plans grammar-first with deliberation disabled — see the
    /// enum doc for why that is the only workable tight tier.
    pub fn think_enabled(self) -> bool {
        matches!(self, Depth::ThinkHarder | Depth::Ultrathink)
    }

    /// The reasoner think-token allowance for this tier (0 = thinking
    /// disabled). The PLAN call's `num_predict` is this plus the JSON
    /// output allowance (`plan_phase::PLAN_OUTPUT_BUDGET`) — Ollama caps
    /// think + content together, so every tier carries its own output
    /// headroom on top of the think allowance.
    pub fn think_budget(self, budgets: &TierBudgets) -> u32 {
        match self {
            Depth::Fast | Depth::Think => 0,
            Depth::ThinkHarder => budgets.think_harder,
            Depth::Ultrathink => budgets.ultrathink,
        }
    }
}

/// Per-tier reasoner think-token budgets (user-tunable knobs of the tier
/// ladder; the `Think` tier has no budget — its deliberation is off by
/// construction). Defaults are eval-backed (2026-07-14, 15-prompt
/// grounded-plan corpus, see `outputs/reasoning-thinking-tiers-report.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBudgets {
    /// `ThinkHarder`: the S3-era Deep budget.
    #[serde(default = "default_think_harder_budget")]
    pub think_harder: u32,
    /// `Ultrathink`: room for the heavy-rumination tail.
    #[serde(default = "default_ultrathink_budget")]
    pub ultrathink: u32,
}

fn default_think_harder_budget() -> u32 {
    4096
}
fn default_ultrathink_budget() -> u32 {
    10240
}

impl Default for TierBudgets {
    fn default() -> Self {
        Self {
            think_harder: default_think_harder_budget(),
            ultrathink: default_ultrathink_budget(),
        }
    }
}

/// Split vs Single mode (scope §3.5, DECISION #11 — confirmed by the maintainer).
/// Derived-but-overridable: `fast == reasoner ⇒ Single`. Default `Single`
/// per the maintainer's 2026-07-13 same-model decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasonMode {
    /// fast slot ≠ reasoner slot — reason ONCE on the reasoner, execute on
    /// fast.
    Split,
    /// fast slot == reasoner slot — one brain plans AND executes. The
    /// default (the maintainer: plan+execute on the same model).
    #[default]
    Single,
}

/// When the in-loop REFLECT critique fires (scope §5, OQ-6 recommended
/// default). Inert until S5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReflectGate {
    Off,
    #[default]
    MultiToolOnly,
    Always,
}

/// The three co-resident, user-swappable model slots (scope §3.1). The
/// structure keeps embedder/fast/reasoner distinct even though the default
/// points fast and reasoner at the same tag (decision 2) — swappability is
/// the design, the split is not the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSlots {
    /// Context-gather embeddings. Unified with `WYLDE_EMBED_MODEL` in S2
    /// (env wins as override).
    #[serde(default = "default_embedder")]
    pub embedder: String,
    /// The everyday ReAct driver. The payload `model` (composer pick)
    /// overrides this per-turn — the pill picker stays authoritative.
    #[serde(default = "default_reasoner")]
    pub fast: String,
    /// PLAN / REPLAN / REFLECT. Same tag as `fast` by default (decision 2).
    #[serde(default = "default_reasoner")]
    pub reasoner: String,
}

fn default_embedder() -> String {
    DEFAULT_EMBED_MODEL.to_owned()
}
fn default_reasoner() -> String {
    DEFAULT_REASONER_MODEL.to_owned()
}

impl Default for ModelSlots {
    fn default() -> Self {
        Self {
            embedder: default_embedder(),
            fast: default_reasoner(),
            reasoner: default_reasoner(),
        }
    }
}

impl ModelSlots {
    /// The maintainer's confirmed derivation rule (DECISION #11): identical fast and
    /// reasoner slots mean one brain plans and executes.
    pub fn derived_mode(&self) -> ReasonMode {
        if self.fast == self.reasoner {
            ReasonMode::Single
        } else {
            ReasonMode::Split
        }
    }
}

/// The master reasoning config (scope §3.1). Every field has a
/// behaviour-safe default and the whole struct round-trips through JSON;
/// an older file missing a key reads that key as its default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// **THE MASTER TOGGLE.** `false` ⇒ the PLAN/EXECUTE/REFLECT gate is
    /// never entered and the turn is byte-identical to today (plain vector
    /// RAG + plain ReAct). Default `false`.
    #[serde(default)]
    pub enabled: bool,

    /// The three model slots. Defaults per the maintainer's 2026-07-13 decisions.
    #[serde(default)]
    pub slots: ModelSlots,

    /// Split | Single. Default `Single` (same-model decision). The fit
    /// picker *suggests*; this field decides.
    #[serde(default)]
    pub mode: ReasonMode,

    /// Depth when the payload carries none. Default `Fast` — never
    /// Deep-by-default (locked).
    #[serde(default)]
    pub default_depth: Depth,

    /// Fast→planning self-escalation (scope OQ-5) — **LIVE since S4b,
    /// under the maintainer's 2026-07-14 NARROWED identity contract**: reasoning
    /// enabled + Fast tier is byte-identical to today EXCEPT after
    /// [`ESCALATE_AFTER_HARD_FAILURES`](super::ESCALATE_AFTER_HARD_FAILURES)
    /// (2) hard tool failures — L0's exact definition (`[error]` /
    /// `[tier_blocked]` content, or a structural error envelope) — at
    /// which point the turn runs ONE mid-turn PLAN at
    /// [`escalate_tier`](Self::escalate_tier) and continues plan-guided.
    /// Fires at most once per turn, fast→planning ONLY (never the other
    /// way), and every failure path degrades back to plain ReAct. The
    /// e2e transcript proof pins the narrowed contract: zero and one
    /// failure stay byte-identical. Default ON: it can only fire when the
    /// master toggle is already an explicit opt-in AND the turn is
    /// already failing, and the default escalation tier costs ~5 s.
    #[serde(default = "default_true")]
    pub auto_escalate: bool,

    /// The tier an auto-escalated Fast turn plans at (S4b). Default
    /// `Think` — grammar-guaranteed plan at ~5 s; escalating straight to
    /// a deliberating tier would spring a 20–40 s stall the user never
    /// asked for. `Fast` is meaningless here and is clamped to `Think`
    /// by [`Self::escalation_tier`].
    #[serde(default = "default_escalate_tier")]
    pub escalate_tier: Depth,

    /// Max replans per turn (S4, live); exhaustion degrades to plain
    /// ReAct with a visible note, never a silent stop (OQ-4). Default 2.
    #[serde(default = "default_replan_budget")]
    pub replan_budget: u8,

    /// Per-tier reasoner think budgets (the tiers slice replaced the old
    /// single `think_budget_tokens` knob — a file still carrying that key
    /// reads as these defaults). The PLAN call's `num_predict` is the
    /// tier's budget + the JSON output allowance.
    #[serde(default)]
    pub tier_budgets: TierBudgets,

    /// When REFLECT fires (OQ-6). Inert until S5. Default MultiToolOnly.
    #[serde(default)]
    pub reflect_gate: ReflectGate,

    /// Grammar-constrained PLAN decoding (the maintainer, 2026-07-13): pass the
    /// canonical `PlanDag` JSON Schema (`wylde_reasoning_plan::plan_dag_format`)
    /// as Ollama's `format` on PLAN/REPLAN calls. Eval-backed: takes the
    /// default reasoner 93.3% → 100% schema-valid at unchanged speed and
    /// quality. Default ON; the toggle exists so a future backend/model
    /// that misbehaves under grammar constraints can be unwired without a
    /// build. Scope discipline (constrain machine-consumed structured
    /// output, never human-read prose): PLAN yes; the S4 L2 verdict yes
    /// once it exists (tiny yes/no schema); REFLECT only if S5 defines a
    /// structured lessons record; NEVER the chat composition, the tool-call
    /// rounds (native `tools` path), or the `<think>` stream (verified
    /// live: `format` constrains only `message.content` — thinking flows
    /// untouched).
    #[serde(default = "default_true")]
    pub constrained_plan: bool,
}

fn default_true() -> bool {
    true
}
fn default_replan_budget() -> u8 {
    2
}
fn default_escalate_tier() -> Depth {
    Depth::Think
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slots: ModelSlots::default(),
            mode: ReasonMode::default(),
            default_depth: Depth::Fast,
            auto_escalate: true,
            escalate_tier: default_escalate_tier(),
            replan_budget: default_replan_budget(),
            tier_budgets: TierBudgets::default(),
            reflect_gate: ReflectGate::default(),
            constrained_plan: true,
        }
    }
}

impl ReasoningConfig {
    /// The tier an auto-escalated turn plans at, clamped to a planning
    /// tier: a configured `Fast` (which cannot plan) resolves to `Think`.
    pub fn escalation_tier(&self) -> Depth {
        if self.escalate_tier.plans() {
            self.escalate_tier
        } else {
            Depth::Think
        }
    }

    /// Parse the on-disk shape. Tolerant: a non-object, a missing key, or a
    /// wrong-typed value all fall back to the default (so a degraded file
    /// keeps reasoning **off**, never silently on).
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Serialise to the on-disk JSON shape.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

// `<data_dir>` (convention A: `WYLDE_DATA_DIR` → `DATA_DIR` →
// `<WYLDE_ROOT>/.wylde/data`) from the ONE canonical resolver (#138) — this was
// a verbatim copy of that body.
use wylde_shared::paths::data_dir;

/// `<data_dir>/settings/reasoning.json` — alongside the other settings
/// stores (`privacy.json`, `ollama.json`, `concept_routing.json`).
fn config_path() -> PathBuf {
    data_dir().join("settings").join("reasoning.json")
}

/// Read from a specific path. Any failure (missing file, bad JSON) yields
/// the default (reasoning **off**) rather than erroring.
fn read_from_path(path: &std::path::Path) -> ReasoningConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str::<Value>(&s)
            .map(|v| ReasoningConfig::from_value(&v))
            .unwrap_or_default(),
        Err(_) => ReasoningConfig::default(),
    }
}

/// Write to a specific path, creating the parent dir. Writes to a sibling
/// `.tmp` then renames so a crash mid-write can't leave a half-written
/// (and thus parse-failing → fail-off) file.
fn write_to_path(path: &std::path::Path, cfg: &ReasoningConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("reasoning: mkdir: {e}"))?;
    }
    let body = serde_json::to_vec_pretty(&cfg.to_value())
        .map_err(|e| format!("reasoning: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("reasoning: write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("reasoning: rename: {e}"))?;
    Ok(())
}

/// Process-global cache, lazily seeded from disk on first access.
static CACHE: OnceLock<Mutex<ReasoningConfig>> = OnceLock::new();

fn cache() -> &'static Mutex<ReasoningConfig> {
    CACHE.get_or_init(|| Mutex::new(read_from_path(&config_path())))
}

impl ReasoningConfig {
    /// Current snapshot — a cheap clone out of the in-memory cache (seeded
    /// from disk on first access). Safe to call on the per-turn hot path.
    pub fn current() -> ReasoningConfig {
        cache().lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Persist a new snapshot: update the cache **and** write it to disk.
    /// The cache is updated even when the disk write fails (optimistic-write
    /// model), the `Err` surfaces in a banner.
    pub fn persist(next: ReasoningConfig) -> Result<(), String> {
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = next.clone();
        write_to_path(&config_path(), &next)
    }

    /// Force-refresh the cache from disk (out-of-band writers + tests).
    pub fn reload_from_disk() -> ReasoningConfig {
        let fresh = read_from_path(&config_path());
        *cache().lock().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_and_safe() {
        let c = ReasoningConfig::default();
        assert!(!c.enabled, "master toggle defaults OFF");
        assert_eq!(c.default_depth, Depth::Fast, "never Deep-by-default");
        assert_eq!(
            c.mode,
            ReasonMode::Single,
            "maintainer 2026-07-13: plan+execute on the same model"
        );
        assert_eq!(c.slots.fast, c.slots.reasoner, "same-model default");
        assert_eq!(c.slots.reasoner, DEFAULT_REASONER_MODEL);
        assert_eq!(c.slots.embedder, DEFAULT_EMBED_MODEL);
        assert!(c.auto_escalate);
        assert_eq!(
            c.escalate_tier,
            Depth::Think,
            "escalation plans at the cheap grammar-first tier"
        );
        assert_eq!(c.replan_budget, 2);
        assert_eq!(
            c.tier_budgets,
            TierBudgets {
                think_harder: 4096,
                ultrathink: 10240
            }
        );
        assert_eq!(c.reflect_gate, ReflectGate::MultiToolOnly);
        assert!(
            c.constrained_plan,
            "constrained PLAN decoding defaults ON (safe: gated behind enabled=false)"
        );
    }

    #[test]
    fn derived_mode_follows_decision_11() {
        assert_eq!(ModelSlots::default().derived_mode(), ReasonMode::Single);
        let split = ModelSlots {
            fast: "qwen2.5:7b-instruct".into(),
            ..ModelSlots::default()
        };
        assert_eq!(split.derived_mode(), ReasonMode::Split);
    }

    #[test]
    fn depth_parse_is_tolerant() {
        assert_eq!(Depth::parse("fast"), Some(Depth::Fast));
        assert_eq!(Depth::parse("think"), Some(Depth::Think));
        assert_eq!(Depth::parse("think_harder"), Some(Depth::ThinkHarder));
        assert_eq!(Depth::parse("think-harder"), Some(Depth::ThinkHarder));
        assert_eq!(Depth::parse("ultrathink"), Some(Depth::Ultrathink));
        assert_eq!(Depth::parse(""), None);
        assert_eq!(Depth::parse("medium"), None, "unknown falls through");
    }

    #[test]
    fn legacy_deep_maps_to_think_harder() {
        // Pre-tier callers (old GUI builds, extensions) still send "deep";
        // it keeps its S3 semantics: thinking on, the 4096 budget.
        assert_eq!(Depth::parse("deep"), Some(Depth::ThinkHarder));
        assert_eq!(Depth::parse(" DEEP "), Some(Depth::ThinkHarder));
    }

    #[test]
    fn tier_ladder_gates_and_budgets() {
        let b = TierBudgets::default();
        assert!(!Depth::Fast.plans());
        assert!(Depth::Think.plans());
        assert!(Depth::ThinkHarder.plans());
        assert!(Depth::Ultrathink.plans());

        assert!(!Depth::Fast.think_enabled());
        assert!(
            !Depth::Think.think_enabled(),
            "the tight tier plans WITHOUT deliberation — a capped think \
             stream that dies mid-<think> yields zero content"
        );
        assert!(Depth::ThinkHarder.think_enabled());
        assert!(Depth::Ultrathink.think_enabled());

        assert_eq!(Depth::Fast.think_budget(&b), 0);
        assert_eq!(Depth::Think.think_budget(&b), 0);
        assert_eq!(Depth::ThinkHarder.think_budget(&b), 4096);
        assert_eq!(Depth::Ultrathink.think_budget(&b), 10240);
    }

    #[test]
    fn escalation_tier_clamps_to_a_planning_tier() {
        let mut c = ReasoningConfig::default();
        assert_eq!(c.escalation_tier(), Depth::Think);
        c.escalate_tier = Depth::Fast; // nonsense: Fast cannot plan
        assert_eq!(c.escalation_tier(), Depth::Think, "Fast clamps to Think");
        c.escalate_tier = Depth::ThinkHarder;
        assert_eq!(c.escalation_tier(), Depth::ThinkHarder);
    }

    #[test]
    fn missing_keys_default_to_off_safe() {
        let c = ReasoningConfig::from_value(&json!({}));
        assert!(!c.enabled);
        assert_eq!(c.default_depth, Depth::Fast);
        // A partial object only flips the key it carries.
        let c = ReasoningConfig::from_value(&json!({ "enabled": true }));
        assert!(c.enabled);
        assert_eq!(c.replan_budget, 2, "unset knob keeps its default");
        assert_eq!(c.slots, ModelSlots::default());
    }

    #[test]
    fn malformed_value_fails_closed() {
        let c = ReasoningConfig::from_value(&json!({ "enabled": "yes", "replan_budget": "lots" }));
        assert!(!c.enabled, "garbage fails closed to off");
    }

    #[test]
    fn value_round_trips() {
        let c = ReasoningConfig {
            enabled: true,
            slots: ModelSlots {
                embedder: "nomic-embed-text".into(),
                fast: "qwen2.5:7b-instruct".into(),
                reasoner: "deepseek-r1:14b".into(),
            },
            mode: ReasonMode::Split,
            default_depth: Depth::Ultrathink,
            auto_escalate: false,
            escalate_tier: Depth::ThinkHarder,
            replan_budget: 3,
            tier_budgets: TierBudgets {
                think_harder: 2048,
                ultrathink: 16384,
            },
            reflect_gate: ReflectGate::Always,
            constrained_plan: false,
        };
        assert_eq!(ReasoningConfig::from_value(&c.to_value()), c);
    }

    #[test]
    fn wire_shape_uses_snake_case_strings() {
        let v = ReasoningConfig::default().to_value();
        assert_eq!(v["mode"], json!("single"));
        assert_eq!(v["default_depth"], json!("fast"));
        assert_eq!(v["reflect_gate"], json!("multi_tool_only"));
    }

    #[test]
    fn disk_round_trip_through_path() {
        // Hermetic: write + read a scratch path directly, bypassing the
        // process-global cache (which other tests in the binary share).
        let dir = std::env::temp_dir().join(format!("wylde-rsn-cfg-{}", std::process::id()));
        let path = dir.join("settings").join("reasoning.json");
        assert_eq!(read_from_path(&path), ReasoningConfig::default());
        let cfg = ReasoningConfig {
            enabled: true,
            ..ReasoningConfig::default()
        };
        write_to_path(&path, &cfg).expect("write");
        assert_eq!(read_from_path(&path), cfg);
        assert!(!path.with_extension("json.tmp").exists(), "no leftover tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_reads_as_default_off() {
        let dir = std::env::temp_dir().join(format!("wylde-rsn-corrupt-{}", std::process::id()));
        let path = dir.join("reasoning.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert!(!read_from_path(&path).enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
