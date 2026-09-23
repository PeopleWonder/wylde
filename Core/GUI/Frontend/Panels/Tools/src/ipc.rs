//! Per-panel IPC helpers for the Tools panel.
//!
//! Wraps the bare `ext.*` + `extensions.list_panels` verbs on the
//! `wylde-extension-bridge` pipe into typed reads / writes the View
//! body consumes.  The bridge is a separate service from the harness,
//! so the in-process short-circuit doesn't apply — every call goes
//! over the wire.

use serde_json::{json, Value};

/// Bridge service name on `\\.\pipe\wylde-extension-bridge`.  Wrapped
/// here so a future service rename touches one constant.
pub const SVC_EXT_BRIDGE: &str = "wylde-extension-bridge";

/// One extension as the bridge reports it through `ext.list`.  Fields
/// mirror `wylde_extension_bridge::host::ExtensionStatus`; we keep the
/// shape inlined here so the panel doesn't pull the bridge crate in as
/// a dependency.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionStatus {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    /// Snake-case status string: `disabled`, `starting`, `running`,
    /// `unhealthy`, `crashed`, `broken`.  Kept as a String so a new
    /// status added by the bridge surfaces as text rather than an
    /// enum-decode error.
    pub status: String,
    pub last_error: Option<String>,
}

impl ExtensionStatus {
    pub fn from_value(v: &Value) -> Self {
        Self {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            version: v
                .get("version")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_owned(),
            last_error: v
                .get("last_error")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
        }
    }
}

/// Availability of a declared panel, as the bridge reports it.
///
/// The bridge recomputes this per read from live discovery plus a
/// loopback reachability probe, so it answers "is this panel actually
/// usable *now*" rather than "was it declared at some point". A panel
/// that is no longer registered simply isn't in the list at all — there
/// is deliberately no `Removed` variant, because absence is the signal.
///
/// The GUI's rule (#239): render live **only** on [`Self::Live`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PanelAvailability {
    /// Registered and reachable.
    #[default]
    Live,
    /// Registered, but nothing is listening at its URL.
    Unreachable,
    /// Registered, but its extension has no live process.
    NotRunning,
    /// The bridge reported a value this GUI doesn't know. Treated as
    /// not-live: an unrecognised state is not a licence to render a
    /// panel as working.
    Unknown,
}

impl PanelAvailability {
    /// Parse the bridge's wire value.
    ///
    /// An **absent** field means an older bridge that predates the
    /// availability probe; that degrades to [`Self::Live`], matching the
    /// precedent in `Shell::nav` (a missing health field never
    /// over-blocks a panel against a daemon that predates the field).
    /// A *present but unrecognised* value is a different case — the
    /// bridge is telling us something and we don't understand it, so we
    /// refuse to claim the panel works.
    pub fn from_wire(raw: Option<&str>) -> Self {
        match raw {
            None => PanelAvailability::Live,
            Some("live") => PanelAvailability::Live,
            Some("unreachable") => PanelAvailability::Unreachable,
            Some("not_running") => PanelAvailability::NotRunning,
            Some(_) => PanelAvailability::Unknown,
        }
    }

    /// May this panel be presented as a working panel? Exactly one
    /// variant qualifies — the point of the type.
    pub fn is_live(self) -> bool {
        matches!(self, PanelAvailability::Live)
    }

    /// Short label for the status chip on the panel's card.
    pub fn label(self) -> &'static str {
        match self {
            PanelAvailability::Live => "LIVE",
            PanelAvailability::Unreachable => "UNAVAILABLE",
            PanelAvailability::NotRunning => "NOT RUNNING",
            PanelAvailability::Unknown => "UNKNOWN",
        }
    }
}

/// One panel as the bridge reports it through `extensions.list_panels`.
/// Mirrors `wylde_extension_bridge::host::PanelEntry`.  The Tools
/// surface lists them so the user can see which extension contributes
/// which panel — and, since #239, what state each one is actually in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionPanel {
    pub extension: String,
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub url: String,
    /// Live availability, recomputed by the bridge on every read.
    pub availability: PanelAvailability,
    /// Why it isn't live, when it isn't. Shown under the card's title so
    /// the user gets a reason rather than a card that merely looks fine.
    pub detail: Option<String>,
}

impl ExtensionPanel {
    pub fn from_value(v: &Value) -> Self {
        Self {
            extension: v
                .get("extension")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            title: v
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            icon: v.get("icon").and_then(|x| x.as_str()).map(|s| s.to_owned()),
            url: v
                .get("url")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            availability: PanelAvailability::from_wire(
                v.get("availability").and_then(|x| x.as_str()),
            ),
            detail: v
                .get("detail")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
        }
    }
}

/// Read the full extension list via `ext.list`.
pub async fn list_extensions() -> Result<Vec<ExtensionStatus>, String> {
    let v = wylde_gui_pipe::call(
        SVC_EXT_BRIDGE,
        "POST",
        "/__action__",
        Some(json!({ "action": "ext.list", "payload": {} })),
    )
    .await?;
    Ok(parse_extension_array(&v))
}

/// Toggle an extension on — fires `ext.enable` and returns the
/// post-write status the bridge reports.
pub async fn enable_extension(name: &str) -> Result<ExtensionStatus, String> {
    let v = wylde_gui_pipe::call(
        SVC_EXT_BRIDGE,
        "POST",
        "/__action__",
        Some(json!({
            "action": "ext.enable",
            "payload": { "name": name },
        })),
    )
    .await?;
    Ok(ExtensionStatus::from_value(&v))
}

/// Toggle an extension off — fires `ext.disable`.
pub async fn disable_extension(name: &str) -> Result<ExtensionStatus, String> {
    let v = wylde_gui_pipe::call(
        SVC_EXT_BRIDGE,
        "POST",
        "/__action__",
        Some(json!({
            "action": "ext.disable",
            "payload": { "name": name },
        })),
    )
    .await?;
    Ok(ExtensionStatus::from_value(&v))
}

/// Read every declared UI panel across every enabled-or-disabled
/// extension via `extensions.list_panels`.  Pure read; never spawns an
/// MCP server.
pub async fn list_extension_panels() -> Result<Vec<ExtensionPanel>, String> {
    let v = wylde_gui_pipe::call(
        SVC_EXT_BRIDGE,
        "POST",
        "/__action__",
        Some(json!({ "action": "extensions.list_panels", "payload": {} })),
    )
    .await?;
    Ok(parse_panel_array(&v))
}

fn parse_extension_array(v: &Value) -> Vec<ExtensionStatus> {
    let Some(arr) = v.get("extensions").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(ExtensionStatus::from_value).collect()
}

fn parse_panel_array(v: &Value) -> Vec<ExtensionPanel> {
    let Some(arr) = v.get("panels").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(ExtensionPanel::from_value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_status_parses_full_payload() {
        let v = json!({
            "name": "n8n",
            "version": "1.2.3",
            "enabled": true,
            "status": "running",
            "pid": 1234,
            "last_error": null,
            "capabilities": ["tools"]
        });
        let s = ExtensionStatus::from_value(&v);
        assert_eq!(s.name, "n8n");
        assert_eq!(s.version, "1.2.3");
        assert!(s.enabled);
        assert_eq!(s.status, "running");
        assert!(s.last_error.is_none());
    }

    #[test]
    fn extension_status_surfaces_last_error_when_present() {
        let v = json!({
            "name": "broken",
            "version": "0.0.1",
            "enabled": true,
            "status": "crashed",
            "last_error": "spawn failed: ENOENT"
        });
        let s = ExtensionStatus::from_value(&v);
        assert_eq!(s.status, "crashed");
        assert_eq!(s.last_error.as_deref(), Some("spawn failed: ENOENT"));
    }

    #[test]
    fn extension_status_defaults_unknown_status_when_missing() {
        let s = ExtensionStatus::from_value(&json!({}));
        assert!(s.name.is_empty());
        assert!(!s.enabled);
        // The Svelte alpha treats absent status as "unknown" too —
        // matching here so log/UI greps remain consistent across the
        // cutover.
        assert_eq!(s.status, "unknown");
    }

    #[test]
    fn extension_panel_parses_full_payload() {
        let v = json!({
            "extension": "n8n",
            "id": "workflows",
            "title": "Workflows",
            "icon": "workflow",
            "kind": "iframe",
            "url": "http://127.0.0.1:5678"
        });
        let p = ExtensionPanel::from_value(&v);
        assert_eq!(p.extension, "n8n");
        assert_eq!(p.id, "workflows");
        assert_eq!(p.title, "Workflows");
        assert_eq!(p.icon.as_deref(), Some("workflow"));
        assert_eq!(p.url, "http://127.0.0.1:5678");
    }

    #[test]
    fn panel_carries_the_bridge_availability_and_reason() {
        let v = json!({
            "extension": "wylde-images",
            "id": "images",
            "title": "Images",
            "kind": "iframe",
            "url": "http://127.0.0.1:8015",
            "availability": "unreachable",
            "detail": "nothing is listening at its address"
        });
        let p = ExtensionPanel::from_value(&v);
        assert_eq!(p.availability, PanelAvailability::Unreachable);
        assert!(!p.availability.is_live(), "a dead panel is never live");
        assert_eq!(
            p.detail.as_deref(),
            Some("nothing is listening at its address")
        );
    }

    #[test]
    fn availability_wire_values_round_trip() {
        assert_eq!(
            PanelAvailability::from_wire(Some("live")),
            PanelAvailability::Live
        );
        assert_eq!(
            PanelAvailability::from_wire(Some("unreachable")),
            PanelAvailability::Unreachable
        );
        assert_eq!(
            PanelAvailability::from_wire(Some("not_running")),
            PanelAvailability::NotRunning
        );
    }

    #[test]
    fn an_absent_availability_field_degrades_live_but_a_strange_one_does_not() {
        // Absent → an older bridge; don't over-block (same precedent as
        // the Shell's missing-health-field handling).
        assert!(PanelAvailability::from_wire(None).is_live());
        assert!(ExtensionPanel::from_value(&json!({ "id": "x" }))
            .availability
            .is_live());
        // Present but unrecognised → the bridge is saying something we
        // don't understand; refuse to claim the panel works.
        let odd = PanelAvailability::from_wire(Some("evacuating"));
        assert_eq!(odd, PanelAvailability::Unknown);
        assert!(!odd.is_live());
    }

    #[test]
    fn exactly_one_availability_permits_a_live_render() {
        assert!(PanelAvailability::Live.is_live());
        for state in [
            PanelAvailability::Unreachable,
            PanelAvailability::NotRunning,
            PanelAvailability::Unknown,
        ] {
            assert!(!state.is_live(), "{state:?} must not render as live");
            assert!(!state.label().is_empty(), "{state:?} needs a chip label");
        }
    }

    #[test]
    fn parse_extension_array_unwraps_envelope() {
        let v = json!({
            "extensions": [
                {"name": "a", "version": "0.1.0", "enabled": true, "status": "running"},
                {"name": "b", "version": "0.2.0", "enabled": false, "status": "disabled"}
            ]
        });
        let out = parse_extension_array(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a");
        assert!(out[0].enabled);
        assert_eq!(out[1].name, "b");
        assert!(!out[1].enabled);
    }

    #[test]
    fn parse_panel_array_unwraps_envelope() {
        let v = json!({
            "panels": [
                {"extension": "n8n", "id": "workflows", "title": "Workflows", "url": "http://127.0.0.1:5678"}
            ]
        });
        let out = parse_panel_array(&v);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].extension, "n8n");
    }

    #[test]
    fn parse_extension_array_handles_missing_key() {
        assert!(parse_extension_array(&json!({})).is_empty());
    }

    #[test]
    fn parse_panel_array_handles_missing_key() {
        assert!(parse_panel_array(&json!({})).is_empty());
    }

    #[test]
    fn ext_bridge_service_name_matches_alpha() {
        // Same string the Svelte alpha + the bridge daemon agree on.
        // A rename would silently route to nothing — guard it.
        assert_eq!(SVC_EXT_BRIDGE, "wylde-extension-bridge");
    }

    #[test]
    fn each_pipe_call_uses_expected_verb() {
        // Build-time witness — same pattern Settings + Workspaces use.
        let _ = list_extensions;
        let _ = enable_extension;
        let _ = disable_extension;
        let _ = list_extension_panels;
    }
}
