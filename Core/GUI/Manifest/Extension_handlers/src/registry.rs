//! Runtime panel registry.
//!
//! The registry is "load once at startup, read often."  The generated
//! source emitted by `wylde-panel-aggregator` calls `register_internal`
//! during process bootstrap; from then on the registry is read-only
//! through `entries()` and per-call overlays for extension panels live
//! in `overlay::union_for_runtime`.
//!
//! The factory closures (`ViewFactory`) live alongside the entries.
//! Each closure is `Send + Sync` because gpui can construct views from
//! any window thread.  No mutex on the registry itself — the
//! once-and-forever bootstrap pattern means readers never race writers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Serialize;

use crate::manifest::{ExtensionPanel, PanelEntry, PanelOrigin, PanelSource};

/// View factory signature.  Mirrors the plan §5.2 design — gpui calls
/// the closure to mint an `AnyView` for the panel slot.  `Send + Sync`
/// so gpui can dispatch the factory off whichever thread holds the
/// `App` at the moment.
pub type ViewFactory =
    Box<dyn Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + Send + Sync>;

/// One row in the runtime registry.  Carries the manifest declaration
/// plus the resolved factory (for first-party panels).
pub struct RegistryRow {
    pub origin: PanelOrigin,
    pub entry: PanelEntry,
    /// Resolved factory.  Some for first-party `gpui_view` panels;
    /// None for iframe panels (those render via the WebView path in a
    /// later slice) and for extension panels (which are iframe-only).
    pub factory: Option<ViewFactory>,
}

impl std::fmt::Debug for RegistryRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryRow")
            .field("origin", &self.origin)
            .field("entry", &self.entry)
            .field("factory", &self.factory.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

/// Why a registration failed.  Surfaced both at startup (panicking is
/// fine — the binary can't run without a valid registry) and from the
/// aggregator tests (which prefer a typed error).
///
/// Implementing `Display`/`Error` by hand keeps the registry crate's
/// dep graph small (no `thiserror`).
#[derive(Debug)]
pub enum RegistryError {
    DuplicateKey(String),
    /// The generated source named a factory string that no entry in
    /// `FactoryMap` resolved.  Means `factories.rs` and `generated.rs`
    /// have drifted — the binary refuses to start so the broken panel
    /// surfaces immediately rather than as a silent missing tab.
    MissingFactory(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::DuplicateKey(k) => write!(
                f,
                "duplicate panel registry key `{k}` — another panel already claimed it",
            ),
            RegistryError::MissingFactory(k) => write!(
                f,
                "missing factory wiring for `{k}` — add an entry in factories.rs",
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Process-wide registry.  Built once by
/// `crate::generated::register_all` at startup and stashed in a
/// `OnceLock` so every consumer reads the same instance.
pub struct PanelRegistry {
    rows: BTreeMap<String, RegistryRow>,
}

static GLOBAL: OnceLock<PanelRegistry> = OnceLock::new();

impl Default for PanelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PanelRegistry {
    pub fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
        }
    }

    /// Register a panel.  Returns `DuplicateKey` if the same
    /// `service/id` pair was already registered — the generated file
    /// must produce a clean tree.
    pub fn register_internal(&mut self, row: RegistryRow) -> Result<(), RegistryError> {
        let key = registry_key(&row.origin, &row.entry.id);
        if self.rows.contains_key(&key) {
            return Err(RegistryError::DuplicateKey(key));
        }
        self.rows.insert(key, row);
        Ok(())
    }

    /// All registered rows, ordered by `order` ASC then `service/id`.
    pub fn entries(&self) -> Vec<&RegistryRow> {
        let mut out: Vec<_> = self.rows.values().collect();
        out.sort_by(|a, b| {
            a.entry
                .order
                .cmp(&b.entry.order)
                .then_with(|| origin_service(&a.origin).cmp(origin_service(&b.origin)))
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });
        out
    }

    /// Number of registered rows.  Stable across reads.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Install this registry as the process-wide one.  Fails (silently
    /// — returns `false`) on the second call so a misbehaving test
    /// doesn't trip an unrelated test that already installed the real
    /// registry.  The Shell calls this exactly once during startup.
    pub fn install_global(self) -> bool {
        GLOBAL.set(self).is_ok()
    }

    /// Read the process-wide registry.  Returns `None` if no registry
    /// was installed (acceptable for unit tests that don't need it).
    pub fn global() -> Option<&'static PanelRegistry> {
        GLOBAL.get()
    }

    /// Build a JSON snapshot of the registry, optionally merged with
    /// an extension-panel overlay.  This is the wire shape for
    /// `gui.list_tabs` and the test-harness comparison surface.
    pub fn snapshot_json(&self, extensions: &[ExtensionPanel]) -> serde_json::Value {
        let mut rows: Vec<RegistryRowSnapshot> = self
            .entries()
            .iter()
            .map(|r| RegistryRowSnapshot {
                registry_key: registry_key(&r.origin, &r.entry.id),
                origin: origin_snapshot(&r.origin),
                title: r.entry.title.clone(),
                icon: r.entry.icon.clone(),
                order: r.entry.order,
                version: r.entry.version.clone(),
                required_services: r.entry.required_services.clone(),
                source: source_snapshot(&r.entry.source),
            })
            .collect();
        for e in extensions {
            rows.push(RegistryRowSnapshot {
                registry_key: format!("ext:{}/{}", e.extension_id, e.id),
                origin: "extension",
                title: e.title.clone(),
                icon: e.icon.clone(),
                order: e.order,
                version: e.version.clone(),
                required_services: Vec::new(),
                source: SourceSnapshot::Iframe { url: e.url.clone() },
            });
        }
        rows.sort_by(|a, b| {
            a.order
                .cmp(&b.order)
                .then_with(|| a.registry_key.cmp(&b.registry_key))
        });
        serde_json::json!({ "tabs": rows })
    }
}

/// Compute the canonical registry key for a row.
fn registry_key(origin: &PanelOrigin, id: &str) -> String {
    match origin {
        PanelOrigin::FirstParty { service } => format!("{service}/{id}"),
        PanelOrigin::Extension { extension_id } => format!("ext:{extension_id}/{id}"),
    }
}

fn origin_service(origin: &PanelOrigin) -> &str {
    match origin {
        PanelOrigin::FirstParty { service } => service,
        PanelOrigin::Extension { extension_id } => extension_id,
    }
}

fn origin_snapshot(origin: &PanelOrigin) -> &'static str {
    match origin {
        PanelOrigin::FirstParty { .. } => "first_party",
        PanelOrigin::Extension { .. } => "extension",
    }
}

fn source_snapshot(source: &PanelSource) -> SourceSnapshot {
    match source {
        PanelSource::GpuiView { factory } => SourceSnapshot::GpuiView {
            factory: factory.clone(),
        },
        PanelSource::Iframe { url, .. } => SourceSnapshot::Iframe { url: url.clone() },
    }
}

#[derive(Serialize)]
struct RegistryRowSnapshot {
    registry_key: String,
    origin: &'static str,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    order: i32,
    version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_services: Vec<String>,
    source: SourceSnapshot,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceSnapshot {
    GpuiView { factory: String },
    Iframe { url: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PanelEntry, PanelSource};

    fn row(service: &str, id: &str, order: i32) -> RegistryRow {
        RegistryRow {
            origin: PanelOrigin::FirstParty {
                service: service.into(),
            },
            entry: PanelEntry {
                id: id.into(),
                title: format!("{id} title"),
                icon: None,
                order,
                version: "0.1.0".into(),
                required_services: vec![],
                source: PanelSource::GpuiView {
                    factory: format!("crate_{service}::Panel::view"),
                },
            },
            factory: None,
        }
    }

    #[test]
    fn register_then_list_orders_by_order_field() {
        let mut r = PanelRegistry::new();
        r.register_internal(row("core", "chat", 10)).unwrap();
        r.register_internal(row("core", "settings", 95)).unwrap();
        r.register_internal(row("memory", "long_term", 30)).unwrap();
        let entries: Vec<_> = r
            .entries()
            .iter()
            .map(|x| (x.entry.id.clone(), x.entry.order))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("chat".into(), 10),
                ("long_term".into(), 30),
                ("settings".into(), 95),
            ]
        );
    }

    #[test]
    fn duplicate_registry_key_is_a_clear_error() {
        let mut r = PanelRegistry::new();
        r.register_internal(row("core", "settings", 95)).unwrap();
        let err = r
            .register_internal(row("core", "settings", 100))
            .expect_err("second register should fail");
        match err {
            RegistryError::DuplicateKey(k) => assert_eq!(k, "core/settings"),
            other => panic!("expected DuplicateKey, got {other:?}"),
        }
        // `Display` impl includes both the offending key and a hint.
        let msg = format!("{}", RegistryError::DuplicateKey("core/settings".into()));
        assert!(msg.contains("core/settings"));
        assert!(msg.contains("duplicate"));
    }

    #[test]
    fn snapshot_json_round_trip_through_extensions() {
        let mut r = PanelRegistry::new();
        r.register_internal(row("core", "settings", 95)).unwrap();
        let ext = ExtensionPanel {
            extension_id: "n8n".into(),
            id: "editor".into(),
            title: "Workflows".into(),
            icon: Some("workflow".into()),
            order: 50,
            version: "0.0.1".into(),
            url: "http://127.0.0.1:5678".into(),
        };
        let snap = r.snapshot_json(&[ext]);
        let tabs = snap["tabs"].as_array().expect("tabs is array");
        assert_eq!(tabs.len(), 2);
        // Order 50 (extension) sits before order 95 (settings).
        assert_eq!(tabs[0]["registry_key"], "ext:n8n/editor");
        assert_eq!(tabs[0]["origin"], "extension");
        assert_eq!(tabs[1]["registry_key"], "core/settings");
        assert_eq!(tabs[1]["origin"], "first_party");
        // Source tag round-trips.
        assert_eq!(tabs[0]["source"]["kind"], "iframe");
        assert_eq!(tabs[1]["source"]["kind"], "gpui_view");
    }

    #[test]
    fn empty_registry_serialises_cleanly() {
        let r = PanelRegistry::new();
        let snap = r.snapshot_json(&[]);
        assert_eq!(snap["tabs"].as_array().unwrap().len(), 0);
    }
}
