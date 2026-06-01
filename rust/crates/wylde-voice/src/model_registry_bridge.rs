//! Tiny model-registry probe used by `voice.check_wake_word_model`.
//!
//! The unified registry lives in `wylde-harness`. We don't depend on
//! that crate from `wylde-voice` (different services, different lifetimes,
//! avoids a build-time edge). Instead we reproduce the wake-word
//! "is this bundle installed" predicate here — same layout the harness
//! scanner walks (`<wakeword_models_dir>/<vendor>/<name>/`), same files.
//!
//! The two implementations stay in sync because both consult
//! [`crate::config::Config::wakeword_models_dir`] for the root and both
//! use the same constant set of base files.

use std::path::{Path, PathBuf};

use crate::config::Config;

const BUNDLE_BASE_FILES: &[&str] = &["melspectrogram.onnx", "embedding_model.onnx"];

/// Is the named openWakeWord bundle ready to load?
///
/// Returns `true` when every required ONNX file is present under
/// `<wakeword_models_dir>/<vendor>/<name>/`. Conservative — any missing
/// file (or unparseable id) returns `false`.
pub fn is_wakeword_installed(model_id: &str) -> bool {
    let Some((vendor, name)) = split_model_id(model_id) else {
        return false;
    };
    let root: PathBuf = Config::get().wakeword_models_dir.clone();
    let bundle = root.join(vendor).join(name);
    bundle_has_required_files(&bundle)
}

fn bundle_has_required_files(bundle: &Path) -> bool {
    if !bundle.is_dir() {
        return false;
    }
    for base in BUNDLE_BASE_FILES {
        if !bundle.join(base).is_file() {
            return false;
        }
    }
    // Plus at least one non-base classifier ONNX.
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

fn split_model_id(model_id: &str) -> Option<(&str, &str)> {
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

    #[test]
    fn split_model_id_basics() {
        assert_eq!(split_model_id("a/b"), Some(("a", "b")));
        assert_eq!(split_model_id("missing-slash"), None);
        assert_eq!(split_model_id(""), None);
        assert_eq!(split_model_id("a/b/c"), None);
    }

    #[test]
    fn bundle_predicate_rejects_missing_dir() {
        let td = tempfile::TempDir::new().unwrap();
        assert!(!bundle_has_required_files(&td.path().join("nope")));
    }

    #[test]
    fn bundle_predicate_requires_three_onnx_files() {
        let td = tempfile::TempDir::new().unwrap();
        let bundle = td.path().join("openWakeWord/hey-jarvis");
        std::fs::create_dir_all(&bundle).unwrap();
        assert!(!bundle_has_required_files(&bundle));
        std::fs::write(bundle.join("melspectrogram.onnx"), b"x").unwrap();
        assert!(!bundle_has_required_files(&bundle));
        std::fs::write(bundle.join("embedding_model.onnx"), b"x").unwrap();
        // Still missing classifier.
        assert!(!bundle_has_required_files(&bundle));
        std::fs::write(bundle.join("hey_jarvis.onnx"), b"x").unwrap();
        assert!(bundle_has_required_files(&bundle));
    }
}
