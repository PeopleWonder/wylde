//! Navigation model — pure data shaping for the sidebar + slot.
//!
//! The Shell View holds a `NavModel` and renders against it; this
//! module is the test-friendly half of the wiring (no gpui types
//! reach it), so the selection / health policy is unit-testable
//! without a window.
//!
//! The Shell's interactive layer maps clicks → `select(key)` and
//! pipe replies → `mark_service_health(name, healthy)`; the panel
//! slot reads `slot_state(...)` once per render to decide what to
//! mount.

use std::collections::{BTreeMap, BTreeSet};

/// One displayable row in the sidebar.  Mirrors a `PanelRegistry`
/// entry without the gpui factory closure — purely the JSON-shaped
/// metadata callers need to render a row + decide what to mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavRow {
    /// Registry key (`core/settings`, `core/workspaces`, `ext:n8n/editor`).
    /// Stable across renders; the Shell's `selected_key` matches against it.
    pub key: String,
    /// `first_party` or `extension` — used so the sidebar can put a
    /// subtle differentiator on extension panels later.
    pub origin: NavOrigin,
    pub title: String,
    /// Lucide icon name or `None`.  The Shell maps it to a glyph when
    /// the lucide bundle lands; for now it is rendered as the first
    /// letter of the title in a chip.
    pub icon: Option<String>,
    pub order: i32,
    /// Services that must be healthy for the panel to mount.  When any
    /// listed service is unhealthy the slot renders a stub instead.
    pub required_services: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOrigin {
    FirstParty,
    Extension,
}

/// Cached navigation state owned by the Shell View.
///
/// The model is small — building it cheaply on every render is fine —
/// but the *selection* state is stateful (the user's last click) and
/// the *health* state is mutated by async pipe replies, so it lives in
/// one place rather than scattered across the View.
#[derive(Debug, Clone, Default)]
pub struct NavModel {
    pub rows: Vec<NavRow>,
    pub selected_key: Option<String>,
    /// Per-service "is the daemon reachable?" cache.  Absent means
    /// "we haven't checked yet" — the slot treats that as healthy so
    /// the panel mounts immediately rather than briefly flashing the
    /// stub while the first probe is in flight.
    pub health: BTreeMap<String, bool>,
    /// Per-service human-readable reason for being unavailable, when the
    /// daemon supplied one (currently: a `min_core` incompatibility — the
    /// service needs a newer Wylde Core than is running). Absent means "down
    /// for the ordinary reason" (not running); present means "present but
    /// incompatible", which the stub renders differently (tells the user to
    /// update Wylde rather than offering a futile Start).
    pub reasons: BTreeMap<String, String>,
}

impl NavModel {
    /// Build a fresh model from a row list + the previous selection
    /// (if any).  Keeps the user's selection across registry rebuilds
    /// when the row still exists; otherwise resets to the first row.
    pub fn new(rows: Vec<NavRow>, previous_selection: Option<String>) -> Self {
        let valid_keys: BTreeSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        let selected_key = match previous_selection {
            Some(k) if valid_keys.contains(k.as_str()) => Some(k),
            _ => rows.first().map(|r| r.key.clone()),
        };
        Self {
            rows,
            selected_key,
            health: BTreeMap::new(),
            reasons: BTreeMap::new(),
        }
    }

    /// Click handler — mutates the selection.  Returns `true` when
    /// the selection changed so callers can decide whether to redraw.
    pub fn select(&mut self, key: &str) -> bool {
        if self.selected_key.as_deref() == Some(key) {
            return false;
        }
        if !self.rows.iter().any(|r| r.key == key) {
            return false;
        }
        self.selected_key = Some(key.to_owned());
        true
    }

    /// Whether `key` names a known nav row. Lets a caller distinguish
    /// "already selected" from "no such panel" (both make `select` return
    /// `false`) so a cross-panel nav request for a renamed/stale key can be
    /// logged rather than silently dropped.
    pub fn has_key(&self, key: &str) -> bool {
        self.rows.iter().any(|r| r.key == key)
    }

    /// Mark a service's health status.  The slot's stub fires when any
    /// required service is explicitly `false`; absent / `true` both
    /// mean "let the panel mount".
    pub fn mark_service_health(&mut self, name: &str, healthy: bool) {
        self.health.insert(name.to_owned(), healthy);
    }

    /// Record (or clear) the human-readable reason a service is unavailable.
    /// `Some(reason)` when the daemon reported a specific cause (a `min_core`
    /// incompatibility); `None` clears it (back to the ordinary "not running").
    pub fn mark_service_reason(&mut self, name: &str, reason: Option<String>) {
        match reason {
            Some(r) => {
                self.reasons.insert(name.to_owned(), r);
            }
            None => {
                self.reasons.remove(name);
            }
        }
    }

    /// Look up the currently selected row.
    pub fn selected_row(&self) -> Option<&NavRow> {
        let key = self.selected_key.as_deref()?;
        self.rows.iter().find(|r| r.key == key)
    }

    /// Decide what the slot should display for the current selection.
    pub fn slot_state(&self) -> SlotState {
        let Some(row) = self.selected_row() else {
            return SlotState::Empty;
        };
        // Any explicitly-false health flag for a required service blocks
        // the mount.  An absent flag is treated as "assume healthy" —
        // the Shell's startup probe fills it in shortly after launch.
        let blocking: Vec<String> = row
            .required_services
            .iter()
            .filter(|svc| self.health.get(svc.as_str()) == Some(&false))
            .cloned()
            .collect();
        if blocking.is_empty() {
            SlotState::Mount {
                key: row.key.clone(),
            }
        } else {
            // Parallel to `blocking`: the daemon-supplied reason for each
            // blocking service (e.g. a min_core incompatibility), or `None`
            // when it's down for the ordinary "not running" reason.
            let reasons: Vec<Option<String>> = blocking
                .iter()
                .map(|svc| self.reasons.get(svc).cloned())
                .collect();
            SlotState::ServiceUnavailable {
                key: row.key.clone(),
                missing: blocking,
                reasons,
            }
        }
    }
}

/// Canonical service name of the Ollama inference wrapper. Centralised so
/// the panel-readiness gate and tests agree on the spelling.
pub const SVC_OLLAMA: &str = "wylde-ollama";

/// Decide whether a `service.health` ok-body means the service is ready
/// for its panel to mount.
///
/// For `wylde-ollama` the lifecycle daemon composes wrapper-pipe liveness
/// with a probe of the external Ollama daemon at 127.0.0.1:11434 and
/// returns `ok` **with** an `reply.upstream` flag even when that daemon is
/// down — deliberately, so the Dashboard can paint a degraded (yellow)
/// tile rather than red. But a Chat/Models panel is unusable until the LLM
/// layer is actually reachable, so we gate ollama on `reply.upstream ==
/// "ok"`: a wrapper answering its pipe is NOT enough. Without this the
/// panels mounted on a down upstream and chat only failed at send-time,
/// with no affordance to start Ollama. Every other service is ready as
/// soon as the daemon answered `ok`.
///
/// A missing `reply.upstream` (an older lifecycle daemon that didn't
/// compose the upstream probe) is treated as ready, so this never
/// over-blocks against a daemon that predates the composed health shape.
pub fn service_health_body_is_ready(name: &str, body: &serde_json::Value) -> bool {
    // A service the daemon flagged incompatible (its manifest's min_core floor
    // exceeds the running Core) is deliberately NOT ready: its panel must render
    // the reason stub, not mount. The daemon returns `reply.incompatible = true`
    // for these (see wylde-lifecycle `service_health_action`).
    if body
        .get("reply")
        .and_then(|r| r.get("incompatible"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return false;
    }
    if name == SVC_OLLAMA {
        return match body.get("reply").and_then(|r| r.get("upstream")) {
            Some(upstream) => upstream.as_str() == Some("ok"),
            None => true,
        };
    }
    true
}

/// The human-readable reason a service is unavailable, when the daemon supplied
/// one on its `service.health` reply (currently: a `min_core` incompatibility).
/// `None` when the reply carried no specific reason.
pub fn service_health_reason(body: &serde_json::Value) -> Option<String> {
    body.get("reply")
        .and_then(|r| r.get("reason"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// What the panel slot is asked to render this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No panels registered (or none selected yet).
    Empty,
    /// The selected panel's factory should mount.
    Mount { key: String },
    /// One or more required services failed their health check — render a stub.
    /// `reasons[i]` is the daemon-supplied cause for `missing[i]` (e.g. a
    /// min_core incompatibility) or `None` for an ordinary "not running", which
    /// the stub renders differently (update-Wylde vs a Start button).
    ServiceUnavailable {
        key: String,
        missing: Vec<String>,
        reasons: Vec<Option<String>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_party(key: &str, order: i32, requires: &[&str]) -> NavRow {
        NavRow {
            key: key.into(),
            origin: NavOrigin::FirstParty,
            title: key.into(),
            icon: None,
            order,
            required_services: requires.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    use serde_json::json;

    #[test]
    fn ollama_ready_only_when_upstream_ok() {
        // service.health for ollama folds the upstream daemon status into
        // `reply.upstream`. The panel must gate on it being "ok".
        let ok = json!({ "name": "wylde-ollama", "reply": { "ok": true, "upstream": "ok" } });
        assert!(service_health_body_is_ready("wylde-ollama", &ok));

        for down in ["unreachable", "timeout"] {
            let body =
                json!({ "name": "wylde-ollama", "reply": { "ok": true, "upstream": down } });
            assert!(
                !service_health_body_is_ready("wylde-ollama", &body),
                "upstream={down} must gate the panel"
            );
        }
    }

    #[test]
    fn ollama_missing_upstream_field_does_not_over_block() {
        // An older lifecycle daemon that didn't compose the upstream probe
        // returns an ok-body without `reply.upstream`; don't stub on it.
        let body = json!({ "name": "wylde-ollama", "reply": { "ok": true } });
        assert!(service_health_body_is_ready("wylde-ollama", &body));
        let bare = json!({ "ok": true });
        assert!(service_health_body_is_ready("wylde-ollama", &bare));
    }

    #[test]
    fn non_ollama_services_ready_on_any_ok_body() {
        // Every other service is ready as soon as service.health answered ok
        // — the gate is ollama-specific.
        let body = json!({ "ok": true });
        assert!(service_health_body_is_ready("wylde-harness", &body));
        assert!(service_health_body_is_ready("wylde-gateway", &json!({})));
    }

    #[test]
    fn selection_defaults_to_first_row_when_empty_previous() {
        let m = NavModel::new(
            vec![
                first_party("core/chat", 10, &[]),
                first_party("core/settings", 95, &[]),
            ],
            None,
        );
        assert_eq!(m.selected_key.as_deref(), Some("core/chat"));
    }

    #[test]
    fn selection_preserved_across_rebuild_when_key_still_present() {
        let m = NavModel::new(
            vec![
                first_party("core/chat", 10, &[]),
                first_party("core/settings", 95, &[]),
            ],
            Some("core/settings".into()),
        );
        assert_eq!(m.selected_key.as_deref(), Some("core/settings"));
    }

    #[test]
    fn selection_falls_back_when_previous_key_disappeared() {
        let m = NavModel::new(
            vec![first_party("core/chat", 10, &[])],
            Some("core/settings".into()),
        );
        assert_eq!(m.selected_key.as_deref(), Some("core/chat"));
    }

    #[test]
    fn select_changes_state_and_returns_true_on_first_pick() {
        let mut m = NavModel::new(
            vec![
                first_party("core/chat", 10, &[]),
                first_party("core/settings", 95, &[]),
            ],
            None,
        );
        assert!(m.select("core/settings"));
        assert_eq!(m.selected_key.as_deref(), Some("core/settings"));
    }

    #[test]
    fn select_is_noop_when_already_selected() {
        let mut m = NavModel::new(vec![first_party("core/chat", 10, &[])], None);
        assert!(!m.select("core/chat"));
    }

    #[test]
    fn select_is_noop_for_unknown_key() {
        let mut m = NavModel::new(vec![first_party("core/chat", 10, &[])], None);
        assert!(!m.select("nope/nope"));
        assert_eq!(m.selected_key.as_deref(), Some("core/chat"));
    }

    #[test]
    fn slot_mounts_when_no_required_services() {
        let m = NavModel::new(vec![first_party("core/chat", 10, &[])], None);
        assert_eq!(
            m.slot_state(),
            SlotState::Mount {
                key: "core/chat".into()
            }
        );
    }

    #[test]
    fn slot_mounts_when_required_services_have_no_health_data_yet() {
        // The startup probe hasn't returned yet — we still mount to
        // avoid a stub-flash on first render.
        let m = NavModel::new(
            vec![first_party("core/settings", 95, &["wylde-harness"])],
            None,
        );
        assert_eq!(
            m.slot_state(),
            SlotState::Mount {
                key: "core/settings".into()
            }
        );
    }

    #[test]
    fn slot_mounts_when_required_services_are_healthy() {
        let mut m = NavModel::new(
            vec![first_party("core/settings", 95, &["wylde-harness"])],
            None,
        );
        m.mark_service_health("wylde-harness", true);
        assert!(matches!(m.slot_state(), SlotState::Mount { .. }));
    }

    #[test]
    fn slot_emits_service_unavailable_when_any_required_is_unhealthy() {
        let mut m = NavModel::new(
            vec![first_party(
                "core/settings",
                95,
                &["wylde-harness", "wylde-lifecycle"],
            )],
            None,
        );
        m.mark_service_health("wylde-harness", false);
        m.mark_service_health("wylde-lifecycle", true);
        let state = m.slot_state();
        match state {
            SlotState::ServiceUnavailable { missing, .. } => {
                assert_eq!(missing, vec!["wylde-harness".to_string()]);
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn slot_lists_every_unhealthy_required_service() {
        let mut m = NavModel::new(
            vec![first_party(
                "core/chat",
                10,
                &["wylde-harness", "wylde-lifecycle"],
            )],
            None,
        );
        m.mark_service_health("wylde-harness", false);
        m.mark_service_health("wylde-lifecycle", false);
        match m.slot_state() {
            SlotState::ServiceUnavailable { missing, .. } => {
                // Order matches the manifest declaration order.
                assert_eq!(missing, vec!["wylde-harness", "wylde-lifecycle"]);
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn empty_registry_produces_empty_slot() {
        let m = NavModel::new(vec![], None);
        assert_eq!(m.slot_state(), SlotState::Empty);
    }

    #[test]
    fn slot_carries_incompatibility_reason_parallel_to_missing() {
        let mut m = NavModel::new(
            vec![first_party("ext/organize", 50, &["wylde-organize"])],
            None,
        );
        m.mark_service_health("wylde-organize", false);
        m.mark_service_reason(
            "wylde-organize",
            Some("needs Wylde Core >= 0.3.0, but this Core is 0.2.1 — update Wylde".into()),
        );
        match m.slot_state() {
            SlotState::ServiceUnavailable {
                missing, reasons, ..
            } => {
                assert_eq!(missing, vec!["wylde-organize".to_string()]);
                assert_eq!(reasons.len(), missing.len(), "reasons parallel to missing");
                assert!(reasons[0].as_deref().unwrap().contains("0.3.0"));
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_down_service_has_no_reason() {
        // A service that's merely not running (no daemon reason) yields a None
        // reason slot, so the stub keeps its "Start" affordance.
        let mut m = NavModel::new(
            vec![first_party("core/chat", 10, &["wylde-harness"])],
            None,
        );
        m.mark_service_health("wylde-harness", false);
        match m.slot_state() {
            SlotState::ServiceUnavailable { reasons, .. } => {
                assert_eq!(reasons, vec![None]);
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn mark_service_reason_clears_on_none() {
        let mut m = NavModel::new(vec![first_party("core/chat", 10, &[])], None);
        m.mark_service_reason("wylde-x", Some("boom".into()));
        assert_eq!(m.reasons.get("wylde-x").map(String::as_str), Some("boom"));
        m.mark_service_reason("wylde-x", None);
        assert!(m.reasons.get("wylde-x").is_none());
    }

    #[test]
    fn incompatible_health_body_is_not_ready_and_yields_reason() {
        let body = json!({
            "name": "wylde-organize",
            "reply": {
                "ok": false,
                "incompatible": true,
                "reason": "needs Wylde Core >= 0.3.0 — update Wylde"
            }
        });
        assert!(
            !service_health_body_is_ready("wylde-organize", &body),
            "an incompatible service must NOT be ready (stub, don't mount)"
        );
        assert_eq!(
            service_health_reason(&body).as_deref(),
            Some("needs Wylde Core >= 0.3.0 — update Wylde")
        );
        // An ordinary ok body: ready, no reason.
        let ok = json!({ "name": "wylde-harness", "reply": { "ok": true } });
        assert!(service_health_body_is_ready("wylde-harness", &ok));
        assert_eq!(service_health_reason(&ok), None);
    }
}
