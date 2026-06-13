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
            SlotState::ServiceUnavailable {
                key: row.key.clone(),
                missing: blocking,
            }
        }
    }
}

/// What the panel slot is asked to render this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No panels registered (or none selected yet).
    Empty,
    /// The selected panel's factory should mount.
    Mount { key: String },
    /// One or more required services failed their health check — render
    /// a stub with the names + a "Start service" button.
    ServiceUnavailable { key: String, missing: Vec<String> },
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
}
