//! Runtime overlay — the per-call union of the static, compiled-in
//! registry and the extension-side `extensions.list_panels` payload.
//!
//! The static registry is read once at startup (the generated source
//! emits `register_internal` calls into the process-wide
//! `PanelRegistry`).  Extension panels are fluid: a user can install or
//! uninstall an extension between two `gui.list_tabs` calls, so we
//! query them every time and union here.
//!
//! Pure function — no IPC, no IO.  The Shell fetches the extension
//! list and hands it in.

use crate::manifest::ExtensionPanel;
use crate::registry::PanelRegistry;

/// Build the unified JSON for one `gui.list_tabs` reply.
///
/// `extensions` is what the Shell got back from
/// `extensions.list_panels` after dropping any non-loopback URLs (the
/// bridge enforces loopback on its side too, but the GUI defends in
/// depth — see `filter_extension_panels`).
pub fn union_for_runtime(
    registry: &PanelRegistry,
    extensions: &[ExtensionPanel],
) -> serde_json::Value {
    let filtered: Vec<ExtensionPanel> = filter_extension_panels(extensions);
    registry.snapshot_json(&filtered)
}

/// Drop extension panels whose URL isn't loopback.  The extension
/// bridge already enforces this at registration time but the GUI keeps
/// its own check so a regression on the backend can't punch a hole in
/// the renderer.  Mirrors `loopback::is_loopback_url`.
pub fn filter_extension_panels(panels: &[ExtensionPanel]) -> Vec<ExtensionPanel> {
    panels
        .iter()
        .filter(|p| crate::loopback::is_loopback_url(&p.url))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PanelEntry, PanelOrigin, PanelSource};
    use crate::registry::{PanelRegistry, RegistryRow};

    fn first_party(service: &str, id: &str, order: i32) -> RegistryRow {
        RegistryRow {
            origin: PanelOrigin::FirstParty {
                service: service.into(),
            },
            entry: PanelEntry {
                id: id.into(),
                title: id.into(),
                icon: None,
                order,
                version: "0.1.0".into(),
                required_services: vec![],
                source: PanelSource::GpuiView {
                    factory: format!("c::T::{id}"),
                },
            },
            factory: None,
        }
    }

    fn ext(extension_id: &str, id: &str, order: i32, url: &str) -> ExtensionPanel {
        ExtensionPanel {
            extension_id: extension_id.into(),
            id: id.into(),
            title: id.into(),
            icon: None,
            order,
            version: "0.0.1".into(),
            url: url.into(),
        }
    }

    #[test]
    fn union_orders_by_order_then_key() {
        let mut r = PanelRegistry::new();
        r.register_internal(first_party("core", "settings", 95)).unwrap();
        r.register_internal(first_party("core", "chat", 10)).unwrap();
        let exts = vec![
            ext("n8n", "editor", 50, "http://127.0.0.1:5678"),
            ext("photos", "main", 5, "http://localhost:9300"),
        ];
        let union = union_for_runtime(&r, &exts);
        let tabs = union["tabs"].as_array().unwrap();
        let ordered: Vec<_> = tabs
            .iter()
            .map(|t| t["registry_key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ordered,
            vec![
                "ext:photos/main".to_string(),  // order  5
                "core/chat".to_string(),         // order 10
                "ext:n8n/editor".to_string(),    // order 50
                "core/settings".to_string(),     // order 95
            ]
        );
    }

    #[test]
    fn filter_drops_non_loopback_extension_panels() {
        let exts = vec![
            ext("good", "x", 0, "http://127.0.0.1:1"),
            ext("evil", "y", 0, "http://attacker.example/"),
            ext("spoof", "z", 0, "http://127.0.0.1.evil.com/"),
        ];
        let kept = filter_extension_panels(&exts);
        let ids: Vec<_> = kept.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, vec!["x".to_string()]);
    }

    #[test]
    fn empty_extension_list_returns_registry_only() {
        let mut r = PanelRegistry::new();
        r.register_internal(first_party("core", "settings", 95)).unwrap();
        let union = union_for_runtime(&r, &[]);
        assert_eq!(union["tabs"].as_array().unwrap().len(), 1);
    }
}
