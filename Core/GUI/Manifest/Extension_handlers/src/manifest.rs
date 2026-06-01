//! Panel-manifest schema v2 — the on-disk JSON the build-time
//! aggregator reads, and the in-memory `PanelEntry` shape the runtime
//! registry holds.
//!
//! v2 changes from v1 (Phase 12.7):
//!
//!   * `source.kind` no longer accepts `"custom_element"`.  The Svelte
//!     custom-element path is gone in the gpui rewrite; manifests that
//!     still declare it now fail loud at parse rather than silently
//!     producing a panel that can't render.  §12 question 10 (hard
//!     bump).
//!   * `source.kind: "gpui_view"` is new.  Carries a `factory:` path-
//!     like string that the build-time aggregator maps to a real Rust
//!     call (see `factories.rs`).
//!   * `source.kind: "iframe"` stays.  Same loopback-only enforcement
//!     as the 12.7 surface — the predicate is ported into this crate
//!     at `loopback::is_loopback_url`.
//!   * `schema_version` is mandatory and must be `2`.  Anything else
//!     is rejected at parse time.

use serde::{Deserialize, Serialize};

use crate::loopback::is_loopback_url;

/// The only schema version this crate understands.  Bumped from 1 to 2
/// on the Svelte → gpui cutover (see plan §12 question 10).
pub const SCHEMA_VERSION: u32 = 2;

/// A whole `manifest.json` file's contents.  The aggregator parses one
/// of these per service or first-party panel directory.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PanelManifest {
    pub schema_version: u32,
    /// Owning service.  For first-party panels this is the lowercase
    /// service name (`core`, `memory`, …).  For extension panels we
    /// expect the extension name (`n8n`, `passwords`, …).  Free-form;
    /// used only to bucket panels in the sidebar by origin.
    pub service: String,
    pub panels: Vec<PanelEntry>,
}

/// Where a panel entry came from at runtime.  Used by the registry to
/// distinguish first-party panels (compiled in via the generated
/// source) from extension panels (overlaid at startup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelOrigin {
    /// Compiled in by the build-time aggregator.  `service` mirrors
    /// the manifest's `service` field.
    FirstParty { service: String },
    /// Reported by an extension via `extensions.list_panels`.  Held
    /// behind the runtime overlay, not the static registry.
    Extension { extension_id: String },
}

/// A single panel declaration.
///
/// On disk this is the JSON record under `panels[]`; in memory it is
/// the registry's row.  Both shapes are the same struct because we
/// expose the registry as JSON through `gui.list_tabs` — having a
/// single struct keeps the parse/serialise round-trip honest.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PanelEntry {
    /// Stable identifier within the panel's owning service.  Combined
    /// with the service it becomes the registry key (`core/settings`,
    /// `memory/long_term`).
    pub id: String,
    pub title: String,
    /// Lucide icon name (the Frontend/Lucide crate's lookup key).
    /// Optional because some panels in the alpha go without a chip
    /// icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Sidebar ordering.  Smaller numbers sit higher; ties broken by
    /// `service` then `id` for determinism.
    pub order: i32,
    pub version: String,
    /// Pipe verbs the panel requires the host to expose.  Empty for
    /// most first-party panels (they call backend services directly).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_services: Vec<String>,
    pub source: PanelSource,
}

/// How a panel paints itself.
///
/// Round-trips through `serde_json` as the tagged shape the schema
/// declares (`{"kind": "gpui_view", …}` / `{"kind": "iframe", …}`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelSource {
    /// First-party panel rendered as a gpui View.  `factory` is the
    /// path-like string the build-time aggregator resolves to a real
    /// Rust call (registered in `factories.rs`).  Format is
    /// `"crate_name::Type::method"` — matches the plan §5.1 example.
    GpuiView { factory: String },
    /// Iframe panel.  `url` must pass `is_loopback_url` (parser
    /// enforces this).  `sandbox` and `health_check` are optional
    /// extension surface declared early so a future slice can ship
    /// them without bumping the schema version again.
    Iframe {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        health_check: Option<String>,
    },
}

/// Extension-reported panel, the shape the runtime overlay accepts.
///
/// Distinct from `PanelEntry` because extension panels are
/// iframe-only (the extension bridge can't ship gpui View factories
/// over IPC) and they always carry an `extension_id` for origin
/// tracking.  Bridged from `extensions.list_panels` by the Shell.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ExtensionPanel {
    pub extension_id: String,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub order: i32,
    pub version: String,
    /// Iframe URL.  Same loopback rule as first-party iframe panels.
    pub url: String,
}

/// Parse a `manifest.json` file's contents into a `PanelManifest`,
/// enforcing the v2 invariants:
///
///   * `schema_version == 2`
///   * every `Iframe` panel has a loopback URL
///   * panel ids are unique within the manifest
///   * the manifest's `service` is non-empty
///
/// Returns a deserialisation/validation error otherwise.  Callers (the
/// aggregator + tests) feed file contents in directly so the function
/// stays IO-free.
pub fn parse_panel_manifest(json: &str) -> anyhow::Result<PanelManifest> {
    let m: PanelManifest = serde_json::from_str(json)?;
    if m.schema_version != SCHEMA_VERSION {
        anyhow::bail!(
            "panel manifest schema_version is {}, expected {SCHEMA_VERSION} \
             (Phase 12.7 v1 manifests are no longer supported — see plan §12.10)",
            m.schema_version,
        );
    }
    if m.service.trim().is_empty() {
        anyhow::bail!("panel manifest `service` is empty");
    }
    let mut seen = std::collections::HashSet::new();
    for p in &m.panels {
        if p.id.trim().is_empty() {
            anyhow::bail!("panel entry missing `id`");
        }
        if p.title.trim().is_empty() {
            anyhow::bail!("panel entry `{}` missing `title`", p.id);
        }
        if p.version.trim().is_empty() {
            anyhow::bail!("panel entry `{}` missing `version`", p.id);
        }
        if !seen.insert(p.id.clone()) {
            anyhow::bail!("duplicate panel id `{}` within manifest", p.id);
        }
        match &p.source {
            PanelSource::Iframe { url, .. } => {
                if !is_loopback_url(url) {
                    anyhow::bail!(
                        "panel `{}` iframe url `{}` is not loopback — only http(s) \
                         URLs on 127.0.0.1, localhost, or [::1] are allowed",
                        p.id,
                        url,
                    );
                }
            }
            PanelSource::GpuiView { factory } => {
                if factory.trim().is_empty() {
                    anyhow::bail!("panel `{}` gpui_view source missing `factory`", p.id);
                }
                // Sanity: factory paths use `::` as the path separator.
                // A bare identifier with no `::` almost certainly means
                // a typo (`"settings_panel"` instead of
                // `"wylde_panel_settings::SettingsPanel::view"`).
                if !factory.contains("::") {
                    anyhow::bail!(
                        "panel `{}` factory `{}` doesn't look path-like (expected \
                         `crate::Type::method`)",
                        p.id,
                        factory,
                    );
                }
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS_JSON: &str = r#"{
        "schema_version": 2,
        "service": "core",
        "panels": [
            {
                "id": "settings",
                "title": "Settings",
                "icon": "settings",
                "order": 95,
                "version": "0.1.0",
                "source": {
                    "kind": "gpui_view",
                    "factory": "wylde_panel_settings::SettingsPanel::view"
                }
            }
        ]
    }"#;

    const IFRAME_JSON: &str = r#"{
        "schema_version": 2,
        "service": "workflows",
        "panels": [
            {
                "id": "editor",
                "title": "Workflows",
                "order": 50,
                "version": "0.1.0",
                "source": {
                    "kind": "iframe",
                    "url": "http://127.0.0.1:5678",
                    "sandbox": "allow-scripts allow-same-origin"
                }
            }
        ]
    }"#;

    #[test]
    fn round_trip_first_party_manifest() {
        let m = parse_panel_manifest(SETTINGS_JSON).expect("parse first-party manifest");
        assert_eq!(m.schema_version, SCHEMA_VERSION);
        assert_eq!(m.service, "core");
        assert_eq!(m.panels.len(), 1);
        // Re-serialise + reparse → byte-for-byte stable structure.
        let blob = serde_json::to_string(&m).unwrap();
        let again = parse_panel_manifest(&blob).expect("reparse own serialisation");
        assert_eq!(m, again);
    }

    #[test]
    fn round_trip_iframe_manifest() {
        let m = parse_panel_manifest(IFRAME_JSON).expect("parse iframe manifest");
        assert_eq!(m.panels.len(), 1);
        assert!(matches!(m.panels[0].source, PanelSource::Iframe { .. }));
        let blob = serde_json::to_string(&m).unwrap();
        let again = parse_panel_manifest(&blob).expect("reparse own serialisation");
        assert_eq!(m, again);
    }

    #[test]
    fn rejects_schema_version_one() {
        let json = r#"{"schema_version": 1, "service":"core", "panels":[]}"#;
        let err = parse_panel_manifest(json).unwrap_err();
        assert!(
            err.to_string().contains("schema_version is 1"),
            "expected schema_version error, got: {err}"
        );
    }

    #[test]
    fn rejects_custom_element_source() {
        // What a stray v1 source looks like — must fail at parse time.
        let json = r#"{
            "schema_version": 2,
            "service": "core",
            "panels": [{
                "id": "x", "title": "X", "order": 0, "version": "0.1.0",
                "source": {"kind": "custom_element", "tag": "x-panel"}
            }]
        }"#;
        let err = parse_panel_manifest(json).unwrap_err();
        // `serde` rejects the unknown variant — message contains
        // `custom_element` and `gpui_view`/`iframe`.
        assert!(
            err.to_string().contains("custom_element")
                || err.to_string().contains("unknown variant"),
            "expected unknown-variant error, got: {err}",
        );
    }

    #[test]
    fn rejects_non_loopback_iframe() {
        let json = r#"{
            "schema_version": 2,
            "service": "evil",
            "panels": [{
                "id": "bad", "title": "Bad", "order": 0, "version": "0.1.0",
                "source": {"kind": "iframe", "url": "http://attacker.example/"}
            }]
        }"#;
        let err = parse_panel_manifest(json).unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "expected loopback validation, got: {err}",
        );
    }

    #[test]
    fn rejects_factory_without_path_separator() {
        let json = r#"{
            "schema_version": 2,
            "service": "core",
            "panels": [{
                "id": "x", "title": "X", "order": 0, "version": "0.1.0",
                "source": {"kind": "gpui_view", "factory": "settings_panel"}
            }]
        }"#;
        let err = parse_panel_manifest(json).unwrap_err();
        assert!(
            err.to_string().contains("path-like"),
            "expected path-like error, got: {err}",
        );
    }

    #[test]
    fn rejects_duplicate_panel_ids_within_manifest() {
        let json = r#"{
            "schema_version": 2,
            "service": "core",
            "panels": [
                {"id":"dup","title":"A","order":0,"version":"0.1.0",
                 "source":{"kind":"gpui_view","factory":"a::B::c"}},
                {"id":"dup","title":"B","order":1,"version":"0.1.0",
                 "source":{"kind":"gpui_view","factory":"a::B::d"}}
            ]
        }"#;
        let err = parse_panel_manifest(json).unwrap_err();
        assert!(
            err.to_string().contains("duplicate panel id"),
            "expected duplicate-id error, got: {err}",
        );
    }

    #[test]
    fn rejects_empty_service_field() {
        let json = r#"{"schema_version":2,"service":"","panels":[]}"#;
        let err = parse_panel_manifest(json).unwrap_err();
        assert!(err.to_string().contains("service"));
    }
}
