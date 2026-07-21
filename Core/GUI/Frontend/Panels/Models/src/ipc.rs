//! Per-panel IPC helpers for the Models panel.
//!
//! Reads / mutates the local Ollama model registry via `wylde-ollama`:
//!
//!   * `ollama.list_models`  — `/api/tags` passthrough; rows include
//!     name, size, modified-at, and a `details` block carrying family /
//!     parameter_size / quantization_level.
//!   * `ollama.list_loaded`  — `/api/ps` passthrough; rows we cross-
//!     reference against `list_models` so the panel can render an
//!     "in use" pill on resident models.
//!   * `ollama.pull`         — streaming.  NDJSON progress chunks
//!     come back with `{status, digest?, total?, completed?}`.  The
//!     panel View accumulates them; this module just supplies the
//!     `PipeStream` and a parsed-projection helper.
//!   * `ollama.delete`       — drop a local model.
//!
//! Hardware-aware recommendations consult `system.inventory` on
//! `wylde-vram-broker` (Phase 12.4 → reports CPU + RAM + every GPU
//! family).  The merge to a "fits your box" hint is pure data shaping;
//! see `recommend::pick`.
//!
//! Default-model star: persisted via the harness `models.set_default` /
//! `models.get_default` pipe verbs (wired 2026-05-30).  The panel reads
//! `get_default` on load to pre-check the star and writes `set_default`
//! when the user toggles it; `session_default` stays as the optimistic
//! mirror so the star updates instantly without waiting on the reply.

use serde_json::{json, Value};

pub const SVC_OLLAMA: &str = "wylde-ollama";
pub const SVC_BROKER: &str = "wylde-vram-broker";
/// The harness pipe — owns the persisted default-model preference
/// (`models.get_default` / `models.set_default`).
pub const SVC_HARNESS: &str = "wylde-harness";

// ── Wire shapes (projected) ──────────────────────────────────────────

/// One row from `ollama.list_models` projected to the fields the panel
/// actually renders.  The full passthrough envelope is bigger; we keep
/// only what the row needs so a future Ollama schema bump doesn't ripple
/// into the View.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstalledModel {
    pub name: String,
    /// Bytes on disk.  0 when the field is missing — Ollama's older
    /// builds omit it for a freshly-pulled tag until the next list.
    pub size_bytes: u64,
    pub modified_at: String,
    pub family: String,
    /// "7B", "1.5B", etc.  Empty when not reported.
    pub param_size: String,
    pub quantization: String,
}

impl InstalledModel {
    pub fn from_value(v: &Value) -> Self {
        let details = v.get("details").cloned().unwrap_or_else(|| json!({}));
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            size_bytes: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            modified_at: v
                .get("modified_at")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            family: details
                .get("family")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            param_size: details
                .get("parameter_size")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            quantization: details
                .get("quantization_level")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// The models the running config actively references — the reasoning
/// slots read from `settings.reasoning.get`. The panel labels each
/// installed row against this set so "what references this model?" (and
/// therefore "is it safe to delete?") is answerable at a glance (#131):
/// a model matching no slot, no VRAM-resident set, and not the session
/// default is superseded/orphaned and safe to drop. Empty strings mean
/// "slot unset"; a down harness leaves the whole set empty (every model
/// then reads as unreferenced only if it's also not loaded / not default).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReferenceSet {
    pub reasoner: String,
    pub fast: String,
    pub embedder: String,
}

impl ReferenceSet {
    pub fn from_value(v: &Value) -> Self {
        let slots = v.get("slots").cloned().unwrap_or_else(|| json!({}));
        let slot = |k: &str| {
            slots
                .get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        Self {
            reasoner: slot("reasoner"),
            fast: slot("fast"),
            embedder: slot("embedder"),
        }
    }
}

/// One chunk of the `ollama.pull` NDJSON stream, projected to the
/// fields the progress bar reads.  `status` is always present; the
/// `completed` / `total` pair appears on the per-layer download
/// progress chunks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PullProgress {
    pub status: String,
    pub completed: u64,
    pub total: u64,
    /// Per-layer digest — when present the same `completed`/`total`
    /// pair belongs to one of several blobs.  We don't render per-
    /// digest UI; the field is kept so a later slice can show "layer
    /// 3 of 7" without re-shaping the projection.
    pub digest: String,
}

impl PullProgress {
    pub fn from_value(v: &Value) -> Self {
        Self {
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            completed: v.get("completed").and_then(|x| x.as_u64()).unwrap_or(0),
            total: v.get("total").and_then(|x| x.as_u64()).unwrap_or(0),
            digest: v
                .get("digest")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// `true` when the upstream stream signalled completion.  Ollama
    /// fires `{"status": "success"}` as the last frame; the streaming
    /// verb passes it through verbatim.
    pub fn is_success(&self) -> bool {
        self.status.eq_ignore_ascii_case("success")
    }

    /// 0.0..=1.0 fraction completed, or `None` when the stream doesn't
    /// yet carry a `total`.  The first few chunks (manifest fetch,
    /// metadata) have no progress numbers — we render an indeterminate
    /// spinner for those frames.
    pub fn ratio(&self) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        let r = self.completed as f32 / self.total as f32;
        Some(r.clamp(0.0, 1.0))
    }
}

/// Snapshot of `system.inventory` projected to the bits the
/// recommendation engine consults.  Reports zero / empty for every
/// field when the broker is down — the recommend layer treats that as
/// "unknown hardware" rather than failing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HardwareSnapshot {
    pub cpu_brand: String,
    /// Total system RAM in bytes.
    pub ram_total_bytes: u64,
    /// Largest single NVIDIA GPU VRAM (bytes) reported by NVML.  0 when
    /// no NVIDIA card was discovered.
    pub nvidia_vram_bytes: u64,
    /// Number of NVIDIA GPUs.
    pub nvidia_count: u32,
    /// Intel iGPU/dGPU count from DXGI.  Present-but-zero on non-
    /// Windows hosts where DXGI isn't available.
    pub intel_count: u32,
    /// AMD GPU count from DXGI.
    pub amd_count: u32,
    /// NPU present (per CPU-brand heuristic).
    pub has_npu: bool,
}

impl HardwareSnapshot {
    pub fn from_value(v: &Value) -> Self {
        let cpu_brand = v
            .get("cpu")
            .and_then(|c| c.get("brand"))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let ram_total_bytes = v
            .get("memory_total_bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let gpus = v
            .get("gpus")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let nvidia_count = gpus.len() as u32;
        let nvidia_vram_bytes = gpus
            .iter()
            .map(|g| {
                g.get("memory_total_bytes")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        let intel_count = v
            .get("intel_gpus")
            .and_then(|x| x.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        let amd_count = v
            .get("amd_gpus")
            .and_then(|x| x.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        let has_npu = v
            .get("npus")
            .and_then(|x| x.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        Self {
            cpu_brand,
            ram_total_bytes,
            nvidia_vram_bytes,
            nvidia_count,
            intel_count,
            amd_count,
            has_npu,
        }
    }

    pub fn is_unknown(&self) -> bool {
        self.cpu_brand.is_empty() && self.ram_total_bytes == 0 && self.nvidia_count == 0
    }
}

// ── Unary verbs ──────────────────────────────────────────────────────

pub async fn list_installed_models() -> Result<Vec<InstalledModel>, String> {
    let v = wylde_gui_pipe::call(
        SVC_OLLAMA,
        "POST",
        "/__action__",
        Some(json!({ "action": "ollama.list_models", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("models").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<InstalledModel> = arr.iter().map(InstalledModel::from_value).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Names of the models currently held in VRAM (per `/api/ps`).  Used
/// to render the "in use" pill on each row.
pub async fn list_loaded_model_names() -> Result<Vec<String>, String> {
    let v = wylde_gui_pipe::call(
        SVC_OLLAMA,
        "POST",
        "/__action__",
        Some(json!({ "action": "ollama.list_loaded", "payload": {} })),
    )
    .await?;
    let Some(arr) = v.get("models").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("name").and_then(|x| x.as_str()).map(str::to_owned))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Delete a local model. Returns the bytes freed as reported by the
/// wrapper (`freed_bytes`), or 0 when the wrapper couldn't determine the
/// size — the panel then falls back to the size it already had cached for
/// the row, so it can still report "Freed N" (#131).
pub async fn delete_installed_model(name: &str) -> Result<u64, String> {
    let v = wylde_gui_pipe::call(
        SVC_OLLAMA,
        "POST",
        "/__action__",
        Some(json!({
            "action": "ollama.delete",
            "payload": { "name": name },
        })),
    )
    .await?;
    Ok(v.get("freed_bytes").and_then(|x| x.as_u64()).unwrap_or(0))
}

/// Read the reasoning slots (`settings.reasoning.get` on the harness) so
/// the panel can label which installed models the running config
/// references. Soft-fails to an empty [`ReferenceSet`] on a down harness —
/// the panel then just shows no slot labels rather than erroring.
pub async fn get_reasoning_slots() -> Result<ReferenceSet, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "settings.reasoning.get", "payload": {} })),
    )
    .await?;
    Ok(ReferenceSet::from_value(&v))
}

/// Read `system.inventory` from the VRAM broker.  Soft-fails on the
/// "broker offline" path — the caller renders a "broker offline" hint
/// instead of an error toast.
pub async fn read_hardware() -> Result<HardwareSnapshot, String> {
    let v = wylde_gui_pipe::call(
        SVC_BROKER,
        "POST",
        "/__action__",
        Some(json!({ "action": "system.inventory", "payload": {} })),
    )
    .await?;
    Ok(HardwareSnapshot::from_value(&v))
}

/// Read the persisted default-model star.  `Ok(None)` means no default
/// is set (and no `WYLDE_DEFAULT_MODEL` env fallback configured); the
/// panel then leaves every star un-filled.  Soft-fails to `None` on a
/// down harness so the panel still renders the installed list.
pub async fn get_default() -> Result<Option<String>, String> {
    let v = wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "models.get_default", "payload": {} })),
    )
    .await?;
    Ok(v.get("model")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

/// Persist the default-model star.  `Some(name)` stars it; `None` clears
/// the choice (the harness then falls back to `WYLDE_DEFAULT_MODEL`).
/// The panel mirrors the choice into `session_default` optimistically,
/// so this call is fire-and-confirm — an `Err` only surfaces a
/// transport problem the panel logs into its error strip.
pub async fn set_default(name: Option<&str>) -> Result<(), String> {
    let model = match name {
        Some(n) => json!(n),
        None => Value::Null,
    };
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": "models.set_default", "payload": { "model": model } })),
    )
    .await
    .map(|_| ())
}

// ── Streaming verbs ──────────────────────────────────────────────────

/// Open a streaming `ollama.pull` for `model_name`.  The View loops on
/// the returned `PipeStream`, accumulates progress, and drops the
/// stream on cancel/Done — the harness's pull driver wires drop →
/// "client disconnected mid-pull → abandon" so the cancel button on
/// the View has the cancel semantics it implies.
pub fn pull_model(model_name: &str) -> Result<wylde_gui_pipe::PipeStream, String> {
    wylde_gui_pipe::stream_call(SVC_OLLAMA, "ollama.pull", json!({ "name": model_name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_model_parses_full_payload() {
        let v = json!({
            "name": "qwen2.5:1.5b",
            "modified_at": "2026-05-29T10:00:00Z",
            "size": 1_500_000_000_u64,
            "details": {
                "family": "qwen2.5",
                "parameter_size": "1.5B",
                "quantization_level": "Q4_K_M",
            }
        });
        let m = InstalledModel::from_value(&v);
        assert_eq!(m.name, "qwen2.5:1.5b");
        assert_eq!(m.size_bytes, 1_500_000_000);
        assert_eq!(m.family, "qwen2.5");
        assert_eq!(m.param_size, "1.5B");
        assert_eq!(m.quantization, "Q4_K_M");
    }

    #[test]
    fn installed_model_defaults_missing_fields() {
        let m = InstalledModel::from_value(&json!({}));
        assert!(m.name.is_empty());
        assert_eq!(m.size_bytes, 0);
        assert!(m.family.is_empty());
    }

    #[test]
    fn pull_progress_ratio_returns_none_without_total() {
        let p = PullProgress::from_value(&json!({"status": "pulling manifest"}));
        assert!(p.ratio().is_none());
        assert!(!p.is_success());
    }

    #[test]
    fn pull_progress_ratio_clamps_into_unit_interval() {
        let p = PullProgress::from_value(&json!({
            "status": "downloading",
            "completed": 5_u64,
            "total": 10_u64,
        }));
        assert_eq!(p.ratio(), Some(0.5));
    }

    #[test]
    fn pull_progress_overshoot_clamps_to_one() {
        // The Ollama driver has been observed to report completed >=
        // total briefly between layers; render as full rather than a
        // bar that visually overflows.
        let p = PullProgress::from_value(&json!({
            "status": "downloading",
            "completed": 12_u64,
            "total": 10_u64,
        }));
        assert_eq!(p.ratio(), Some(1.0));
    }

    #[test]
    fn pull_progress_is_success_is_case_insensitive() {
        let p = PullProgress::from_value(&json!({"status": "Success"}));
        assert!(p.is_success());
    }

    #[test]
    fn hardware_snapshot_parses_phase_12_4_shape() {
        let v = json!({
            "cpu": { "brand": "AMD Ryzen 9" },
            "memory_total_bytes": 64_u64 * 1024 * 1024 * 1024,
            "memory_available_bytes": 32_u64 * 1024 * 1024 * 1024,
            "gpus": [
                { "memory_total_bytes": 24_u64 * 1024 * 1024 * 1024 }
            ],
            "intel_gpus": [],
            "amd_gpus": [],
            "npus": [],
        });
        let hw = HardwareSnapshot::from_value(&v);
        assert_eq!(hw.cpu_brand, "AMD Ryzen 9");
        assert_eq!(hw.nvidia_count, 1);
        assert_eq!(hw.nvidia_vram_bytes, 24_u64 * 1024 * 1024 * 1024);
        assert_eq!(hw.intel_count, 0);
        assert!(!hw.has_npu);
        assert!(!hw.is_unknown());
    }

    #[test]
    fn hardware_snapshot_is_unknown_for_empty_envelope() {
        let hw = HardwareSnapshot::from_value(&json!({}));
        assert!(hw.is_unknown());
    }

    #[test]
    fn hardware_snapshot_picks_largest_nvidia_vram() {
        let v = json!({
            "cpu": { "brand": "Intel" },
            "memory_total_bytes": 32_u64 * 1024 * 1024 * 1024,
            "gpus": [
                { "memory_total_bytes": 8_u64 * 1024 * 1024 * 1024 },
                { "memory_total_bytes": 24_u64 * 1024 * 1024 * 1024 },
            ],
        });
        let hw = HardwareSnapshot::from_value(&v);
        assert_eq!(hw.nvidia_count, 2);
        assert_eq!(hw.nvidia_vram_bytes, 24_u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn reference_set_parses_reasoning_get_slots() {
        let v = json!({
            "enabled": true,
            "slots": { "embedder": "nomic-embed-text", "fast": "qwen2.5:1.5b", "reasoner": "qwen2.5:7b" },
            "mode": "single",
        });
        let refs = ReferenceSet::from_value(&v);
        assert_eq!(refs.reasoner, "qwen2.5:7b");
        assert_eq!(refs.fast, "qwen2.5:1.5b");
        assert_eq!(refs.embedder, "nomic-embed-text");
    }

    #[test]
    fn reference_set_defaults_empty_when_slots_absent() {
        let refs = ReferenceSet::from_value(&json!({}));
        assert!(refs.reasoner.is_empty() && refs.fast.is_empty() && refs.embedder.is_empty());
    }

    #[test]
    fn pipe_call_helpers_exist() {
        // Build-time witness pattern — matches Settings / Memory tests.
        let _ = list_installed_models;
        let _ = list_loaded_model_names;
        let _ = delete_installed_model;
        let _ = read_hardware;
        let _ = pull_model;
        let _ = get_default;
        let _ = set_default;
        let _ = get_reasoning_slots;
    }

    #[test]
    fn service_constants_match_pipe_prefix() {
        assert_eq!(SVC_OLLAMA, "wylde-ollama");
        assert_eq!(SVC_BROKER, "wylde-vram-broker");
        assert_eq!(SVC_HARNESS, "wylde-harness");
    }
}
