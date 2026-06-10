//! HuggingFace online model search — the opt-in extension to the curated
//! catalog.
//!
//! Privacy-first: this module is only ever exercised when the user has
//! turned on "Online model search" in Settings → Privacy & Network. The
//! Models panel checks `wylde_gui_pipe::privacy_prefs::current()` before
//! surfacing any affordance that calls in here, so with the toggle off no
//! code path reaches HuggingFace at all.
//!
//! The query hits HuggingFace's public list API
//! (`GET https://huggingface.co/api/models?search=…&filter=gguf&limit=20`),
//! filtered to GGUF repos since those are what Ollama can pull via its
//! `hf.co/<owner>/<repo>:<quant>` syntax. The request is a blocking
//! `reqwest` call (the project's chosen HTTP client) with a 5 s timeout,
//! run off the gpui thread through the Pipe crate's blocking bridge.

use std::time::Duration;

use serde_json::Value;

/// The HuggingFace list endpoint. Kept as a constant so the privacy modal
/// copy and this call site can't drift apart.
pub const HF_API_URL: &str = "https://huggingface.co/api/models";

/// Hard cap on the request, per the spec's 5 s budget.
const HF_TIMEOUT: Duration = Duration::from_secs(5);

/// How many results to ask HuggingFace for.
const HF_LIMIT: &str = "20";

/// Quantization options offered for an HF pull, default-first. These are
/// the common GGUF quants Ollama accepts as the `:tag` suffix on an
/// `hf.co/...` pull. The user picks one before pulling; we never silently
/// commit a choice for them.
pub const QUANTS: &[&str] = &["Q4_K_M", "Q5_K_M", "Q6_K", "Q8_0", "FP16"];

/// The default quant a freshly-selected result starts on.
pub fn default_quant() -> &'static str {
    QUANTS[0]
}

/// One HuggingFace search hit, projected to the fields the result row
/// renders. The full API object is much larger; we keep only what we show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HfModel {
    /// `owner/repo`, e.g. `"bartowski/Qwen2.5-7B-Instruct-GGUF"`. This is
    /// what becomes the `hf.co/<repo_id>` pull target.
    pub repo_id: String,
    /// Repo owner (the segment before the `/`), or the explicit `author`
    /// field when the API provides one.
    pub author: String,
    /// Lifetime download count (0 when absent).
    pub downloads: u64,
    /// Last-modified date, trimmed to the `YYYY-MM-DD` day portion for a
    /// compact display. Empty when the API omits it.
    pub last_modified: String,
}

/// Translate a selected repo + quant into the exact Ollama pull tag.
///
/// Ollama pulls a HuggingFace GGUF with `ollama pull hf.co/<repo>:<quant>`,
/// so the Models panel just drops this string into its existing pull
/// field and the regular Pull button does the rest.
pub fn to_pull_tag(repo_id: &str, quant: &str) -> String {
    format!("hf.co/{repo_id}:{quant}")
}

/// Parse the HuggingFace `/api/models` JSON array into projected rows.
///
/// Pure + total: a non-array body yields an empty vec, and any entry
/// missing an `id`/`modelId` is skipped rather than rendered blank. Split
/// out from the network call so it's testable against a fixture.
pub fn parse_search_results(body: &Value) -> Vec<HfModel> {
    let Some(arr) = body.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_one).collect()
}

fn parse_one(v: &Value) -> Option<HfModel> {
    // The list endpoint uses `id`; some shapes carry `modelId`. Either is
    // the `owner/repo` string we need.
    let repo_id = v
        .get("id")
        .or_else(|| v.get("modelId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())?;
    let author = v
        .get("author")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            // Derive from the `owner/repo` prefix when no explicit author.
            repo_id
                .split_once('/')
                .map(|(owner, _)| owner.to_owned())
                .unwrap_or_default()
        });
    let downloads = v.get("downloads").and_then(Value::as_u64).unwrap_or(0);
    let last_modified = v
        .get("lastModified")
        .and_then(Value::as_str)
        // Keep the day portion only — `2024-01-02T03:04:05.000Z` → `2024-01-02`.
        .map(|s| s.split('T').next().unwrap_or(s).to_owned())
        .unwrap_or_default();
    Some(HfModel {
        repo_id,
        author,
        downloads,
        last_modified,
    })
}

/// Search HuggingFace for `term`, off the gpui executor.
///
/// Hops the blocking `reqwest` call onto the shared tokio runtime's
/// blocking pool (the same bridge the updater uses) so a `cx.spawn` task —
/// which has no tokio reactor — can await it. Returns the projected rows
/// on success; an unreachable/slow/rate-limited HuggingFace surfaces as an
/// `Err(String)` the panel renders inline rather than crashing.
pub async fn search(term: String) -> Result<Vec<HfModel>, String> {
    wylde_gui_pipe::bridged_spawn_blocking(move || search_blocking(&term)).await
}

/// The blocking body of [`search`]. Separate so the bridge wrapper stays a
/// one-liner and this can be reasoned about as plain synchronous code.
fn search_blocking(term: &str) -> Result<Vec<HfModel>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HF_TIMEOUT)
        .build()
        .map_err(|e| format!("HuggingFace client: {e}"))?;
    let resp = client
        .get(HF_API_URL)
        // reqwest URL-encodes these, so a query like "qwen 3.6" is safe.
        .query(&[("search", term), ("filter", "gguf"), ("limit", HF_LIMIT)])
        // A descriptive UA is good HF citizenship and avoids some bot blocks.
        .header(reqwest::header::USER_AGENT, "Wylde/0.1 (+models-panel)")
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                "HuggingFace timed out (5s) — check your connection.".to_owned()
            } else {
                format!("HuggingFace unreachable: {e}")
            }
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!(
            "HuggingFace returned {} — try again later.",
            status.as_u16()
        ));
    }
    let body: Value = resp
        .json()
        .map_err(|e| format!("HuggingFace response parse: {e}"))?;
    Ok(parse_search_results(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_quant_is_q4_k_m() {
        assert_eq!(default_quant(), "Q4_K_M");
        // Default must be a member of the offered list.
        assert!(QUANTS.contains(&default_quant()));
    }

    #[test]
    fn pull_tag_uses_hf_co_prefix() {
        assert_eq!(
            to_pull_tag("bartowski/Qwen2.5-7B-Instruct-GGUF", "Q4_K_M"),
            "hf.co/bartowski/Qwen2.5-7B-Instruct-GGUF:Q4_K_M"
        );
        assert_eq!(to_pull_tag("owner/repo", "Q8_0"), "hf.co/owner/repo:Q8_0");
    }

    #[test]
    fn parse_projects_full_api_row() {
        let body = json!([
            {
                "id": "bartowski/Qwen2.5-7B-Instruct-GGUF",
                "downloads": 123456_u64,
                "lastModified": "2024-11-02T03:04:05.000Z",
                "likes": 42
            }
        ]);
        let rows = parse_search_results(&body);
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.repo_id, "bartowski/Qwen2.5-7B-Instruct-GGUF");
        // Author derived from the owner/repo prefix.
        assert_eq!(m.author, "bartowski");
        assert_eq!(m.downloads, 123456);
        // Trimmed to the day.
        assert_eq!(m.last_modified, "2024-11-02");
    }

    #[test]
    fn parse_prefers_explicit_author_then_falls_back() {
        let body = json!([
            { "id": "team/model-a", "author": "explicit-author" },
            { "id": "solo/model-b" },
            { "modelId": "alt/model-c" }
        ]);
        let rows = parse_search_results(&body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].author, "explicit-author");
        assert_eq!(rows[1].author, "solo");
        // `modelId` is accepted as the id alias.
        assert_eq!(rows[2].repo_id, "alt/model-c");
        assert_eq!(rows[2].author, "alt");
    }

    #[test]
    fn parse_skips_rows_without_an_id() {
        let body = json!([
            { "downloads": 5_u64 },
            { "id": "" },
            { "id": "good/repo" }
        ]);
        let rows = parse_search_results(&body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo_id, "good/repo");
    }

    #[test]
    fn parse_non_array_is_empty() {
        assert!(parse_search_results(&json!({ "error": "rate limited" })).is_empty());
        assert!(parse_search_results(&json!("nope")).is_empty());
    }

    #[test]
    fn parse_defaults_missing_optional_fields() {
        let rows = parse_search_results(&json!([{ "id": "a/b" }]));
        assert_eq!(rows[0].downloads, 0);
        assert!(rows[0].last_modified.is_empty());
    }
}
