//! Scanner for locally-installed openWakeWord bundles. Slice 11.E+.
//!
//! openWakeWord ships ONNX models outside the HF cache — the Wylde user's Voice
//! service drops them under `<wakeword_models_dir>/<vendor>/<name>/`
//! where each directory holds the three-ONNX bundle (`melspectrogram`,
//! `embedding_model`, and the per-wakeword classifier). The HF scanner
//! doesn't see these because they don't live under `models--<org>--*/`,
//! so the unified registry needs its own walker.
//!
//! ## Layout
//!
//! Bundles live under the tree configured by Voice's
//! [`Config::wakeword_models_dir`] (default
//! `%LOCALAPPDATA%\Wylde\voice\wakeword`). Within that root:
//!
//! ```text
//! wakeword/
//!   openWakeWord/
//!     hey-jarvis/
//!       melspectrogram.onnx
//!       embedding_model.onnx
//!       hey_jarvis.onnx
//! ```
//!
//! The two-level `<vendor>/<name>` shape mirrors the HF cache id
//! convention (`<org>/<repo>`); a bundle id is composed back as
//! `<vendor>/<name>`. The classifier file's basename is the bundle's
//! "tag" (`hey_jarvis` here) — most callers don't care, but it's
//! returned in the metadata so the GUI can show the file users dropped.

use std::path::{Path, PathBuf};

use crate::model_registry::types::{default_chat_visible, Kind, ModelEntry};

/// Filenames required for a bundle to count as installed. The classifier
/// is the third ONNX, named after the wake word itself — we don't pin
/// that name; the [`scan`] function picks the lone non-base `*.onnx` in
/// each bundle dir as the classifier.
const BUNDLE_BASE_FILES: &[&str] = &["melspectrogram.onnx", "embedding_model.onnx"];

/// Resolve the wake-word models tree. Mirrors
/// [`wylde_voice::config::Config::wakeword_models_dir`] so the scanner
/// sees the same root the Voice service writes into.
///
/// Lookup order:
///   1. `WYLDE_VOICE_WAKEWORD_MODELS_DIR` (explicit override).
///   2. `%LOCALAPPDATA%\Wylde\voice\wakeword` on Windows.
///   3. `<WYLDE_ROOT>/cache/voice/wakeword` everywhere else.
pub fn wakeword_root() -> PathBuf {
    if let Some(v) = std::env::var_os("WYLDE_VOICE_WAKEWORD_MODELS_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local)
                .join("Wylde")
                .join("voice")
                .join("wakeword");
        }
    }
    let wylde_root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    wylde_root.join("cache").join("voice").join("wakeword")
}

/// Walk [`wakeword_root`] and emit one [`ModelEntry`] per bundle that
/// has both base ONNX files plus at least one classifier. Returns an
/// empty list when the root doesn't exist; this is the cold-start state
/// before [`download_bundle`] runs.
pub fn scan() -> Vec<ModelEntry> {
    let root = wakeword_root();
    scan_at(&root)
}

/// Scanner variant scoped to an explicit root — used by tests to walk
/// a tempdir without touching the user's real `wakeword_models_dir`.
pub fn scan_at(root: &Path) -> Vec<ModelEntry> {
    let mut entries: Vec<ModelEntry> = Vec::new();
    if !root.is_dir() {
        return entries;
    }
    let Ok(vendor_iter) = std::fs::read_dir(root) else {
        return entries;
    };
    for vendor_entry in vendor_iter.flatten() {
        let Ok(file_type) = vendor_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let vendor_name = match vendor_entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let vendor_path = vendor_entry.path();
        let Ok(model_iter) = std::fs::read_dir(&vendor_path) else {
            continue;
        };
        for model_entry in model_iter.flatten() {
            let Ok(ft) = model_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let model_name = match model_entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let bundle_path = model_entry.path();
            if let Some(entry) = build_entry(&vendor_name, &model_name, &bundle_path) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Quick "is this bundle ready" check — same predicate
/// [`voice.check_wake_word_model`] returns. Cheaper than [`scan`] when
/// the caller already knows the model name they care about.
pub fn is_installed(model_id: &str) -> bool {
    let Some((vendor, name)) = split_model_id(model_id) else {
        return false;
    };
    let root = wakeword_root();
    let bundle = root.join(vendor).join(name);
    bundle_has_base_files(&bundle) && bundle_has_classifier(&bundle)
}

fn build_entry(vendor: &str, name: &str, bundle_path: &Path) -> Option<ModelEntry> {
    if !bundle_has_base_files(bundle_path) {
        return None;
    }
    if !bundle_has_classifier(bundle_path) {
        return None;
    }
    let id = format!("{vendor}/{name}");
    let size = dir_size_bytes(bundle_path);
    Some(ModelEntry {
        id,
        kind: Kind::Wakeword,
        path: Some(bundle_path.to_string_lossy().into_owned()),
        size_bytes: size,
        loaded: false,
        provider: "local".to_owned(),
        required_by: Vec::new(),
        profile: None,
        last_accessed: None,
        chat_visible: default_chat_visible(Kind::Wakeword),
    })
}

fn bundle_has_base_files(bundle: &Path) -> bool {
    BUNDLE_BASE_FILES.iter().all(|f| bundle.join(f).is_file())
}

fn bundle_has_classifier(bundle: &Path) -> bool {
    let Ok(it) = std::fs::read_dir(bundle) else {
        return false;
    };
    for entry in it.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.ends_with(".onnx") {
            continue;
        }
        if BUNDLE_BASE_FILES.contains(&name_str) {
            continue;
        }
        return true;
    }
    false
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let Ok(it) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in it.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Split `vendor/name` into its parts. Returns `None` on any string
/// that isn't exactly one `/`-separated pair.
pub(crate) fn split_model_id(model_id: &str) -> Option<(&str, &str)> {
    let mut parts = model_id.split('/');
    let vendor = parts.next()?;
    let name = parts.next()?;
    if vendor.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((vendor, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    use crate::memory::common::TEST_ENV_LOCK;

    struct WakewordEnv {
        _guard: MutexGuard<'static, ()>,
        td: TempDir,
        prior_dir: Option<std::ffi::OsString>,
    }

    impl WakewordEnv {
        fn new() -> Self {
            let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().expect("tempdir");
            let prior_dir = std::env::var_os("WYLDE_VOICE_WAKEWORD_MODELS_DIR");
            std::env::set_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR", td.path());
            Self {
                _guard: guard,
                td,
                prior_dir,
            }
        }

        fn root(&self) -> &Path {
            self.td.path()
        }
    }

    impl Drop for WakewordEnv {
        fn drop(&mut self) {
            match self.prior_dir.take() {
                Some(v) => std::env::set_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR", v),
                None => std::env::remove_var("WYLDE_VOICE_WAKEWORD_MODELS_DIR"),
            }
        }
    }

    fn write_onnx(root: &Path, rel: &str, body: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn write_bundle(root: &Path, vendor: &str, name: &str, classifier_file: &str) {
        let dir = root.join(vendor).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for base in BUNDLE_BASE_FILES {
            std::fs::write(dir.join(base), b"abc").unwrap();
        }
        std::fs::write(dir.join(classifier_file), b"defgh").unwrap();
    }

    #[test]
    fn scan_empty_root_returns_empty() {
        let env = WakewordEnv::new();
        let _ = env;
        let out = scan();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_skips_bundle_missing_base_files() {
        let env = WakewordEnv::new();
        // Drop only the classifier in place — no base files. Should be ignored.
        write_onnx(env.root(), "openWakeWord/hey-jarvis/hey_jarvis.onnx", b"x");
        let out = scan();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_skips_bundle_missing_classifier() {
        let env = WakewordEnv::new();
        // Both base files but no classifier.
        let dir = env.root().join("openWakeWord/hey-jarvis");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("melspectrogram.onnx"), b"x").unwrap();
        std::fs::write(dir.join("embedding_model.onnx"), b"y").unwrap();
        let out = scan();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_finds_complete_bundle() {
        let env = WakewordEnv::new();
        write_bundle(env.root(), "openWakeWord", "hey-jarvis", "hey_jarvis.onnx");
        let out = scan();
        assert_eq!(out.len(), 1);
        let entry = &out[0];
        assert_eq!(entry.id, "openWakeWord/hey-jarvis");
        assert_eq!(entry.kind, Kind::Wakeword);
        assert_eq!(entry.provider, "local");
        assert!(entry.path.is_some());
        // Three files at 3 + 3 + 5 bytes.
        assert_eq!(entry.size_bytes, 11);
    }

    #[test]
    fn scan_sorts_by_id() {
        let env = WakewordEnv::new();
        write_bundle(env.root(), "vendor-b", "modelB", "cls.onnx");
        write_bundle(env.root(), "vendor-a", "modelA", "cls.onnx");
        let out = scan();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "vendor-a/modelA");
        assert_eq!(out[1].id, "vendor-b/modelB");
    }

    #[test]
    fn is_installed_checks_bundle_files() {
        let env = WakewordEnv::new();
        write_bundle(env.root(), "openWakeWord", "hey-jarvis", "hey_jarvis.onnx");
        assert!(is_installed("openWakeWord/hey-jarvis"));
        assert!(!is_installed("openWakeWord/missing"));
        assert!(!is_installed("malformed"));
        assert!(!is_installed(""));
    }

    #[test]
    fn split_model_id_accepts_exactly_one_slash() {
        assert_eq!(split_model_id("a/b"), Some(("a", "b")));
        assert_eq!(split_model_id("a"), None);
        assert_eq!(split_model_id("a/b/c"), None);
        assert_eq!(split_model_id("/b"), None);
        assert_eq!(split_model_id("a/"), None);
    }

    #[test]
    fn wakeword_root_respects_env() {
        let env = WakewordEnv::new();
        assert_eq!(wakeword_root(), env.root());
    }
}
