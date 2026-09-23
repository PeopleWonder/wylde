//! Egress destination registry — manifest-driven allowlist.
//!
//! Rust port of `Gateway/egress/destinations.py`. Walks
//! `Wylde/<component>/manifest.json` + `Wylde/Extensions/<ext>/manifest.json`
//! at startup, collects every `egress` entry, and publishes them under
//! their declared `key` — **scoped to the declaring component**.
//!
//! Manifest schema (JSON):
//!
//! ```json
//! {
//!   "name": "Webcrawler",
//!   "egress": [
//!     {
//!       "key":            "web",
//!       "url_prefix":     "https://",
//!       "purpose":        "Public web fetch.",
//!       "verify_tls":     true,
//!       "auth_env":       "",
//!       "path_allowlist": []
//!     }
//!   ]
//! }
//! ```
//!
//! URL-prefix vs full URL
//! -----------------------
//! An entry whose `url_prefix` is just a scheme (`https://`) is treated
//! as a **wildcard** — callers supply the full URL and only the scheme
//! is enforced. Anything else is a **pinned** base URL — paths are
//! appended verbatim and must satisfy `path_allowlist` (if declared).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum EgressDestinationError {
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default)]
pub struct Destination {
    pub key: String,
    pub component: String,
    pub url_prefix: String,
    pub auth_header_env: String,
    pub verify_tls: bool,
    pub purpose: String,
    pub path_allowlist: Vec<String>,
    /// Optional SSRF host pinning — when non-empty, the resolved host must
    /// match one entry (exact, or suffix via a leading `*.`/`.`). Empty ⇒
    /// any public host (the SSRF deny-list still applies). See
    /// [`super::ssrf`].
    pub host_allowlist: Vec<String>,
    /// Escape hatch for a destination that legitimately reaches a
    /// private/loopback host. Off by default; the SSRF deny-list is only
    /// skipped when this is `true`. Must never be set on a wildcard
    /// destination.
    pub allow_private: bool,
}

impl Destination {
    /// True when `url_prefix` is just a scheme (no host) — caller
    /// supplies the full URL and we only enforce the scheme.
    pub fn is_wildcard(&self) -> bool {
        let (_, host_and_path) = match split_scheme(&self.url_prefix) {
            Some(p) => p,
            None => return false,
        };
        host_and_path.is_empty()
    }

    fn scheme(&self) -> Option<&str> {
        split_scheme(&self.url_prefix).map(|(s, _)| s)
    }
}

/// `scheme://rest` → `Some(("scheme", "rest"))`; else `None`.
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let idx = s.find("://")?;
    let scheme = &s[..idx];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    Some((scheme, &s[idx + 3..]))
}

/// component name → { key → Destination }
type Registry = HashMap<String, HashMap<String, Destination>>;

fn registry() -> &'static RwLock<Registry> {
    static CELL: std::sync::OnceLock<RwLock<Registry>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Walk every component manifest under `root` and rebuild the registry.
///
/// Called from lifespan startup. Tests pass a tempdir to populate from a
/// fixture. Falls back to `WYLDE_ROOT` or the workspace root when `None`.
pub fn reload(root: Option<&Path>) {
    let owned: PathBuf;
    let r = match root {
        Some(p) => p,
        None => {
            owned = std::env::var_os("WYLDE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(default_root);
            owned.as_path()
        }
    };

    let mut new: Registry = HashMap::new();
    if !r.is_dir() {
        tracing::info!("egress: WYLDE_ROOT {:?} not a dir — registry empty", r);
        replace_registry(new);
        return;
    }

    for manifest in candidate_manifests(r) {
        let data = match load_manifest(&manifest) {
            Some(d) => d,
            None => continue,
        };
        let component = data
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                manifest
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let entries = match data.get("egress") {
            Some(Value::Array(a)) => a,
            _ => continue,
        };
        if entries.is_empty() {
            continue;
        }
        let mut per_comp: HashMap<String, Destination> = HashMap::new();
        for (i, raw) in entries.iter().enumerate() {
            let label = format!("{}: egress[{i}]", manifest.display());
            if let Some(d) = entry_to_destination(&component, raw, &label) {
                if per_comp.contains_key(&d.key) {
                    tracing::warn!(
                        "egress: {}: duplicate key {:?} within component {}",
                        manifest.display(),
                        d.key,
                        component
                    );
                    continue;
                }
                per_comp.insert(d.key.clone(), d);
            }
        }
        if !per_comp.is_empty() {
            new.insert(component, per_comp);
        }
    }

    let total: usize = new.values().map(HashMap::len).sum();
    let comps = new.len();
    replace_registry(new);
    tracing::info!(
        "egress: registered {} destinations across {} components",
        total,
        comps
    );
}

fn replace_registry(new: Registry) {
    let mut guard = registry().write().expect("registry poisoned");
    *guard = new;
}

fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn candidate_manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if name.is_empty() || name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        if name.eq_ignore_ascii_case("extensions") {
            if let Ok(ext_iter) = std::fs::read_dir(&path) {
                for ext in ext_iter.flatten() {
                    let p = ext.path();
                    if p.is_dir() {
                        let m = p.join("manifest.json");
                        if m.is_file() {
                            out.push(m);
                        }
                    }
                }
            }
            continue;
        }
        let m = path.join("manifest.json");
        if m.is_file() {
            out.push(m);
        }
    }
    out
}

fn load_manifest(path: &Path) -> Option<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("egress: cannot read {}: {}", path.display(), e);
            return None;
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("egress: cannot parse {}: {}", path.display(), e);
            None
        }
    }
}

fn entry_to_destination(component: &str, raw: &Value, label: &str) -> Option<Destination> {
    let obj = match raw {
        Value::Object(m) => m,
        _ => {
            tracing::warn!("egress: {}: each egress entry must be an object", label);
            return None;
        }
    };
    let key = obj
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if key.is_empty() {
        tracing::warn!("egress: {}: missing 'key'", label);
        return None;
    }
    let url_prefix = obj
        .get("url_prefix")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url_prefix.is_empty() {
        tracing::warn!("egress: {}/{}: missing 'url_prefix'", label, key);
        return None;
    }
    let auth_env = obj
        .get("auth_env")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let verify_tls = obj
        .get("verify_tls")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let purpose = obj
        .get("purpose")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let path_allowlist = match obj.get("path_allowlist") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    let host_allowlist = match obj.get("host_allowlist") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let allow_private = obj
        .get("allow_private")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Guard-rail: a wildcard destination (`url_prefix` is just a scheme)
    // must never carry `allow_private` — that would re-open SSRF to the
    // whole internal network for any URL the caller supplies. Drop the
    // flag with a loud warning rather than honour it.
    let is_wildcard = split_scheme(&url_prefix)
        .map(|(_, r)| r.is_empty())
        .unwrap_or(false);
    let allow_private = if allow_private && is_wildcard {
        tracing::warn!(
            "egress: {}/{}: 'allow_private' ignored on a wildcard destination (SSRF risk)",
            label,
            key
        );
        false
    } else {
        allow_private
    };
    Some(Destination {
        key,
        component: component.to_owned(),
        url_prefix,
        auth_header_env: auth_env,
        verify_tls,
        purpose,
        path_allowlist,
        host_allowlist,
        allow_private,
    })
}

/// Look up a destination by `(caller, key)`. Cross-component access is
/// rejected — the caller must be the component name that *declared* the
/// destination.
pub fn resolve(caller: &str, key: &str) -> Result<Destination, EgressDestinationError> {
    if key.is_empty() {
        return Err(EgressDestinationError::Invalid(
            "destination key is required".into(),
        ));
    }
    if caller.is_empty() {
        return Err(EgressDestinationError::Invalid(
            "caller is required for scoped destination lookup".into(),
        ));
    }
    let guard = registry().read().expect("registry poisoned");
    let comp = guard.get(caller).ok_or_else(|| {
        EgressDestinationError::Invalid(format!(
            "caller {caller:?} declares no egress destinations"
        ))
    })?;
    let dest = comp.get(key).ok_or_else(|| {
        EgressDestinationError::Invalid(format!(
            "caller {caller:?} did not declare destination {key:?}"
        ))
    })?;
    Ok(dest.clone())
}

/// Validate `path` against `dest`. Returns the validated path on success.
pub fn validate_path(dest: &Destination, path: &str) -> Result<String, EgressDestinationError> {
    if path.is_empty() {
        return Err(EgressDestinationError::Invalid("path is required".into()));
    }

    if dest.is_wildcard() {
        let (scheme, rest) = match split_scheme(path) {
            Some(p) => p,
            None => {
                return Err(EgressDestinationError::Invalid(format!(
                    "wildcard destination {:?} requires a full URL (scheme+host)",
                    dest.key
                )));
            }
        };
        // Host must be non-empty (i.e., must not look like another
        // wildcard prefix `https://`).
        let host_part = rest.split('/').next().unwrap_or("");
        if host_part.is_empty() {
            return Err(EgressDestinationError::Invalid(format!(
                "wildcard destination {:?} requires a full URL (scheme+host)",
                dest.key
            )));
        }
        if let Some(want_scheme) = dest.scheme() {
            if !want_scheme.is_empty() && scheme != want_scheme {
                return Err(EgressDestinationError::Invalid(format!(
                    "destination {:?} only permits scheme {:?}",
                    dest.key, want_scheme
                )));
            }
        }
        return Ok(path.to_owned());
    }

    if !path.starts_with('/') {
        return Err(EgressDestinationError::Invalid(
            "path must start with '/' for pinned destinations".into(),
        ));
    }
    if path.contains("://") {
        return Err(EgressDestinationError::Invalid(
            "absolute URLs not permitted on pinned destinations".into(),
        ));
    }
    if !dest.path_allowlist.is_empty() {
        let bare = path.split('?').next().unwrap_or(path);
        let ok = dest
            .path_allowlist
            .iter()
            .any(|allowed| bare == allowed || bare.starts_with(&format!("{allowed}/")));
        if !ok {
            return Err(EgressDestinationError::Invalid(format!(
                "path {:?} not in allowlist for destination {:?}",
                bare, dest.key
            )));
        }
    }
    Ok(path.to_owned())
}

/// Compose the upstream URL. Wildcard → `path` is the full URL.
pub fn build_target_url(dest: &Destination, path: &str) -> String {
    if dest.is_wildcard() {
        return path.to_owned();
    }
    format!("{}{}", dest.url_prefix.trim_end_matches('/'), path)
}

/// Per-component diagnostic view, suitable for `GET /api/egress/destinations`.
/// Never leaks secret values — only env-var names appear.
pub fn list_destinations() -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let guard = registry().read().expect("registry poisoned");
    for (component, dests) in guard.iter() {
        // Emit each component's destinations sorted by key so the listing
        // is deterministic regardless of HashMap iteration order — the
        // Python port sorts the same way.
        let mut sorted: Vec<&Destination> = dests.values().collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        let mut arr = Vec::with_capacity(dests.len());
        for d in sorted {
            arr.push(serde_json::json!({
                "key": d.key,
                "url_prefix": d.url_prefix,
                "wildcard": d.is_wildcard(),
                "auth_env": d.auth_header_env,
                "verify_tls": d.verify_tls,
                "purpose": d.purpose,
                "path_allowlist": d.path_allowlist,
                "host_allowlist": d.host_allowlist,
                "allow_private": d.allow_private,
            }));
        }
        out.insert(component.clone(), Value::Array(arr));
    }
    out
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    replace_registry(HashMap::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::kill_switch::EGRESS_TEST_LOCK;
    use tempfile::TempDir;

    // ── One resource, ONE lock ───────────────────────────────────────────
    // These tests mutate the process-global destination registry
    // (`reset_for_test` / `reload`). So do `egress::client`'s and
    // `crate::pipe`'s tests — and those take `EGRESS_TEST_LOCK`. This module
    // used to take its own private `REGISTRY_LOCK` instead, and **two different
    // mutexes guarding one shared resource provide no mutual exclusion at all**:
    // each module was internally serialised and completely unsynchronised
    // against the other, so a `reload` here wiped the registry out from under a
    // `client` test mid-request. It surfaced as `forward_ssrf_blocks_metadata` /
    // `forward_ssrf_blocks_private` failing with
    // `Denied("caller \"Caller\" declares no egress destinations")` instead of
    // the `Ssrf` they assert — a flake in a REQUIRED check (4 failures in 20
    // runs of `--lib egress::`, vs 0 in 20 for `egress::client` alone; that gap
    // is the whole proof).
    //
    // Every test touching the registry must take THIS lock. It's a tokio mutex,
    // hence `#[tokio::test]` on the registry-touching tests below; the pure ones
    // (parsing, `split_scheme`, …) stay sync `#[test]` and need no lock.

    fn write_manifest(dir: &Path, component: &str, body: &str) {
        let comp = dir.join(component);
        std::fs::create_dir_all(&comp).unwrap();
        std::fs::write(comp.join("manifest.json"), body).unwrap();
    }

    #[tokio::test]
    async fn empty_registry_when_root_missing() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        reset_for_test();
        let path = PathBuf::from("/nonexistent-wylde-egress-test-root");
        reload(Some(&path));
        assert!(list_destinations().is_empty());
    }

    #[tokio::test]
    async fn reload_picks_up_egress_entries() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        reset_for_test();
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "Webcrawler",
            r#"{"name":"Webcrawler","egress":[{"key":"web","url_prefix":"https://","purpose":"wildcard"}]}"#,
        );
        reload(Some(tmp.path()));
        let listing = list_destinations();
        assert!(
            listing.contains_key("Webcrawler"),
            "got: {:?}",
            listing.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reload_collects_from_extensions_subdir() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        reset_for_test();
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("Extensions").join("MyExt");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.json"),
            r#"{"name":"MyExt","egress":[{"key":"web","url_prefix":"https://","purpose":""}]}"#,
        )
        .unwrap();
        reload(Some(tmp.path()));
        assert!(list_destinations().contains_key("MyExt"));
    }

    #[tokio::test]
    async fn resolve_rejects_cross_component_access() {
        let _g = EGRESS_TEST_LOCK.lock().await;
        reset_for_test();
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "Webcrawler",
            r#"{"name":"Webcrawler","egress":[{"key":"web","url_prefix":"https://","purpose":"wildcard"}]}"#,
        );
        reload(Some(tmp.path()));
        let err = resolve("OtherComponent", "web").expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.contains("OtherComponent"), "got: {msg}");
    }

    #[test]
    fn validate_path_pinned_requires_leading_slash() {
        let d = Destination {
            key: "api".into(),
            component: "X".into(),
            url_prefix: "https://api.example.com".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert!(validate_path(&d, "no-slash").is_err());
        assert!(validate_path(&d, "/ok").is_ok());
    }

    #[test]
    fn validate_path_pinned_rejects_absolute_url() {
        let d = Destination {
            key: "api".into(),
            component: "X".into(),
            url_prefix: "https://api.example.com".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert!(validate_path(&d, "https://attacker.com/x").is_err());
    }

    #[test]
    fn validate_path_pinned_honors_allowlist() {
        let d = Destination {
            key: "api".into(),
            component: "X".into(),
            url_prefix: "https://api.example.com".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec!["/v1/foo".into()],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert!(validate_path(&d, "/v1/foo").is_ok());
        assert!(validate_path(&d, "/v1/foo/bar").is_ok());
        assert!(validate_path(&d, "/v2/other").is_err());
    }

    #[test]
    fn validate_path_wildcard_requires_full_url() {
        let d = Destination {
            key: "web".into(),
            component: "X".into(),
            url_prefix: "https://".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert!(validate_path(&d, "/no-host").is_err());
        assert!(validate_path(&d, "https://example.com/").is_ok());
    }

    #[test]
    fn validate_path_wildcard_enforces_scheme() {
        let d = Destination {
            key: "web".into(),
            component: "X".into(),
            url_prefix: "https://".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        // http:// when https:// declared — rejected.
        assert!(validate_path(&d, "http://example.com/").is_err());
    }

    #[test]
    fn build_target_url_wildcard_passes_through() {
        let d = Destination {
            key: "web".into(),
            component: "X".into(),
            url_prefix: "https://".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert_eq!(
            build_target_url(&d, "https://example.com/foo"),
            "https://example.com/foo"
        );
    }

    #[test]
    fn build_target_url_pinned_appends_path() {
        let d = Destination {
            key: "api".into(),
            component: "X".into(),
            url_prefix: "https://api.example.com/".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert_eq!(
            build_target_url(&d, "/v1/x"),
            "https://api.example.com/v1/x"
        );
    }

    #[test]
    fn destination_is_wildcard_only_for_scheme_only_prefix() {
        let wild = Destination {
            key: "x".into(),
            component: "Y".into(),
            url_prefix: "https://".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        let pinned = Destination {
            key: "x".into(),
            component: "Y".into(),
            url_prefix: "https://api.example.com".into(),
            auth_header_env: String::new(),
            verify_tls: true,
            purpose: String::new(),
            path_allowlist: vec![],
            host_allowlist: vec![],
            allow_private: false,
        };
        assert!(wild.is_wildcard());
        assert!(!pinned.is_wildcard());
    }

    #[test]
    fn entry_parses_host_allowlist_and_allow_private() {
        let raw = serde_json::json!({
            "key": "api",
            "url_prefix": "https://api.internal.example.com",
            "host_allowlist": ["api.internal.example.com", "*.example.com"],
            "allow_private": true
        });
        let d = entry_to_destination("X", &raw, "test").expect("parses");
        assert_eq!(
            d.host_allowlist,
            vec!["api.internal.example.com", "*.example.com"]
        );
        // pinned destination → allow_private is honoured
        assert!(d.allow_private);
    }

    #[test]
    fn entry_drops_allow_private_on_wildcard() {
        let raw = serde_json::json!({
            "key": "web",
            "url_prefix": "https://",
            "allow_private": true
        });
        let d = entry_to_destination("X", &raw, "test").expect("parses");
        // A wildcard must never carry allow_private — it is forced off.
        assert!(!d.allow_private);
    }

    #[test]
    fn entry_defaults_new_fields() {
        let raw = serde_json::json!({
            "key": "web",
            "url_prefix": "https://"
        });
        let d = entry_to_destination("X", &raw, "test").expect("parses");
        assert!(d.host_allowlist.is_empty());
        assert!(!d.allow_private);
    }
}
