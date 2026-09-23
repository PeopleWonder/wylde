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
///
/// Note what this does *not* drop: an unreachable panel. A panel whose
/// registration is gone never arrives here (the bridge re-walks the
/// filesystem per read, so it simply isn't in the list), but one that is
/// registered and dead must still be listed — with its status — rather
/// than silently disappearing. Vanishing and broken are different facts
/// and the user is owed the difference (#239).
pub fn filter_extension_panels(panels: &[ExtensionPanel]) -> Vec<ExtensionPanel> {
    panels
        .iter()
        .filter(|p| crate::loopback::is_loopback_url(&p.url))
        .cloned()
        .collect()
}

/// The subset of `panels` that may be rendered as working panels.
///
/// The one place a caller should ask "can I mount this?". Keeping it a
/// named function rather than an inline `== "live"` is the point: adding
/// a new availability state can't silently start counting as live.
pub fn live_extension_panels(panels: &[ExtensionPanel]) -> Vec<ExtensionPanel> {
    panels
        .iter()
        .filter(|p| crate::manifest::availability_is_live(&p.availability))
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
            availability: "live".into(),
        }
    }

    #[test]
    fn union_orders_by_order_then_key() {
        let mut r = PanelRegistry::new();
        r.register_internal(first_party("core", "settings", 95))
            .unwrap();
        r.register_internal(first_party("core", "chat", 10))
            .unwrap();
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
                "ext:photos/main".to_string(), // order  5
                "core/chat".to_string(),       // order 10
                "ext:n8n/editor".to_string(),  // order 50
                "core/settings".to_string(),   // order 95
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

    fn ext_with(extension_id: &str, id: &str, url: &str, availability: &str) -> ExtensionPanel {
        ExtensionPanel {
            availability: availability.into(),
            ..ext(extension_id, id, 0, url)
        }
    }

    #[test]
    fn availability_reaches_the_wire_so_no_consumer_has_to_guess() {
        let mut r = PanelRegistry::new();
        r.register_internal(first_party("core", "settings", 95))
            .unwrap();
        let exts = vec![ext_with(
            "wylde-images",
            "images",
            "http://127.0.0.1:8015",
            "unreachable",
        )];
        let union = union_for_runtime(&r, &exts);
        let tabs = union["tabs"].as_array().unwrap();
        let images = tabs
            .iter()
            .find(|t| t["registry_key"] == "ext:wylde-images/images")
            .expect("the dead panel is still listed");
        assert_eq!(
            images["availability"], "unreachable",
            "the tab list carries the panel's real state — a consumer \
             cannot mount it without seeing this"
        );
        // First-party rows are gated by `required_services` instead, so
        // they carry no availability rather than a misleading default.
        let settings = tabs
            .iter()
            .find(|t| t["registry_key"] == "core/settings")
            .unwrap();
        assert!(settings.get("availability").is_none());
    }

    #[test]
    fn only_a_live_panel_is_offered_for_mounting() {
        let panels = vec![
            ext_with("n8n", "editor", "http://127.0.0.1:5678", "live"),
            ext_with(
                "wylde-images",
                "images",
                "http://127.0.0.1:8015",
                "unreachable",
            ),
            ext_with("some-ext", "p", "http://127.0.0.1:9000", "not_running"),
            // A state this build doesn't know must not count as live.
            ext_with("future-ext", "p", "http://127.0.0.1:9100", "quiescent"),
        ];
        let live: Vec<String> = live_extension_panels(&panels)
            .iter()
            .map(|p| p.extension_id.clone())
            .collect();
        assert_eq!(live, vec!["n8n".to_string()]);
        // But every one of them survives the loopback filter — dead is
        // not the same as gone, and the list still has to show them.
        assert_eq!(filter_extension_panels(&panels).len(), 4);
    }

    #[test]
    fn a_reply_without_availability_defaults_to_live() {
        // An older bridge that predates the probe must not blank the GUI.
        let raw = serde_json::json!({
            "extension_id": "n8n",
            "id": "editor",
            "title": "Workflows",
            "order": 50,
            "version": "0.0.1",
            "url": "http://127.0.0.1:5678"
        });
        let parsed: ExtensionPanel = serde_json::from_value(raw).expect("parses without the field");
        assert_eq!(parsed.availability, "live");
        assert_eq!(live_extension_panels(&[parsed]).len(), 1);
    }

    #[test]
    fn empty_extension_list_returns_registry_only() {
        let mut r = PanelRegistry::new();
        r.register_internal(first_party("core", "settings", 95))
            .unwrap();
        let union = union_for_runtime(&r, &[]);
        assert_eq!(union["tabs"].as_array().unwrap().len(), 1);
    }
}
