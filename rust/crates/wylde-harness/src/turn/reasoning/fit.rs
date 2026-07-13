//! Pure VRAM fit picker — `fit()` → [`SlotFit`] (scope §3.2, slice S1).
//!
//! Prices a [`ModelSlots`] combo against a VRAM budget and *suggests* a
//! [`ReasonMode`] — it **never blocks, only warns** (the workspaces
//! readiness-chip pattern). All I/O (model sizes from `ollama.list_models`,
//! budget from the broker's `vram.state`) happens in the caller
//! ([`super::handle_fit_check`]); this module is deterministic and
//! unit-testable in isolation, like `wylde-reasoning-plan::evaluate`.

use std::collections::HashMap;

use serde::Serialize;

use super::config::{ModelSlots, ReasonMode};

/// The verdict the fit picker hands the GUI's inline fit chip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlotFit {
    /// Every distinct slot model fits in the budget at once.
    pub co_resident: bool,
    /// Summed per-model estimates (deduped by tag — Single mode counts the
    /// shared brain once).
    pub total_estimate_bytes: u64,
    /// The VRAM budget priced against. `0` = unknown (broker unreachable).
    pub budget_bytes: u64,
    /// What the picker would run: `Single` when fast == reasoner (Aaron's
    /// derivation rule) or when the combo won't co-reside; `Split` when a
    /// genuine split fits.
    pub suggested_mode: ReasonMode,
    /// Human-readable advisories for the fit chip. Empty = clean fit.
    pub warnings: Vec<String>,
    /// A combo that *does* fit, when the given one doesn't (the §3.2
    /// "reasoner = fast, one brain" collapse). `None` when no change needed
    /// or nothing better to offer.
    pub suggestion: Option<ModelSlots>,
}

/// Price `slots` against `budget_bytes`.
///
/// * `sizes` — estimated resident bytes per model tag (on-disk size ×
///   `WYLDE_OLLAMA_VRAM_ESTIMATE_MULT`, assembled by the caller). A tag
///   missing from the map is priced at 0 with a "not pulled?" warning —
///   the configured default may legitimately not be pulled yet.
/// * `budget_bytes == 0` — budget unknown; the picker warns and reports
///   `co_resident: false` without suggesting a collapse (no data to
///   decide with).
pub fn fit(
    slots: &ModelSlots,
    mode: ReasonMode,
    budget_bytes: u64,
    sizes: &HashMap<String, u64>,
) -> SlotFit {
    let mut warnings = Vec::new();

    // Dedupe by tag: Single mode (fast == reasoner) prices the shared brain
    // once; the embedder is always its own entry unless it shares a tag too.
    let mut unique: Vec<&str> = Vec::new();
    for tag in [
        slots.embedder.as_str(),
        slots.fast.as_str(),
        slots.reasoner.as_str(),
    ] {
        if !tag.is_empty() && !unique.contains(&tag) {
            unique.push(tag);
        }
    }

    let mut total: u64 = 0;
    for tag in &unique {
        match sizes.get(*tag) {
            Some(bytes) => total += bytes,
            None => warnings.push(format!(
                "size unknown for {tag} — not pulled locally? pull it before enabling reasoning"
            )),
        }
    }

    if budget_bytes == 0 {
        warnings.push("VRAM budget unknown (broker unreachable) — fit not verifiable".to_owned());
        return SlotFit {
            co_resident: false,
            total_estimate_bytes: total,
            budget_bytes,
            suggested_mode: mode,
            warnings,
            suggestion: None,
        };
    }

    let co_resident = total <= budget_bytes;
    let derived = slots.derived_mode();

    let (suggested_mode, suggestion) = if derived == ReasonMode::Single {
        // One brain already — nothing to collapse. Warn if even that
        // doesn't fit (advisory only; Ollama will offload to DRAM).
        if !co_resident {
            warnings.push(format!(
                "slot set (~{:.1} GiB est.) exceeds the {:.1} GiB VRAM budget — expect \
                 DRAM offload / slower deep turns",
                gib(total),
                gib(budget_bytes)
            ));
        }
        (ReasonMode::Single, None)
    } else if co_resident {
        (ReasonMode::Split, None)
    } else {
        // A genuine split that doesn't co-reside: suggest the §3.2 collapse
        // — reasoner = fast, one brain plans and executes.
        warnings.push(format!(
            "split slots (~{:.1} GiB est.) exceed the {:.1} GiB VRAM budget — suggest \
             Single mode (reasoner = fast)",
            gib(total),
            gib(budget_bytes)
        ));
        (
            ReasonMode::Single,
            Some(ModelSlots {
                embedder: slots.embedder.clone(),
                fast: slots.fast.clone(),
                reasoner: slots.fast.clone(),
            }),
        )
    };

    SlotFit {
        co_resident,
        total_estimate_bytes: total,
        budget_bytes,
        suggested_mode,
        warnings,
        suggestion,
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn sizes(entries: &[(&str, u64)]) -> HashMap<String, u64> {
        entries.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    fn split_slots() -> ModelSlots {
        ModelSlots {
            embedder: "nomic-embed-text".into(),
            fast: "qwen2.5:7b-instruct".into(),
            reasoner: "deepseek-r1:14b".into(),
        }
    }

    #[test]
    fn reference_rig_24gb_split_fits() {
        // The scope §3.2 reference matrix: 1.5 + 5 + 10 ≈ 16.5 GiB on 24.
        let s = sizes(&[
            ("nomic-embed-text", 3 * GIB / 2),
            ("qwen2.5:7b-instruct", 5 * GIB),
            ("deepseek-r1:14b", 10 * GIB),
        ]);
        let f = fit(&split_slots(), ReasonMode::Split, 24 * GIB, &s);
        assert!(f.co_resident);
        assert_eq!(f.suggested_mode, ReasonMode::Split);
        assert!(
            f.warnings.is_empty(),
            "clean fit has no warnings: {:?}",
            f.warnings
        );
        assert!(f.suggestion.is_none());
        assert_eq!(f.total_estimate_bytes, (16 * GIB) + GIB / 2);
    }

    #[test]
    fn big_reasoner_suggests_single_with_collapse() {
        // 32B-class reasoner (~20 GiB) blows the 24 GiB budget → Single,
        // with a concrete reasoner=fast suggestion.
        let big = ModelSlots {
            reasoner: "qwq:32b".into(),
            ..split_slots()
        };
        let s = sizes(&[
            ("nomic-embed-text", 3 * GIB / 2),
            ("qwen2.5:7b-instruct", 5 * GIB),
            ("qwq:32b", 20 * GIB),
        ]);
        let f = fit(&big, ReasonMode::Split, 24 * GIB, &s);
        assert!(!f.co_resident);
        assert_eq!(f.suggested_mode, ReasonMode::Single);
        let suggestion = f.suggestion.expect("collapse suggested");
        assert_eq!(suggestion.reasoner, suggestion.fast, "one brain");
        assert_eq!(suggestion.embedder, "nomic-embed-text");
        assert!(f.warnings.iter().any(|w| w.contains("Single")));
    }

    #[test]
    fn single_mode_dedupes_the_shared_brain() {
        // Default shape: fast == reasoner — the shared model is priced once.
        let single = ModelSlots {
            embedder: "nomic-embed-text".into(),
            fast: "big-brain:35b".into(),
            reasoner: "big-brain:35b".into(),
        };
        let s = sizes(&[("nomic-embed-text", GIB), ("big-brain:35b", 20 * GIB)]);
        let f = fit(&single, ReasonMode::Single, 24 * GIB, &s);
        assert_eq!(f.total_estimate_bytes, 21 * GIB, "not 41 GiB");
        assert!(f.co_resident);
        assert_eq!(f.suggested_mode, ReasonMode::Single);
        assert!(
            f.suggestion.is_none(),
            "already one brain — nothing to collapse"
        );
    }

    #[test]
    fn tight_rig_single_warns_but_never_blocks() {
        let single = ModelSlots {
            embedder: "nomic-embed-text".into(),
            fast: "big-brain:35b".into(),
            reasoner: "big-brain:35b".into(),
        };
        let s = sizes(&[("nomic-embed-text", GIB), ("big-brain:35b", 25 * GIB)]);
        let f = fit(&single, ReasonMode::Single, 24 * GIB, &s);
        assert!(!f.co_resident);
        assert_eq!(f.suggested_mode, ReasonMode::Single);
        assert!(f.suggestion.is_none());
        assert!(
            f.warnings.iter().any(|w| w.contains("DRAM offload")),
            "advisory warning present: {:?}",
            f.warnings
        );
    }

    /// Aaron's RTX 5080 budget as the broker reports it (16303 MiB per
    /// nvidia-smi, 2026-07-13) — the rig the default slots must fit.
    const DEV_RIG_BUDGET: u64 = 16303 * 1024 * 1024;

    #[test]
    fn default_slots_on_the_dev_rig_measured_vs_x12_estimate() {
        use super::super::config::{DEFAULT_EMBED_MODEL, DEFAULT_REASONER_MODEL};

        // MEASURED truth (2026-07-13 fit probe, embedder co-loaded): the
        // 35B-A3B UD-IQ3_XXS default is fully GPU-resident at ≤32k ctx —
        // 12.93 GiB model total + ~0.33 GiB embedder vs 15.9 GiB. Priced
        // with measured bytes the verdict is CLEAN. If this fails, the
        // default outgrew the reference rig.
        // 12.93 GiB model total (`ollama ps`, 32k ctx) + ~0.33 GiB embedder.
        const MEASURED_REASONER_BYTES: u64 = 13_882_470_000;
        const MEASURED_EMBEDDER_BYTES: u64 = 354_334_801;
        let measured = sizes(&[
            (DEFAULT_EMBED_MODEL, MEASURED_EMBEDDER_BYTES),
            (DEFAULT_REASONER_MODEL, MEASURED_REASONER_BYTES),
        ]);
        let f = fit(
            &ModelSlots::default(),
            ReasonMode::Single,
            DEV_RIG_BUDGET,
            &measured,
        );
        assert!(f.co_resident, "measured residency fits the 16 GB rig");
        assert_eq!(f.suggested_mode, ReasonMode::Single);
        assert!(f.suggestion.is_none());
        assert!(f.warnings.is_empty(), "clean fit: {:?}", f.warnings);

        // The ×1.2-disk-estimate wart, since S2 CONFINED to unloaded
        // models: `probe_model_sizes` now overlays measured `/api/ps`
        // footprints for loaded models (and warm slots keep the defaults
        // loaded), so the live chip prices this quant at its real
        // 12.9 GiB once resident and the spurious DRAM-offload advisory
        // clears. This case pins pure fit() on the raw ×1.2 inputs — the
        // verdict a fresh boot shows before anything has loaded: an
        // advisory only (never blocks, no collapse to suggest).
        let disk_x12 = sizes(&[
            (DEFAULT_EMBED_MODEL, (274_302_450_f64 * 1.2) as u64),
            (DEFAULT_REASONER_MODEL, (14_113_978_939_f64 * 1.2) as u64),
        ]);
        let f = fit(
            &ModelSlots::default(),
            ReasonMode::Single,
            DEV_RIG_BUDGET,
            &disk_x12,
        );
        assert!(!f.co_resident, "×1.2 disk estimate exceeds the budget");
        assert_eq!(f.suggested_mode, ReasonMode::Single, "advisory only");
        assert!(f.suggestion.is_none(), "one brain — nothing to collapse");
        assert!(
            f.warnings.iter().any(|w| w.contains("DRAM offload")),
            "the known advisory: {:?}",
            f.warnings
        );
    }

    #[test]
    fn official_35b_a3b_on_the_dev_rig_warns_offload_honestly() {
        // The model Aaron's ruling swapped OUT, pinned with its real
        // numbers: qwen3.6:35b-a3b is 23_938_333_577 bytes on disk
        // (× 1.2 est. ≈ 26.8 GiB) vs 15.9 GiB. One brain, so no collapse
        // to suggest — the correct verdict is the DRAM-offload advisory
        // (never a block, and no spurious "not pulled" warning).
        let slots = ModelSlots {
            embedder: "nomic-embed-text".into(),
            fast: "qwen3.6:35b-a3b".into(),
            reasoner: "qwen3.6:35b-a3b".into(),
        };
        let s = sizes(&[
            ("nomic-embed-text", (274_302_450_f64 * 1.2) as u64),
            ("qwen3.6:35b-a3b", (23_938_333_577_f64 * 1.2) as u64),
        ]);
        let f = fit(&slots, ReasonMode::Single, DEV_RIG_BUDGET, &s);
        assert!(!f.co_resident, "26.8 GiB est. cannot fit 15.9 GiB");
        assert_eq!(f.suggested_mode, ReasonMode::Single);
        assert!(f.suggestion.is_none());
        assert!(
            f.warnings.iter().any(|w| w.contains("DRAM offload")),
            "honest offload advisory: {:?}",
            f.warnings
        );
        assert!(
            !f.warnings.iter().any(|w| w.contains("not pulled")),
            "the tag IS pulled — no spurious warning: {:?}",
            f.warnings
        );
    }

    #[test]
    fn unpulled_model_warns_and_prices_zero() {
        let s = sizes(&[("nomic-embed-text", GIB)]);
        let f = fit(&split_slots(), ReasonMode::Split, 24 * GIB, &s);
        assert_eq!(f.total_estimate_bytes, GIB);
        assert_eq!(
            f.warnings
                .iter()
                .filter(|w| w.contains("not pulled"))
                .count(),
            2,
            "both missing models flagged: {:?}",
            f.warnings
        );
    }

    #[test]
    fn unknown_budget_warns_without_suggesting() {
        let s = sizes(&[
            ("nomic-embed-text", GIB),
            ("qwen2.5:7b-instruct", 5 * GIB),
            ("deepseek-r1:14b", 10 * GIB),
        ]);
        let f = fit(&split_slots(), ReasonMode::Split, 0, &s);
        assert!(!f.co_resident);
        assert_eq!(
            f.suggested_mode,
            ReasonMode::Split,
            "no data — keep the user's mode"
        );
        assert!(f.suggestion.is_none());
        assert!(f.warnings.iter().any(|w| w.contains("budget unknown")));
    }
}
