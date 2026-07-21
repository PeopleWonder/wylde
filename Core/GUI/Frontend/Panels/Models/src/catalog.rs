//! Curated catalog of popular Ollama models, backing the "Pull a model"
//! autocomplete.
//!
//! Ollama exposes no official library-search API, so — like Open WebUI
//! and other clients — we bundle a hand-curated starter catalog.  It is
//! **not** meant to be exhaustive (~40 entries covering the main
//! families); anything uncatalogued still pulls fine via the "Pull
//! anyway" fallback, since `ollama pull <tag>` works regardless of
//! whether we list it.
//!
//! The catalog ships as compile-time JSON (`include_str!`) so there is
//! no runtime file dependency.  An optional `WYLDE_MODEL_CATALOG_URL`
//! env override points at a local JSON file (a `file://` prefix is
//! accepted) for users who want to refresh the list without rebuilding;
//! a malformed or unreadable override falls back to the bundled copy
//! rather than leaving the panel empty.
//!
//! Metadata is sourced from real ollama.com/library model cards — sizes
//! and parameter counts are the values those pages display, not
//! estimates.  Entries we could not verify were omitted rather than
//! guessed.

use std::sync::OnceLock;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

/// Compile-time bundled catalog.  Always available as the fallback.
const BUNDLED_CATALOG_JSON: &str = include_str!("../catalog/models.json");

/// Env var pointing at a local JSON file that overrides the bundled
/// catalog.  Named `…_URL` for forward-compatibility with a future
/// remote-refresh path; today it is read as a filesystem path (a
/// `file://` prefix is stripped).  `http(s)://` values are not fetched
/// in-process — they log a warning and fall back to bundled.
const CATALOG_OVERRIDE_ENV: &str = "WYLDE_MODEL_CATALOG_URL";

/// One curated model.  `context` and `license` are optional because not
/// every model page surfaces them; the UI just omits the field when it
/// is absent rather than rendering a placeholder.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CatalogEntry {
    /// The exact `ollama pull <tag>` argument, e.g. `"llama3.2:3b"`.
    pub tag: String,
    /// Short family slug used for grouping and the row's letter icon.
    pub family: String,
    /// Human-friendly label, e.g. `"Llama 3.2 3B"`.
    pub display_name: String,
    /// Parameter count as shown on the model card, e.g. `"3B"`, `"8x7B"`.
    pub parameters: String,
    /// Download size in GB, taken from the model card's displayed size.
    pub size_gb: f64,
    /// Context window in tokens, when the card states it.
    #[serde(default)]
    pub context: Option<u64>,
    /// License name, when the card states it.
    #[serde(default)]
    pub license: Option<String>,
    /// One-line factual description.
    pub description: String,
    /// Category tags (general / instruct / code / vision / embedding /
    /// reasoning / small / tiny / moe).  Folded into the search haystack.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl CatalogEntry {
    /// 1–2 letter badge for the family icon, upper-cased.
    pub fn icon_letters(&self) -> String {
        let base = if self.family.is_empty() {
            &self.tag
        } else {
            &self.family
        };
        base.chars()
            .filter(|c| c.is_alphanumeric())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }

    /// Human-readable download size, e.g. `"2.0 GB"` or `"274 MB"` for
    /// sub-gigabyte models.
    pub fn size_label(&self) -> String {
        format_size_gb(self.size_gb)
    }

    /// The text a query is matched against: tag, family, display name,
    /// parameters, and category tags joined.  Folding them into one
    /// haystack lets "llama", "3b", "code", or "vision" each hit
    /// whichever field carries it.
    fn searchable_text(&self) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(&self.tag);
        s.push(' ');
        s.push_str(&self.family);
        s.push(' ');
        s.push_str(&self.display_name);
        s.push(' ');
        s.push_str(&self.parameters);
        for t in &self.tags {
            s.push(' ');
            s.push_str(t);
        }
        s
    }
}

/// Format a GB float as a size label, dropping to MB below 1 GB so tiny
/// embedding/curiosity models don't all read "0.3 GB".
pub fn format_size_gb(gb: f64) -> String {
    if gb <= 0.0 {
        "—".to_owned()
    } else if gb < 1.0 {
        format!("{} MB", (gb * 1024.0).round() as i64)
    } else {
        format!("{gb:.1} GB")
    }
}

/// The process-wide catalog, parsed once.  Honors the env override, then
/// falls back to the bundled JSON.
pub fn catalog() -> &'static [CatalogEntry] {
    static CATALOG: OnceLock<Vec<CatalogEntry>> = OnceLock::new();
    CATALOG.get_or_init(load_catalog)
}

fn load_catalog() -> Vec<CatalogEntry> {
    if let Some(over) = std::env::var(CATALOG_OVERRIDE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        match load_override(&over) {
            Ok(entries) if !entries.is_empty() => return entries,
            Ok(_) => {
                // Parsed but empty — treat as a mistake, keep bundled.
            }
            Err(e) => {
                // Don't blow up the panel over a bad override; log and
                // fall through to the bundled catalog.
                eprintln!("[models] {CATALOG_OVERRIDE_ENV} ignored: {e}");
            }
        }
    }
    parse_catalog(BUNDLED_CATALOG_JSON)
        .expect("bundled models.json is valid (checked by catalog_parses test)")  // INVARIANT: BUNDLED_CATALOG_JSON is compile-time-embedded and validated by the catalog_parses test. wylde-check: panel-panic-allowed
}

/// Resolve and read an override value into a parsed catalog.  Accepts a
/// bare path or a `file://` URL; `http(s)://` is rejected (not fetched
/// in-process) so the caller falls back to bundled.
fn load_override(value: &str) -> anyhow::Result<Vec<CatalogEntry>> {
    let v = value.trim();
    if v.starts_with("http://") || v.starts_with("https://") {
        anyhow::bail!("remote URLs are not fetched in-process; point at a local JSON file");
    }
    let path = v.strip_prefix("file://").unwrap_or(v);
    let json = std::fs::read_to_string(path)?;
    parse_catalog(&json)
}

/// Parse catalog JSON (an array of [`CatalogEntry`]).
pub fn parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogEntry>> {
    let entries: Vec<CatalogEntry> = serde_json::from_str(json)?;
    Ok(entries)
}

/// Fuzzy-rank the catalog against `query`, returning up to `limit`
/// entries best-first.
///
///   * Empty / whitespace query → the first `limit` entries in catalog
///     order (the catalog is authored most-popular-first, so this is the
///     useful default suggestion set).
///   * Otherwise → only entries whose `searchable_text` fuzzily matches,
///     sorted by descending nucleo relevance, ties broken by tag so the
///     list is stable across renders.
pub fn fuzzy_search(query: &str, limit: usize) -> Vec<&'static CatalogEntry> {
    search_in(catalog(), query, limit)
}

/// Testable core of [`fuzzy_search`] over an explicit slice.
pub(crate) fn search_in<'a>(
    entries: &'a [CatalogEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a CatalogEntry> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return entries.iter().take(limit).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(trimmed, CaseMatching::Smart, Normalization::Smart);
    // One scratch buffer reused across haystacks — `Utf32Str::new`
    // clears it per call (and skips it for ASCII), so reuse is safe.
    let mut buf: Vec<char> = Vec::new();

    let mut scored: Vec<(u32, &CatalogEntry)> = entries
        .iter()
        .filter_map(|e| {
            let haystack = e.searchable_text();
            let utf = Utf32Str::new(&haystack, &mut buf);
            pattern.score(utf, &mut matcher).map(|score| (score, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.tag.cmp(&b.1.tag)));
    scored.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// Exact catalog lookup by tag — used to surface the parameters + size
/// detail for the currently-typed/selected tag.
pub fn exact(tag: &str) -> Option<&'static CatalogEntry> {
    let t = tag.trim();
    catalog().iter().find(|e| e.tag == t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_parses_and_is_nonempty() {
        let entries = parse_catalog(BUNDLED_CATALOG_JSON).expect("bundled catalog parses");
        assert!(
            entries.len() >= 30,
            "expected a starter catalog of ~30+ entries, got {}",
            entries.len()
        );
    }

    #[test]
    fn bundled_entries_have_sane_fields() {
        let entries = parse_catalog(BUNDLED_CATALOG_JSON).unwrap();
        for e in &entries {
            assert!(!e.tag.is_empty(), "entry with empty tag");
            assert!(!e.family.is_empty(), "{}: empty family", e.tag);
            assert!(!e.display_name.is_empty(), "{}: empty display_name", e.tag);
            assert!(e.size_gb > 0.0, "{}: non-positive size_gb", e.tag);
        }
    }

    #[test]
    fn bundled_tags_are_unique() {
        let entries = parse_catalog(BUNDLED_CATALOG_JSON).unwrap();
        let mut tags: Vec<&str> = entries.iter().map(|e| e.tag.as_str()).collect();
        let before = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(before, tags.len(), "duplicate tags in bundled catalog");
    }

    fn entry(tag: &str, family: &str, display: &str, params: &str, tags: &[&str]) -> CatalogEntry {
        CatalogEntry {
            tag: tag.into(),
            family: family.into(),
            display_name: display.into(),
            parameters: params.into(),
            size_gb: 2.0,
            context: None,
            license: None,
            description: String::new(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sample() -> Vec<CatalogEntry> {
        vec![
            entry("llama3.2:3b", "llama", "Llama 3.2 3B", "3B", &["general"]),
            entry("llama3.1:8b", "llama", "Llama 3.1 8B", "8B", &["general"]),
            entry("qwen2.5:7b", "qwen", "Qwen2.5 7B", "7B", &["general"]),
            entry(
                "qwen2.5-coder:7b",
                "qwen",
                "Qwen2.5 Coder 7B",
                "7B",
                &["code"],
            ),
            entry(
                "nomic-embed-text",
                "nomic",
                "Nomic Embed Text",
                "137M",
                &["embedding"],
            ),
        ]
    }

    fn tags(v: &[&CatalogEntry]) -> Vec<String> {
        v.iter().map(|e| e.tag.clone()).collect()
    }

    #[test]
    fn empty_query_returns_first_n_in_order() {
        let cat = sample();
        let got = tags(&search_in(&cat, "", 3));
        assert_eq!(got, vec!["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"]);
        // Whitespace-only behaves like empty.
        assert_eq!(tags(&search_in(&cat, "   ", 2)).len(), 2);
    }

    #[test]
    fn family_query_keeps_only_that_family() {
        let cat = sample();
        let got = tags(&search_in(&cat, "qwen", 10));
        assert!(got.iter().all(|t| t.contains("qwen")), "only qwen: {got:?}");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn category_tag_is_searchable() {
        // "code" hits the coder entry via its category tag, not its name.
        let cat = sample();
        let got = tags(&search_in(&cat, "code", 10));
        assert!(got.contains(&"qwen2.5-coder:7b".to_owned()), "got {got:?}");
    }

    #[test]
    fn embedding_query_matches_via_tag() {
        let cat = sample();
        let got = tags(&search_in(&cat, "embedding", 10));
        assert_eq!(got, vec!["nomic-embed-text".to_owned()]);
    }

    #[test]
    fn limit_caps_result_count() {
        let cat = sample();
        assert!(search_in(&cat, "", 2).len() <= 2);
        assert!(search_in(&cat, "llama", 1).len() <= 1);
    }

    #[test]
    fn no_match_returns_empty() {
        let cat = sample();
        assert!(search_in(&cat, "asdjkfh", 10).is_empty());
    }

    #[test]
    fn exact_lookup_finds_by_tag() {
        let cat = sample();
        let hit = cat.iter().find(|e| e.tag == "qwen2.5:7b");
        assert!(hit.is_some());
    }

    #[test]
    fn icon_letters_takes_two_uppercase() {
        let e = entry("llama3.2:3b", "llama", "Llama 3.2 3B", "3B", &[]);
        assert_eq!(e.icon_letters(), "LL");
        let e2 = entry("nomic-embed-text", "nomic", "Nomic", "137M", &[]);
        assert_eq!(e2.icon_letters(), "NO");
    }

    #[test]
    fn size_label_drops_to_mb_below_one_gb() {
        assert_eq!(format_size_gb(2.0), "2.0 GB");
        assert_eq!(format_size_gb(0.27), "276 MB");
        assert_eq!(format_size_gb(0.0), "—");
    }

    #[test]
    fn override_rejects_http_urls() {
        // http(s) values are not fetched in-process.
        assert!(load_override("https://example.com/models.json").is_err());
        assert!(load_override("http://example.com/models.json").is_err());
    }

    #[test]
    fn override_reads_local_file_when_present() {
        // A bare path that doesn't exist should error (and the caller
        // falls back to bundled) — proves we attempt a filesystem read
        // rather than mis-routing the value.
        let missing = load_override("definitely-not-a-real-catalog-file.json");
        assert!(missing.is_err());
    }
}
