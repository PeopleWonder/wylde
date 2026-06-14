//! Shared "download an Ollama model" helper for the GUI panels.
//!
//! Two concerns live here so every surface that hits a "model not
//! installed" error can offer an inline **Download <model>** button
//! instead of sending the user to a terminal:
//!
//!   * [`parse_pullable_model`] — pull the model name out of the backend's
//!     `EmbedError::ModelMissing` string (and the generic Ollama
//!     `model_not_found` shape) so the button never hardcodes a name.
//!   * [`pull_model`] + [`PullProgress`] — open the streaming `ollama.pull`
//!     verb (which wraps Ollama's `/api/pull` NDJSON progress) and project
//!     each frame to the fields a progress bar reads.
//!
//! The Models panel grew its own copy of the progress projection first;
//! this is the shared home so the RAG/index error surface (and any future
//! one) reuses the same wire handling rather than re-deriving it.

use serde_json::{json, Value};

use crate::PipeStream;

/// The Ollama wrapper's pipe name. `ollama.pull` is registered there.
const SVC_OLLAMA: &str = "wylde-ollama";

/// Extract a pullable model name from a backend error string.
///
/// Handles the two shapes the embed/index path produces:
///   * `EmbedError::ModelMissing` —
///     `backend has no model named "X" — pull it with: ollama pull X (…)`
///   * the generic Ollama `model_not_found` — `model "X" not installed in Ollama`
///
/// The `ollama pull <name>` remediation hint is the most reliable anchor
/// (it is exactly the token the user would type), so we prefer it and fall
/// back to the quoted name after `no model named` / `model`. Returns
/// `None` when the error isn't a missing-model error — callers use that to
/// decide whether to show the Download button at all.
pub fn parse_pullable_model(err: &str) -> Option<String> {
    // 1. `ollama pull <name>` — take the token right after it.
    if let Some(rest) = err.split("ollama pull ").nth(1) {
        if let Some(tok) = first_model_token(rest) {
            return Some(tok);
        }
    }
    // 2. `no model named "<name>"` / `model "<name>" not installed`.
    for anchor in ["no model named", "model"] {
        if let Some(rest) = err.split(anchor).nth(1) {
            if let Some(name) = first_quoted(rest) {
                // Guard against matching the literal word "model" inside an
                // unrelated sentence: only accept when the wider error looks
                // like a missing-model error.
                if err.contains("not installed") || err.contains("no model named") {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// First whitespace/paren-delimited token, trimmed of trailing punctuation
/// the error sentence may append (e.g. a closing quote or period).
fn first_model_token(s: &str) -> Option<String> {
    let tok = s
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find(|t| !t.is_empty())?;
    let tok = tok.trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == ',');
    (!tok.is_empty()).then(|| tok.to_owned())
}

/// First double-quoted substring in `s`, if any.
fn first_quoted(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let rest = &s[start..];
    let end = rest.find('"')?;
    let name = &rest[..end];
    (!name.is_empty()).then(|| name.to_owned())
}

/// One chunk of the `ollama.pull` NDJSON stream, projected to the fields a
/// progress indicator reads. `status` is always present; `completed` /
/// `total` appear on the per-layer download chunks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PullProgress {
    pub status: String,
    pub completed: u64,
    pub total: u64,
    /// Per-layer digest — kept so a later slice can show "layer N of M"
    /// without re-shaping the projection.
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

    /// `true` when the stream signalled completion (`{"status":"success"}`).
    pub fn is_success(&self) -> bool {
        self.status.eq_ignore_ascii_case("success")
    }

    /// `0.0..=1.0` fraction complete, or `None` while no `total` is known
    /// yet (manifest/metadata frames) — render an indeterminate spinner.
    pub fn ratio(&self) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        Some((self.completed as f32 / self.total as f32).clamp(0.0, 1.0))
    }

    /// Compact human label, e.g. `pulling (42%)` or `verifying sha256…`.
    pub fn label(&self) -> String {
        match self.ratio() {
            Some(r) => format!("{} ({:.0}%)", self.status, r * 100.0),
            None => {
                if self.status.is_empty() {
                    "starting…".to_owned()
                } else {
                    self.status.clone()
                }
            }
        }
    }
}

/// Open a streaming `ollama.pull` for `model`. The caller loops on
/// [`PipeStream::recv`], projecting each frame with
/// [`PullProgress::from_value`] until [`PullProgress::is_success`] (or an
/// error / end-of-stream). Dropping the stream cancels the pull.
pub fn pull_model(model: &str) -> Result<PipeStream, String> {
    crate::stream_call(SVC_OLLAMA, "ollama.pull", json!({ "name": model }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_from_embed_model_missing_error() {
        // The exact shape wylde-workspaces / wylde-harness emit.
        let err = "backend has no model named \"nomic-embed-text\" — pull it with: \
                   ollama pull nomic-embed-text (model \"nomic-embed-text\" not installed in Ollama)";
        assert_eq!(
            parse_pullable_model(err).as_deref(),
            Some("nomic-embed-text")
        );
    }

    #[test]
    fn parses_name_from_generic_model_not_found() {
        let err = "model \"llama3.2:3b\" not installed in Ollama";
        assert_eq!(parse_pullable_model(err).as_deref(), Some("llama3.2:3b"));
    }

    #[test]
    fn parses_tagged_and_hf_model_names() {
        let err = "pull it with: ollama pull hf.co/user/repo:Q4_K_M";
        assert_eq!(
            parse_pullable_model(err).as_deref(),
            Some("hf.co/user/repo:Q4_K_M")
        );
    }

    #[test]
    fn returns_none_for_unrelated_errors() {
        assert_eq!(parse_pullable_model("ollama_unreachable: upstream down"), None);
        assert_eq!(parse_pullable_model("vram admission denied"), None);
        assert_eq!(parse_pullable_model(""), None);
    }

    #[test]
    fn progress_ratio_and_label() {
        let mid = PullProgress::from_value(&json!({
            "status": "pulling", "completed": 50u64, "total": 200u64,
            "digest": "sha256:abc"
        }));
        assert_eq!(mid.ratio(), Some(0.25));
        assert_eq!(mid.label(), "pulling (25%)");
        assert!(!mid.is_success());

        let meta = PullProgress::from_value(&json!({ "status": "pulling manifest" }));
        assert_eq!(meta.ratio(), None);
        assert_eq!(meta.label(), "pulling manifest");

        let done = PullProgress::from_value(&json!({ "status": "success" }));
        assert!(done.is_success());
    }

    #[test]
    fn progress_clamps_overshoot() {
        let over = PullProgress::from_value(&json!({
            "status": "pulling", "completed": 300u64, "total": 200u64
        }));
        assert_eq!(over.ratio(), Some(1.0));
    }
}
