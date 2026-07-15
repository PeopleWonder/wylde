//! `voice.list_models` — enumerate Whisper / Kokoro snapshots on disk.
//!
//! Scans the HuggingFace cache (the same location `Voice/download_models.py`
//! materialises into) and reports which Whisper variants + the Kokoro
//! voice catalogue are present. Cheap — does NOT load any model weights.
//!
//! Output shape is deliberately verbose so dashboards can render
//! everything without a second round-trip: snapshot path, model size,
//! and the OpenVINO-IR sibling presence flag (matters for NPU users).

use serde_json::{json, Value};
use std::path::PathBuf;
use wylde_shared::ipc::{IpcError, Reply};

use crate::actions::error::invalid_request;
use crate::config::Config;
use crate::model_download::{EnsureJobs, EnsureStatus, KOKORO_REPO};

/// HuggingFace hub cache root. Honours `HUGGINGFACE_HUB_CACHE` /
/// `HF_HOME` like Python's `huggingface_hub.constants` does, then
/// falls back to the per-platform default.
fn hf_cache_root() -> PathBuf {
    if let Some(p) = std::env::var_os("HUGGINGFACE_HUB_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    if let Some(home) = dirs_home() {
        return home.join(".cache").join("huggingface").join("hub");
    }
    PathBuf::from(".cache/huggingface/hub")
}

/// Cross-platform home dir. Avoids pulling in the `dirs` crate for a
/// single use — `USERPROFILE` on Windows, `HOME` elsewhere.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Convert an HF repo id (`"openai/whisper-small"`) into the cache's
/// `models--<owner>--<name>` directory name.
fn cache_dir_name(repo_id: &str) -> String {
    format!("models--{}", repo_id.replace('/', "--"))
}

/// Find the resolved snapshot dir under a repo's cache entry. HF lays
/// out caches as `models--<repo>/snapshots/<commit-sha>/<files>`; we
/// pick the most-recently-modified snapshot when there are several.
fn resolve_snapshot(repo_cache: &std::path::Path) -> Option<PathBuf> {
    let snapshots = repo_cache.join("snapshots");
    if !snapshots.is_dir() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&snapshots).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        match &best {
            None => best = Some((mtime, p)),
            Some((m, _)) if mtime > *m => best = Some((mtime, p)),
            _ => {}
        }
    }
    best.map(|(_, p)| p)
}

/// Total bytes across all regular files in `dir` (recursive). Best-effort —
/// IO errors return 0 for that subtree so a permission-denied symlink
/// doesn't blow up the whole probe.
fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in read.flatten() {
        let p = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes(&p));
        } else if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn probe_repo(repo_id: &str) -> Value {
    let cache = hf_cache_root().join(cache_dir_name(repo_id));
    let snapshot = resolve_snapshot(&cache);
    let installed = snapshot.is_some();
    let snap_path = snapshot.as_ref().map(|p| p.display().to_string());
    let bytes = snapshot.as_ref().map(|p| dir_size_bytes(p)).unwrap_or(0);

    // For Whisper, OpenVINO IR lives in a sibling under
    // `<HF_HUB_CACHE>/ov-export/<repo--with--dashes>/` per
    // `Voice/transcribe.py::_ov_export_dir`. The NPU-static rebuild is
    // `<that>-npu/`. We surface both presence flags so a dashboard can
    // tell at a glance whether the NPU path is ready.
    let ov_export_dir = hf_cache_root()
        .join("ov-export")
        .join(repo_id.replace('/', "--"));
    let ov_export_present = ov_export_dir.join("openvino_encoder_model.xml").exists();
    let ov_npu_dir = ov_export_dir
        .parent()
        .map(|p| {
            p.join(format!(
                "{}-npu",
                ov_export_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ))
        })
        .unwrap_or_else(|| ov_export_dir.with_extension("npu"));
    let ov_npu_present = ov_npu_dir.join("openvino_encoder_model.xml").exists();

    json!({
        "id": repo_id,
        "installed": installed,
        "snapshot_path": snap_path,
        "bytes_on_disk": bytes,
        "ov_export_present": ov_export_present,
        "ov_npu_static_present": ov_npu_present,
    })
}

pub async fn handle_list_models(_payload: Value) -> Reply {
    let cfg = Config::get();

    // Whisper variants we know about. The configured model is always
    // probed first so the most-likely-used entry is at index 0.
    let mut whisper_ids = vec![cfg.stt_model.clone()];
    for canonical in [
        "openai/whisper-tiny",
        "openai/whisper-tiny.en",
        "openai/whisper-base",
        "openai/whisper-base.en",
        "openai/whisper-small",
        "openai/whisper-small.en",
        "openai/whisper-medium",
        "openai/whisper-medium.en",
        "openai/whisper-large-v3",
    ] {
        if !whisper_ids.iter().any(|id| id == canonical) {
            whisper_ids.push(canonical.to_owned());
        }
    }

    let whisper: Vec<Value> = whisper_ids.iter().map(|id| probe_repo(id)).collect();

    // Kokoro is single-repo; report the voice catalogue inline so
    // callers don't need a separate `voice.list_voices` round-trip.
    let kokoro_id = "onnx-community/Kokoro-82M-v1.0-ONNX";
    let mut kokoro = probe_repo(kokoro_id);
    if let Some(obj) = kokoro.as_object_mut() {
        obj.insert("voices".to_owned(), kokoro_voice_catalogue());
    }

    Reply::ok(json!({
        "stt": {
            "active_backend": cfg.stt_backend.as_str(),
            "active_model": cfg.stt_model.clone(),
            "models": whisper,
        },
        "tts": {
            "active_voice": cfg.tts_voice.clone(),
            "model": kokoro,
        },
        "hf_cache_root": hf_cache_root().display().to_string(),
    }))
}

/// `voice.download_models` — kick a Rust-native bootstrap of the Whisper
/// STT + Kokoro TTS model files into the HF cache (Slice 4). Returns
/// immediately with a `job_id`; poll `voice.download_status` for
/// progress. Replaces `Voice/download_models.py` — no Python.
pub async fn handle_download_models(_payload: Value) -> Reply {
    let cfg = Config::get();
    let job_id = crate::model_download::spawn_ensure_job();
    Reply::ok(json!({
        "job_id": job_id,
        "stt_model": cfg.stt_model.clone(),
        "kokoro_model": KOKORO_REPO,
    }))
}

/// `voice.download_status` — poll the in-progress / done / failed status
/// of a previously-issued `voice.download_models` job. Payload:
/// `{job_id}`. Reply mirrors the wake-word pull-status shape.
pub async fn handle_download_status(payload: Value) -> Reply {
    let job_id = match payload.get("job_id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => return Reply::err(invalid_request("job_id is required")),
    };
    match EnsureJobs::global().status(&job_id) {
        Some(EnsureStatus::InProgress { done, total }) => Reply::ok(json!({
            "job_id": job_id,
            "state": "in_progress",
            "done": done,
            "total": total,
        })),
        Some(EnsureStatus::Done {
            whisper_dir,
            kokoro_dir,
        }) => Reply::ok(json!({
            "job_id": job_id,
            "state": "done",
            "whisper_dir": whisper_dir.display().to_string(),
            "kokoro_dir": kokoro_dir.display().to_string(),
        })),
        Some(EnsureStatus::Failed { error }) => Reply::ok(json!({
            "job_id": job_id,
            "state": "failed",
            "error": error,
        })),
        None => Reply::err(IpcError::new(
            "unknown_job",
            format!("no download job tracked for id {job_id}"),
        )),
    }
}

/// Inline Kokoro voice catalogue, copied from
/// `Voice/synthesize.py::KOKORO_VOICES`. Static — switching voices is
/// free at runtime (one model + voices.npz drives all 28).
fn kokoro_voice_catalogue() -> Value {
    json!([
        {"name": "af_heart", "lang": "en-us", "gender": "female"},
        {"name": "af_alloy", "lang": "en-us", "gender": "female"},
        {"name": "af_aoede", "lang": "en-us", "gender": "female"},
        {"name": "af_bella", "lang": "en-us", "gender": "female"},
        {"name": "af_jessica", "lang": "en-us", "gender": "female"},
        {"name": "af_kore", "lang": "en-us", "gender": "female"},
        {"name": "af_nicole", "lang": "en-us", "gender": "female"},
        {"name": "af_nova", "lang": "en-us", "gender": "female"},
        {"name": "af_river", "lang": "en-us", "gender": "female"},
        {"name": "af_sarah", "lang": "en-us", "gender": "female"},
        {"name": "af_sky", "lang": "en-us", "gender": "female"},
        {"name": "am_adam", "lang": "en-us", "gender": "male"},
        {"name": "am_echo", "lang": "en-us", "gender": "male"},
        {"name": "am_eric", "lang": "en-us", "gender": "male"},
        {"name": "am_fenrir", "lang": "en-us", "gender": "male"},
        {"name": "am_liam", "lang": "en-us", "gender": "male"},
        {"name": "am_michael", "lang": "en-us", "gender": "male"},
        {"name": "am_onyx", "lang": "en-us", "gender": "male"},
        {"name": "am_puck", "lang": "en-us", "gender": "male"},
        {"name": "am_santa", "lang": "en-us", "gender": "male"},
        {"name": "bf_alice", "lang": "en-gb", "gender": "female"},
        {"name": "bf_emma", "lang": "en-gb", "gender": "female"},
        {"name": "bf_isabella", "lang": "en-gb", "gender": "female"},
        {"name": "bf_lily", "lang": "en-gb", "gender": "female"},
        {"name": "bm_daniel", "lang": "en-gb", "gender": "male"},
        {"name": "bm_fable", "lang": "en-gb", "gender": "male"},
        {"name": "bm_george", "lang": "en-gb", "gender": "male"},
        {"name": "bm_lewis", "lang": "en-gb", "gender": "male"}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_name_translates_slashes() {
        assert_eq!(
            cache_dir_name("openai/whisper-small"),
            "models--openai--whisper-small"
        );
    }

    #[tokio::test]
    async fn list_models_includes_configured_backend_and_active_model() {
        let r = handle_list_models(json!({})).await;
        assert!(r.ok);
        assert!(r.data["stt"]["active_backend"].is_string());
        assert!(r.data["stt"]["active_model"].is_string());
        assert!(r.data["stt"]["models"].is_array());
        // Kokoro voice catalogue should always be present.
        let voices = r.data["tts"]["model"]["voices"].as_array().unwrap();
        assert_eq!(voices.len(), 28);
        // hf_cache_root reported so debugging "where did it look" is one-look.
        assert!(r.data["hf_cache_root"].is_string());
    }

    #[tokio::test]
    async fn missing_models_report_installed_false_with_zero_bytes() {
        // Confidently absent repo id.
        let v = probe_repo("never-going-to-exist/wylde-test-fake-model");
        assert_eq!(v["installed"], false);
        assert_eq!(v["bytes_on_disk"], 0);
        assert!(v["snapshot_path"].is_null());
        // Tolerant: probe should not panic; OV flags are bool.
        assert!(v["ov_export_present"].is_boolean());
        assert!(v["ov_npu_static_present"].is_boolean());
    }
}
