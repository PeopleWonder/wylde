//! Hardware-aware model recommendations.
//!
//! Reads a `HardwareSnapshot` (from `system.inventory` on the VRAM
//! broker) and picks a small, ordered list of `ollama pull <name>`
//! suggestions calibrated for the available compute.  The names mirror
//! the curated picks in `docs/first-run-bootstrap.md` so the first-run
//! wizard and the Models panel surface the same vocabulary.
//!
//! The picker is pure data; the View renders the result as a row of
//! pull-this-instead chips next to the input field.

use crate::ipc::HardwareSnapshot;

/// One recommended pull, in priority order (highest-fit first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// `ollama pull <name>` argument.
    pub name: String,
    /// User-facing reason — surfaces in the chip's tooltip so the
    /// "why" is in source rather than scattered across docs.
    pub reason: &'static str,
}

/// Pick recommendations matching the user's hardware.  Always returns
/// at least one entry — the smallest model in the catalogue — so even
/// the unknown-hardware path is actionable.
pub fn pick(hw: &HardwareSnapshot) -> Vec<Recommendation> {
    if hw.is_unknown() {
        return vec![Recommendation {
            name: "qwen2.5:0.5b".to_owned(),
            reason: "Default starter — fits any box; broker offline so we can't sharpen the pick.",
        }];
    }

    let mut out: Vec<Recommendation> = Vec::new();
    let vram_gb = bytes_to_gb(hw.nvidia_vram_bytes);
    let ram_gb = bytes_to_gb(hw.ram_total_bytes);

    // Heavy NVIDIA — fits a serious local LLM happily.
    if vram_gb >= 24.0 {
        out.push(Recommendation {
            name: "qwen2.5:14b".to_owned(),
            reason: "24 GB+ VRAM — runs a 14B chat model comfortably.",
        });
        out.push(Recommendation {
            name: "qwen2.5-coder:14b".to_owned(),
            reason: "24 GB+ VRAM — coding-specialised peer for the chat model above.",
        });
    } else if vram_gb >= 12.0 {
        out.push(Recommendation {
            name: "qwen2.5:7b".to_owned(),
            reason: "12 GB+ VRAM — 7B chat fits with room for context.",
        });
    } else if vram_gb >= 6.0 {
        out.push(Recommendation {
            name: "llama3.2:3b".to_owned(),
            reason: "6 GB+ VRAM — 3B Llama with fast inference.",
        });
    }

    // No NVIDIA: lean on system RAM for CPU-only inference.
    if hw.nvidia_count == 0 {
        if ram_gb >= 32.0 {
            out.push(Recommendation {
                name: "qwen2.5:7b".to_owned(),
                reason: "32 GB+ RAM, CPU inference — 7B is the largest practical model.",
            });
        } else if ram_gb >= 16.0 {
            out.push(Recommendation {
                name: "llama3.2:3b".to_owned(),
                reason: "16 GB+ RAM — 3B Llama runs in CPU mode.",
            });
        }
    }

    // Always include a starter — small, fast, fits anywhere.
    out.push(Recommendation {
        name: "qwen2.5:0.5b".to_owned(),
        reason: "Tiny starter — runs on any machine.  Good for the first chat.",
    });

    // Embedding model — useful for any RAG-aware workspace.
    out.push(Recommendation {
        name: "nomic-embed-text".to_owned(),
        reason: "Embedding model — required for workspace RAG.",
    });

    // De-dup while preserving order.
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|r| seen.insert(r.name.clone()));
    out
}

fn bytes_to_gb(b: u64) -> f32 {
    (b as f64 / 1024.0 / 1024.0 / 1024.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(vram_gb: u64, ram_gb: u64, nvidia_count: u32) -> HardwareSnapshot {
        HardwareSnapshot {
            cpu_brand: "Test".to_owned(),
            ram_total_bytes: ram_gb * 1024 * 1024 * 1024,
            nvidia_vram_bytes: vram_gb * 1024 * 1024 * 1024,
            nvidia_count,
            intel_count: 0,
            amd_count: 0,
            has_npu: false,
        }
    }

    #[test]
    fn unknown_hardware_returns_single_starter() {
        let recs = pick(&HardwareSnapshot::default());
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "qwen2.5:0.5b");
    }

    #[test]
    fn heavy_nvidia_box_recommends_14b_models() {
        let recs = pick(&hw(24, 64, 1));
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"qwen2.5:14b"));
        assert!(names.contains(&"qwen2.5-coder:14b"));
        assert!(names.contains(&"qwen2.5:0.5b")); // starter still present
    }

    #[test]
    fn mid_nvidia_box_recommends_7b() {
        let recs = pick(&hw(12, 32, 1));
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"qwen2.5:7b"));
        assert!(!names.contains(&"qwen2.5:14b"));
    }

    #[test]
    fn cpu_only_with_16gb_ram_recommends_3b() {
        let recs = pick(&hw(0, 16, 0));
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"llama3.2:3b"));
    }

    #[test]
    fn cpu_only_with_low_ram_recommends_starter_only() {
        let recs = pick(&hw(0, 8, 0));
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"qwen2.5:0.5b"));
        assert!(!names.contains(&"llama3.2:3b"));
    }

    #[test]
    fn always_includes_embedding_model_when_known() {
        let recs = pick(&hw(12, 32, 1));
        assert!(recs.iter().any(|r| r.name == "nomic-embed-text"));
    }

    #[test]
    fn embedding_model_is_absent_for_unknown_hardware() {
        // Unknown-hardware path is deliberately the minimum actionable
        // surface — no RAG hint when we can't even confirm the broker
        // is up.
        let recs = pick(&HardwareSnapshot::default());
        assert!(!recs.iter().any(|r| r.name == "nomic-embed-text"));
    }

    #[test]
    fn no_duplicate_names_in_output() {
        let recs = pick(&hw(24, 64, 1));
        let mut names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "recommendation list had duplicates");
    }
}
