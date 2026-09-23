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

/// Running aggregate of an `ollama.pull` stream across all layers, so a
/// progress bar reflects the **overall** download rather than resetting to
/// 0% each time Ollama moves to the next layer.
///
/// Ollama streams per-layer frames keyed by `digest` (each with its own
/// `completed`/`total`). We keep the largest `completed`/`total` seen per
/// digest and sum across digests for the overall ratio; manifest / verify /
/// write frames (no digest) just update the status text.
#[derive(Debug, Clone, Default)]
pub struct PullAggregate {
    /// `digest -> (completed, total)` for every sized layer seen so far.
    layers: std::collections::BTreeMap<String, (u64, u64)>,
    /// Latest status text (`"pulling manifest"`, `"verifying sha256 digest"`, …).
    pub status: String,
}

impl PullAggregate {
    /// Fold one projected frame into the aggregate.
    pub fn update(&mut self, p: &PullProgress) {
        if !p.status.is_empty() {
            self.status = p.status.clone();
        }
        // Only sized download frames carry a digest + total; frames are
        // monotonic but take the max defensively against any reordering.
        if !p.digest.is_empty() && p.total > 0 {
            let e = self.layers.entry(p.digest.clone()).or_insert((0, 0));
            e.0 = e.0.max(p.completed);
            e.1 = e.1.max(p.total);
        }
    }

    /// Total bytes downloaded / total bytes known so far.
    pub fn bytes(&self) -> (u64, u64) {
        self.layers
            .values()
            .fold((0u64, 0u64), |(c, t), (lc, lt)| (c + lc, t + lt))
    }

    /// Overall `0.0..=1.0`, or `None` before any sized layer has been seen
    /// (manifest/metadata phase) — render an empty/indeterminate bar then.
    pub fn overall_ratio(&self) -> Option<f32> {
        let (c, t) = self.bytes();
        if t == 0 {
            return None;
        }
        Some((c as f32 / t as f32).clamp(0.0, 1.0))
    }

    /// Overall percent (0..=100), or `None` while indeterminate.
    pub fn percent(&self) -> Option<u32> {
        self.overall_ratio().map(|r| (r * 100.0).round() as u32)
    }

    /// `pulling 42%` style label for the text beside the bar.
    pub fn label(&self) -> String {
        let status = if self.status.is_empty() {
            "pulling"
        } else {
            &self.status
        };
        match self.percent() {
            Some(p) => format!("{status} {p}%"),
            None => format!("{status}…"),
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
        assert_eq!(
            parse_pullable_model("ollama_unreachable: upstream down"),
            None
        );
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

    #[test]
    fn aggregate_overall_percent_across_layers() {
        let mut agg = PullAggregate::default();
        // Manifest frame: status only, no bar yet.
        agg.update(&PullProgress::from_value(
            &json!({ "status": "pulling manifest" }),
        ));
        assert_eq!(agg.overall_ratio(), None);
        assert_eq!(agg.label(), "pulling manifest…");

        // Layer A: 50/100. Overall = 50%.
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:a", "completed": 50u64, "total": 100u64
        })));
        assert_eq!(agg.percent(), Some(50));

        // Layer B appears: 0/300. Overall now 50 / 400 = 12%.
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:b", "completed": 0u64, "total": 300u64
        })));
        assert_eq!(agg.bytes(), (50, 400));
        assert_eq!(agg.percent(), Some(13)); // 12.5 rounds to 13

        // Layer A completes, B advances: 100/100 + 150/300 = 250/400 = 62.5% → 63.
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:a", "completed": 100u64, "total": 100u64
        })));
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:b", "completed": 150u64, "total": 300u64
        })));
        assert_eq!(agg.percent(), Some(63));
        assert_eq!(agg.label(), "pulling 63%");
    }

    #[test]
    fn aggregate_does_not_regress_on_out_of_order_frame() {
        let mut agg = PullAggregate::default();
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:a", "completed": 90u64, "total": 100u64
        })));
        // A stale/duplicate frame with a smaller completed must not drop the bar.
        agg.update(&PullProgress::from_value(&json!({
            "status": "pulling", "digest": "sha256:a", "completed": 10u64, "total": 100u64
        })));
        assert_eq!(agg.percent(), Some(90));
    }
}
