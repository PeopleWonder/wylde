//! Public registry API. Rust port of `model_registry/__init__.py`.
//!
//! `list_models` merges three sources, later wins for duplicate ids on
//! overlapping fields:
//!
//! 1. **HF cache scanner** — `path`, `size_bytes`, `last_accessed`,
//!    `provider="huggingface"`.
//! 2. **Ollama tags** — fills `loaded` for matching ids, contributes
//!    Ollama-only ids with `provider="ollama"`.
//! 3. **Routing profiles** — `profile` block on LLM-kind entries.
//!
//! The Ollama tags + profile merge requires an IPC client. Tests pass a
//! synthetic [`OllamaProbe`] so the registry's merge logic can be
//! exercised without a live wylde-ollama. The default
//! [`live_ollama_probe`] talks to wylde-ollama via the shared IPC
//! transport.

use std::collections::HashMap;

use serde_json::Value;

use crate::model_registry::hf_scanner::{invalidate_cache as hf_invalidate, scan_hf_cache};
use crate::model_registry::routing::profiles::list_profiles;
use crate::model_registry::service_manifests::{
    invalidate_cache as manifests_invalidate, load_declarations,
};
use crate::model_registry::types::{default_chat_visible, Kind, ModelEntry};
use crate::model_registry::wakeword_scanner::scan as scan_wakeword_bundles;

/// Trait abstracting "where do I get the list of Ollama-loaded models"
/// so tests don't need to spin up wylde-ollama.
pub trait OllamaProbe: Send + Sync {
    /// Return the model names currently visible to Ollama
    /// (`/api/tags` → `models[].name`). Empty list when Ollama is
    /// unreachable — mirrors Python's `try/except` shape.
    fn list_models(&self) -> Vec<String>;
}

/// Stub probe used when no probe is supplied. Returns an empty list so
/// the merged view falls back to HF cache + manifests only.
pub struct NullProbe;

impl OllamaProbe for NullProbe {
    fn list_models(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Probe that calls `ollama.list_models` on wylde-ollama via the shared
/// IPC primitive. Each call is unary, short timeout.
///
/// Available so unit tests can pass [`NullProbe`] without pulling in a
/// tokio runtime.
pub fn live_ollama_probe() -> LivePipeProbe {
    LivePipeProbe
}

pub struct LivePipeProbe;

impl OllamaProbe for LivePipeProbe {
    fn list_models(&self) -> Vec<String> {
        // The IPC primitive is async-only. The registry-side `list_models`
        // is sync; bridge via tokio's `block_in_place` only when a runtime
        // is present. The Python path also runs synchronously and tolerates
        // an Ollama outage by returning an empty list — we mirror that.
        let Some(handle) = current_runtime_handle() else {
            return Vec::new();
        };
        let result = std::thread::scope(|scope| {
            let h = scope
                .spawn(|| {
                    handle.block_on(async {
                        let reply = wylde_shared::ipc::send_action(
                            "wylde-ollama",
                            "ollama.list_models",
                            serde_json::json!({}),
                        )
                        .await;
                        if !reply.ok {
                            return Vec::<String>::new();
                        }
                        reply
                            .data
                            .get("models")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|m| {
                                        m.get("name").and_then(Value::as_str).map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                })
                .join();
            h.unwrap_or_default()
        });
        result
    }
}

fn current_runtime_handle() -> Option<tokio::runtime::Handle> {
    tokio::runtime::Handle::try_current().ok()
}

/// Return every known model, optionally filtered by `kind`. Matches
/// Python's `list_models(kind=None)` semantics.
///
/// * `kind` — `None` returns every entry; `Some(kind)` returns only that
///   bucket. The filter is applied after the merge so an Ollama-only id
///   whose service-manifest declared an STT kind still surfaces under
///   `Some(Kind::Stt)`.
/// * `probe` — Ollama-loaded-model source. Pass [`NullProbe`] in tests
///   that don't care about the merge; pass [`LivePipeProbe`] (or the
///   convenience helper [`live_ollama_probe`]) in production.
/// * `profiles` — caller-supplied LLM routing profiles. Pass `None` to
///   read them from disk; `Some(...)` injects a deterministic view for
///   tests.
pub fn list_models<P: OllamaProbe>(
    kind: Option<Kind>,
    probe: &P,
    profiles: Option<Vec<Value>>,
) -> Vec<ModelEntry> {
    let (overrides, required_by) = load_declarations(false);
    let scanned = scan_hf_cache(&overrides, &required_by, false);
    let mut by_id: HashMap<String, ModelEntry> =
        scanned.into_iter().map(|e| (e.id.clone(), e)).collect();

    // Wake-word bundles live outside the HF cache (Voice's
    // `wakeword_models_dir`) — Slice 11.E+. Merge after the HF scan so
    // a manifest entry that names the same id can still flip `kind`.
    for mut wake in scan_wakeword_bundles() {
        if let Some(kind_override) = overrides.get(&wake.id).copied() {
            wake.kind = kind_override;
            wake.chat_visible = default_chat_visible(kind_override);
        }
        if let Some(rb) = required_by.get(&wake.id) {
            wake.required_by = rb.clone();
        }
        by_id.entry(wake.id.clone()).or_insert(wake);
    }

    // Merge in Ollama-loaded models so the inference bar sees what's resident.
    for name in probe.list_models() {
        if let Some(existing) = by_id.get_mut(&name) {
            existing.loaded = true;
            if existing.provider == "huggingface" {
                existing.provider = "ollama".to_owned();
            }
            continue;
        }
        // Pure-Ollama model: no HF cache footprint. Service manifests
        // may still claim it; honour that, otherwise call it an LLM
        // (the only kind Ollama hosts today).
        let kind_for_ollama = overrides.get(&name).copied().unwrap_or(Kind::Llm);
        by_id.insert(
            name.clone(),
            ModelEntry {
                id: name.clone(),
                kind: kind_for_ollama,
                path: None,
                size_bytes: 0,
                loaded: true,
                provider: "ollama".to_owned(),
                required_by: required_by.get(&name).cloned().unwrap_or_default(),
                profile: None,
                last_accessed: None,
                chat_visible: default_chat_visible(kind_for_ollama),
            },
        );
    }

    // Attach routing profiles to LLM-kind entries.
    let profile_dicts = profiles.unwrap_or_else(list_profiles);
    let profiles_by_name: HashMap<String, Value> = profile_dicts
        .into_iter()
        .filter_map(|p| {
            p.get("name")
                .and_then(Value::as_str)
                .map(|n| (n.to_owned(), p.clone()))
        })
        .collect();
    for entry in by_id.values_mut() {
        if entry.kind != Kind::Llm {
            continue;
        }
        if let Some(p) = profiles_by_name.get(&entry.id) {
            entry.profile = Some(p.clone());
        }
    }

    let mut entries: Vec<ModelEntry> = by_id.into_values().collect();
    entries.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.id.cmp(&b.id))
    });
    match kind {
        Some(k) => entries.into_iter().filter(|e| e.kind == k).collect(),
        None => entries,
    }
}

/// Look up one entry by id. Returns `None` if no source knows it.
/// Mirrors Python's `get_model(model_id)`.
pub fn get_model<P: OllamaProbe>(model_id: &str, probe: &P) -> Option<ModelEntry> {
    if model_id.is_empty() {
        return None;
    }
    list_models(None, probe, None)
        .into_iter()
        .find(|e| e.id == model_id)
}

/// Whether the model is currently resident in its inference engine.
/// For LLMs this means "the local Ollama daemon has it loaded right
/// now". Non-LLM kinds always return `false` until per-kind probes are
/// wired in — matches Python's `is_loaded(model_id)`.
pub fn is_loaded<P: OllamaProbe>(model_id: &str, probe: &P) -> bool {
    get_model(model_id, probe)
        .map(|e| e.loaded)
        .unwrap_or(false)
}

/// Drop the HF-cache mtime cache and the service-manifest cache. The
/// routing layer's profile store is unaffected (it's the source of
/// truth for benchmarks; we don't want to lose it on a refresh).
pub fn refresh_cache() {
    hf_invalidate();
    manifests_invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::common::TEST_ENV_LOCK;
    use crate::model_registry::hf_scanner::hub_root;
    use std::path::Path;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    /// Combined HF + WYLDE_ROOT + MODEL_DATA_DIR sandbox so the
    /// scanner, manifests, and routing files all live under a fresh
    /// tempdir.
    struct Sandbox {
        _guard: MutexGuard<'static, ()>,
        td: TempDir,
        prior_hf: Option<std::ffi::OsString>,
        prior_root: Option<std::ffi::OsString>,
        prior_services: Option<std::ffi::OsString>,
        prior_data: Option<std::ffi::OsString>,
        prior_wakeword: Option<std::ffi::OsString>,
    }

    impl Sandbox {
        fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior_hf = std::env::var_os("HF_HUB_CACHE");
            let prior_root = std::env::var_os("WYLDE_ROOT");
            // A dev machine points WYLDE_SERVICES at its live install; the
            // `Services/` discovery honours it, so clear it to keep the scan
            // inside the tempdir (#125).
            let prior_services = std::env::var_os("WYLDE_SERVICES");
            let prior_data = std::env::var_os("MODEL_DATA_DIR");
            let prior_wakeword = std::env::var_os("WYLDE_VOICE_WAKEWORD_MODELS_DIR");
            let hub = td.path().join("hub");
            std::fs::create_dir_all(&hub).unwrap();
            std::env::set_var("HF_HUB_CACHE", &hub);
            std::env::set_var("WYLDE_ROOT", td.path());
            std::env::remove_var("WYLDE_SERVICES");
            std::env::set_var("MODEL_DATA_DIR", td.path().join("routing"));
            // Pin the wake-word scanner's root inside the sandbox so it
            // doesn't pick up the developer's real LOCALAPPDATA bundle.
            std::env::set_var(
                "WYLDE_VOICE_WAKEWORD_MODELS_DIR",
                td.path().join("wakeword"),
            );
            std::env::remove_var("HUGGINGFACE_HUB_CACHE");
            std::env::remove_var("HF_HOME");
            refresh_cache();
            Self {
                _guard: guard,
                td,
                prior_hf,
                prior_root,
                prior_services,
                prior_data,
                prior_wakeword,
            }
        }

        fn hub(&self) -> std::path::PathBuf {
            self.td.path().join("hub")
        }

        fn root(&self) -> &Path {
            self.td.path()
        }

        fn wakeword_root(&self) -> std::path::PathBuf {
            self.td.path().join("wakeword")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            refresh_cache();
            match self.prior_hf.take() {
                Some(v) => std::env::set_var("HF_HUB_CACHE", v),
                None => std::env::remove_var("HF_HUB_CACHE"),
            }
            match self.prior_root.take() {
                Some(v) => std::env::set_var("WYLDE_ROOT", v),
                None => std::env::remove_var("WYLDE_ROOT"),
            }
            match self.prior_services.take() {
                Some(v) => std::env::set_var("WYLDE_SERVICES", v),
                None => std::env::remove_var("WYLDE_SERVICES"),
            }
            match self.prior_data.take() {
                Some(v) => std::env::set_var("MODEL_DATA_DIR", v),
                None => std::env::remove_var("MODEL_DATA_DIR"),
            }
            match self.prior_wakeword.take() {
                Some(v) => std::env::set_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR", v),
                None => std::env::remove_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR"),
            }
        }
    }

    fn make_hf_model(hub: &Path, folder: &str, bytes: &[u8]) {
        let dir = hub.join(folder);
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("data.bin"), bytes).unwrap();
    }

    fn write_service_manifest(root: &Path, service: &str, body: serde_json::Value) {
        // The model registry discovers service manifests in the `Services/`
        // bucket (`wylde_stack::roster::discovered_folders`, #125), so the
        // fixture drops the service there rather than at the top level.
        let path = root.join("Services").join(service).join("manifest.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    }

    struct FakeProbe(Vec<String>);
    impl OllamaProbe for FakeProbe {
        fn list_models(&self) -> Vec<String> {
            self.0.clone()
        }
    }

    #[test]
    fn list_models_empty_when_no_sources() {
        let sb = Sandbox::new();
        assert_eq!(hub_root(), sb.hub());
        let out = list_models(None, &NullProbe, Some(Vec::new()));
        assert!(out.is_empty());
    }

    #[test]
    fn list_models_filters_by_kind() {
        let sb = Sandbox::new();
        make_hf_model(&sb.hub(), "models--openai--whisper-small", b"a");
        make_hf_model(&sb.hub(), "models--meta-llama--Llama-3.1-8B-Instruct", b"b");
        let stt = list_models(Some(Kind::Stt), &NullProbe, Some(Vec::new()));
        assert_eq!(stt.len(), 1);
        assert_eq!(stt[0].id, "openai/whisper-small");
        assert_eq!(stt[0].kind, Kind::Stt);
        let llm = list_models(Some(Kind::Llm), &NullProbe, Some(Vec::new()));
        assert_eq!(llm.len(), 1);
        assert_eq!(llm[0].kind, Kind::Llm);
    }

    #[test]
    fn ollama_probe_marks_existing_hf_model_loaded() {
        let sb = Sandbox::new();
        make_hf_model(&sb.hub(), "models--meta-llama--Llama-3.1-8B-Instruct", b"a");
        let probe = FakeProbe(vec!["meta-llama/Llama-3.1-8B-Instruct".to_owned()]);
        let out = list_models(None, &probe, Some(Vec::new()));
        assert_eq!(out.len(), 1);
        assert!(out[0].loaded);
        assert_eq!(out[0].provider, "ollama");
    }

    #[test]
    fn ollama_only_model_added_with_provider_ollama() {
        let sb = Sandbox::new();
        let _ = sb;
        let probe = FakeProbe(vec!["qwen2.5:0.5b".to_owned()]);
        let out = list_models(None, &probe, Some(Vec::new()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "qwen2.5:0.5b");
        assert_eq!(out[0].provider, "ollama");
        assert!(out[0].loaded);
        assert_eq!(out[0].kind, Kind::Llm);
        assert!(out[0].chat_visible);
    }

    #[test]
    fn service_manifest_override_wins_for_ollama_only_model() {
        let sb = Sandbox::new();
        write_service_manifest(
            sb.root(),
            "Voice",
            serde_json::json!({
                "name": "voice",
                "models": [{"id": "whisper-special", "kind": "stt", "required": true}],
            }),
        );
        let probe = FakeProbe(vec!["whisper-special".to_owned()]);
        let out = list_models(None, &probe, Some(Vec::new()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, Kind::Stt);
        assert!(!out[0].chat_visible);
        assert_eq!(out[0].required_by, vec!["voice".to_owned()]);
    }

    #[test]
    fn profiles_attach_to_llm_entries_only() {
        let sb = Sandbox::new();
        make_hf_model(&sb.hub(), "models--meta-llama--Llama-3.1-8B-Instruct", b"a");
        make_hf_model(&sb.hub(), "models--openai--whisper-small", b"b");
        let profiles = vec![
            serde_json::json!({"name": "meta-llama/Llama-3.1-8B-Instruct", "status": "active"}),
            serde_json::json!({"name": "openai/whisper-small", "status": "active"}),
        ];
        let out = list_models(None, &NullProbe, Some(profiles));
        let llm = out.iter().find(|e| e.kind == Kind::Llm).unwrap();
        let stt = out.iter().find(|e| e.kind == Kind::Stt).unwrap();
        assert!(llm.profile.is_some());
        assert!(stt.profile.is_none());
    }

    #[test]
    fn get_model_finds_by_id() {
        let sb = Sandbox::new();
        make_hf_model(&sb.hub(), "models--openai--whisper-small", b"a");
        let entry = get_model("openai/whisper-small", &NullProbe).unwrap();
        assert_eq!(entry.kind, Kind::Stt);
        assert!(get_model("missing", &NullProbe).is_none());
        assert!(get_model("", &NullProbe).is_none());
    }

    #[test]
    fn is_loaded_reflects_ollama_probe() {
        let sb = Sandbox::new();
        let _ = sb;
        let probe = FakeProbe(vec!["qwen2.5:0.5b".to_owned()]);
        assert!(is_loaded("qwen2.5:0.5b", &probe));
        assert!(!is_loaded("ghost", &probe));
    }

    fn write_wakeword_bundle(root: &Path, vendor: &str, name: &str, classifier: &str) {
        let dir = root.join(vendor).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("melspectrogram.onnx"), b"abc").unwrap();
        std::fs::write(dir.join("embedding_model.onnx"), b"def").unwrap();
        std::fs::write(dir.join(classifier), b"ghij").unwrap();
    }

    #[test]
    fn list_models_includes_wakeword_bundles() {
        let sb = Sandbox::new();
        write_wakeword_bundle(
            &sb.wakeword_root(),
            "openWakeWord",
            "hey-jarvis",
            "hey_jarvis.onnx",
        );
        let out = list_models(Some(Kind::Wakeword), &NullProbe, Some(Vec::new()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "openWakeWord/hey-jarvis");
        assert_eq!(out[0].kind, Kind::Wakeword);
        assert_eq!(out[0].provider, "local");
        assert!(!out[0].chat_visible);
    }

    #[test]
    fn list_models_wakeword_kind_visible_when_filtering() {
        let sb = Sandbox::new();
        write_wakeword_bundle(&sb.wakeword_root(), "openWakeWord", "alexa", "alexa.onnx");
        make_hf_model(&sb.hub(), "models--openai--whisper-small", b"a");
        let all = list_models(None, &NullProbe, Some(Vec::new()));
        assert_eq!(all.len(), 2, "wakeword + whisper merged into one list");
        let wake = list_models(Some(Kind::Wakeword), &NullProbe, Some(Vec::new()));
        assert_eq!(wake.len(), 1);
        assert_eq!(wake[0].id, "openWakeWord/alexa");
    }

    #[test]
    fn results_sorted_by_kind_then_id() {
        let sb = Sandbox::new();
        make_hf_model(&sb.hub(), "models--zeta--llm-a", b"a"); // llm
        make_hf_model(&sb.hub(), "models--alpha--llm-b", b"b"); // llm
        make_hf_model(&sb.hub(), "models--openai--whisper-small", b"c"); // stt
        let out = list_models(None, &NullProbe, Some(Vec::new()));
        assert_eq!(out.len(), 3);
        // llm comes before stt alphabetically.
        assert_eq!(out[0].kind, Kind::Llm);
        assert_eq!(out[1].kind, Kind::Llm);
        assert_eq!(out[2].kind, Kind::Stt);
        // Within llm: alphabetical by id.
        assert!(out[0].id < out[1].id);
    }
}
