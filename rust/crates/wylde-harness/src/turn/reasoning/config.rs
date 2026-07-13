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
//! by an explicit, persisted opt-in **and** a per-turn `depth:"deep"` flag.
//!
//! ## Aaron's locked slot decisions (2026-07-13)
//!
//! 1. **Default reasoner = the locally-pulled Qwen3.6-35B-A3B build**
//!    ([`DEFAULT_REASONER_MODEL`]). The tag is the exact string Ollama
//!    reports for the pulled model (verified via `ollama list`); the
//!    official-registry `qwen3.6:35b-a3b` alias is NOT pulled on the dev
//!    box, so the hf.co tag is the honest default.
//! 2. **PLAN and EXECUTE run on the SAME model.** The `fast` slot defaults
//!    to the same tag as `reasoner`, and `fast == reasoner ⇒ Single` is
//!    Aaron's confirmed derivation rule (scope DECISION #11), so
//!    [`ReasoningConfig::default`] carries `mode: Single`. The
//!    [`ModelSlots`] *structure* keeps all three user-swappable slots —
//!    a user can re-split later — but the default is deliberately NOT a
//!    fast/reasoner split; do not "restore" one.
//! 3. **PLAN may read long-term memory / lessons on bound conversations.**
//!    A deliberate, Aaron-authorized (2026-07-13) relaxation of the D2
//!    privacy rule (which confines long-term memory to *unbound*
//!    conversations in the normal gather). The S3 `PlanInputs.lessons`
//!    selector reads the long-term reflection store directly, without the
//!    D2 workspace filter. Documented here so a future reader doesn't
//!    "fix" it back — see the implementation plan's R7 resolution.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The exact tag Ollama knows the default reasoner by on the dev box
/// (Aaron's decision 1, 2026-07-13: "qwen 3.6 35B a3b"). Verified against
/// `ollama list` — this is the pulled 21 GB Q4_K_M build; there is no
/// plain `qwen3.6:35b-a3b` registry tag present locally.
pub const DEFAULT_REASONER_MODEL: &str =
    "hf.co/mradermacher/Qwen3.6-35B-A3B-Abliterix-EGA-abliterated-i1-GGUF:Qwen3.6-35B-A3B-Abliterix-EGA-abliterated.i1-Q4_K_M";

/// Default embedder — matches `crate::memory::common::embed_model()`'s
/// fallback so the slot and the env-driven embed path agree out of the box.
/// (S2 unifies them: the slot becomes the settings-backed source with
/// `WYLDE_EMBED_MODEL` as the override.)
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";

/// Per-turn reasoning depth. `Fast` = today's ReAct loop, byte-identical.
/// `Deep` = the gated PLAN→EXECUTE→REFLECT pipeline (S3+). Never
/// Deep-by-default (locked — the *Illusion-of-Thinking* tax).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    #[default]
    Fast,
    Deep,
}

impl Depth {
    pub fn as_str(self) -> &'static str {
        match self {
            Depth::Fast => "fast",
            Depth::Deep => "deep",
        }
    }

    /// Tolerant parse: unknown/empty strings are `None` so the resolution
    /// chain (payload → config → Fast) can fall through, never fail a turn.
    pub fn parse(s: &str) -> Option<Depth> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fast" => Some(Depth::Fast),
            "deep" => Some(Depth::Deep),
            _ => None,
        }
    }
}

/// Split vs Single mode (scope §3.5, DECISION #11 — confirmed by Aaron).
/// Derived-but-overridable: `fast == reasoner ⇒ Single`. Default `Single`
/// per Aaron's 2026-07-13 same-model decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonMode {
    /// fast slot ≠ reasoner slot — reason ONCE on the reasoner, execute on
    /// fast.
    Split,
    /// fast slot == reasoner slot — one brain plans AND executes. The
    /// default (Aaron: plan+execute on the same model).
    Single,
}

impl Default for ReasonMode {
    fn default() -> Self {
        ReasonMode::Single
    }
}

/// When the in-loop REFLECT critique fires (scope §5, OQ-6 recommended
/// default). Inert until S5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectGate {
    Off,
    MultiToolOnly,
    Always,
}

impl Default for ReflectGate {
    fn default() -> Self {
        ReflectGate::MultiToolOnly
    }
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
    /// Aaron's confirmed derivation rule (DECISION #11): identical fast and
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

    /// The three model slots. Defaults per Aaron's 2026-07-13 decisions.
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

    /// Fast→Deep self-escalation on hard surprise (scope OQ-5 recommended
    /// default ON; fast→deep ONLY, never deep→fast). Inert until S4.
    #[serde(default = "default_true")]
    pub auto_escalate: bool,

    /// Max replans per turn; exhaustion = finalize with a visible note,
    /// never a silent stop (OQ-4). Inert until S4. Default 2.
    #[serde(default = "default_replan_budget")]
    pub replan_budget: u8,

    /// Hard cap on reasoner think tokens (`num_predict` on the plan call).
    /// Inert until S3. Default 4096.
    #[serde(default = "default_think_budget")]
    pub think_budget_tokens: u32,

    /// When REFLECT fires (OQ-6). Inert until S5. Default MultiToolOnly.
    #[serde(default)]
    pub reflect_gate: ReflectGate,
}

fn default_true() -> bool {
    true
}
fn default_replan_budget() -> u8 {
    2
}
fn default_think_budget() -> u32 {
    4096
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slots: ModelSlots::default(),
            mode: ReasonMode::default(),
            default_depth: Depth::Fast,
            auto_escalate: true,
            replan_budget: default_replan_budget(),
            think_budget_tokens: default_think_budget(),
            reflect_gate: ReflectGate::default(),
        }
    }
}

impl ReasoningConfig {
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

/// `<data_dir>` resolved exactly the way every other settings store does
/// (`WYLDE_DATA_DIR` → `DATA_DIR` → `<WYLDE_ROOT>/.wylde/data`). Read on
/// every call so tests can point the env at a scratch dir per-case.
fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_DATA_DIR") {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("DATA_DIR") {
        return PathBuf::from(v);
    }
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join(".wylde").join("data")
}

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
            "Aaron 2026-07-13: plan+execute on the same model"
        );
        assert_eq!(c.slots.fast, c.slots.reasoner, "same-model default");
        assert_eq!(c.slots.reasoner, DEFAULT_REASONER_MODEL);
        assert_eq!(c.slots.embedder, DEFAULT_EMBED_MODEL);
        assert!(c.auto_escalate);
        assert_eq!(c.replan_budget, 2);
        assert_eq!(c.think_budget_tokens, 4096);
        assert_eq!(c.reflect_gate, ReflectGate::MultiToolOnly);
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
        assert_eq!(Depth::parse("deep"), Some(Depth::Deep));
        assert_eq!(Depth::parse(" DEEP "), Some(Depth::Deep));
        assert_eq!(Depth::parse(""), None);
        assert_eq!(Depth::parse("medium"), None, "unknown falls through");
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
            default_depth: Depth::Deep,
            auto_escalate: false,
            replan_budget: 3,
            think_budget_tokens: 2048,
            reflect_gate: ReflectGate::Always,
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
