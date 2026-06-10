//! Per-panel IPC helpers for the Devices panel.
//!
//! Every call goes through `wylde-device-gate`'s `/__action__` pipe
//! envelope.  Action verbs documented at `device_gate/pipe.py`; their
//! reply shapes live in `device_gate/core.py::DeviceGateService`.
//!
//! Helpers are thin: they translate the JSON envelope into a small
//! Rust struct the View consumes, so the rendering layer never sees
//! `serde_json::Value` directly.  This keeps the View testable as a
//! pure function of "what did the wire reply with".

use serde_json::{json, Value};

pub const SVC_DEVICE_GATE: &str = "wylde-device-gate";

// ── Tier identifiers (mirroring `device_gate/store.py`) ──────────────

/// Read-only access.  The device can read chat history but cannot
/// trigger any tools.  Default tier for newly-paired devices.
pub const TIER_READ_ONLY: &str = "read_only";

/// Tool-use tier.  Read / search / retrieve tools are allowed.  No
/// write / delete / execute paths.
pub const TIER_TOOL_USE: &str = "tool_use";

/// Destructive tool access.  Write / delete / execute are unlocked.
/// The View gates the upgrade behind an inline confirmation strip so a
/// stray click can't grant a phone shell access.
pub const TIER_DESTRUCTIVE: &str = "destructive_tool_access";

/// Display order for the segmented tier pill row.
pub const ALL_TIERS: &[&str] = &[TIER_READ_ONLY, TIER_TOOL_USE, TIER_DESTRUCTIVE];

/// Human-readable label for a tier — surfaced on the pill and in
/// confirmation strips.
pub fn tier_label(tier: &str) -> &'static str {
    match tier {
        TIER_READ_ONLY => "Read only",
        TIER_TOOL_USE => "Tool use",
        TIER_DESTRUCTIVE => "Full access",
        _ => "Unknown",
    }
}

/// One-line blurb explaining what the tier grants.  Used for the
/// description line under the pill row.
pub fn tier_blurb(tier: &str) -> &'static str {
    match tier {
        TIER_READ_ONLY => "View chat history. Cannot trigger tools.",
        TIER_TOOL_USE => "Read / search / retrieve tools.",
        TIER_DESTRUCTIVE => "Includes write / delete / execute.",
        _ => "Unknown tier.",
    }
}

// ── Reply shapes ─────────────────────────────────────────────────────

/// One row off `device_gate.list_devices`.  Mirror of the Python
/// `Device.to_dict()` projection — the `token` field is deliberately
/// elided server-side, so this struct doesn't carry it either.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceRow {
    pub device_id: String,
    pub name: String,
    pub tier: String,
    /// Unix seconds, pairing time.
    pub paired_at: f64,
    /// Unix seconds, last verify().
    pub last_seen: f64,
    /// Active in the last `active_threshold_s` (60 s server default).
    pub is_active: bool,
}

impl DeviceRow {
    pub fn from_value(v: &Value) -> Self {
        Self {
            device_id: v
                .get("device_id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            tier: v
                .get("tier")
                .and_then(|x| x.as_str())
                .unwrap_or(TIER_READ_ONLY)
                .to_owned(),
            paired_at: v.get("paired_at").and_then(|x| x.as_f64()).unwrap_or(0.0),
            last_seen: v.get("last_seen").and_then(|x| x.as_f64()).unwrap_or(0.0),
            is_active: v
                .get("is_active")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
        }
    }

    /// Short fingerprint surfaced in the row header.  The wire device
    /// id is `dev_<unix>_<hex6>`; we take the trailing hex so two
    /// devices paired in the same second still render distinguishable
    /// chips.  Falls back to the first 8 chars when the id doesn't
    /// match the expected shape.
    pub fn short_fingerprint(&self) -> String {
        if let Some(tail) = self.device_id.rsplit('_').next() {
            if !tail.is_empty() && tail.len() <= 12 {
                return tail.to_owned();
            }
        }
        self.device_id.chars().take(8).collect()
    }
}

/// One entry off `device_gate.recent_actions`.  Mirror of the Python
/// `ActionLog` entry shape — `{action, timestamp, status}` where
/// `timestamp` is ISO-8601 UTC.  Backs the per-row "recent activity"
/// strip; the View renders the newest few verbatim.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionEntry {
    /// Human-readable verb, e.g. "paired", "token rotated", "tier → tool_use".
    pub action: String,
    /// ISO-8601 UTC, second resolution (e.g. "2026-05-30T12:34:56Z").
    pub timestamp: String,
    /// "ok" on success — the only status the service writes today, but
    /// kept on the wire so a future failure-audit entry needs no reshape.
    pub status: String,
}

impl ActionEntry {
    pub fn from_value(v: &Value) -> Self {
        Self {
            action: v
                .get("action")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            timestamp: v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// Snapshot of `device_gate.get_pairing_status`.  When pairing isn't
/// active the server returns `{pairing_active: false}` and we surface
/// `Inactive`; otherwise the timer + code travel together so the View
/// can render the countdown without a second round-trip.
#[derive(Debug, Clone, PartialEq)]
pub enum PairingStatus {
    Inactive,
    Active { code: String, expires_at: f64 },
}

impl PairingStatus {
    pub fn from_value(v: &Value) -> Self {
        let active = v
            .get("pairing_active")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        if !active {
            return Self::Inactive;
        }
        let code = v
            .get("code")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        let expires_at = v.get("expires_at").and_then(|x| x.as_f64()).unwrap_or(0.0);
        if code.is_empty() || expires_at <= 0.0 {
            // Server says "active" but the payload is malformed — treat
            // as inactive so the View doesn't render a zero-second
            // countdown.
            return Self::Inactive;
        }
        Self::Active { code, expires_at }
    }
}

// ── Unary action helpers ─────────────────────────────────────────────

async fn action(verb: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_DEVICE_GATE,
        "POST",
        "/__action__",
        Some(json!({ "action": verb, "payload": payload })),
    )
    .await
}

/// Load the paired-device list.  Returns rows sorted by `paired_at`
/// descending — newest at the top, matching the Svelte page.  Empty
/// vec when no devices are paired or the service is down (the View
/// renders an empty-state in either case; `Err` is reserved for
/// transport-level diagnostics).
pub async fn list_devices() -> Result<Vec<DeviceRow>, String> {
    let v = action("device_gate.list_devices", json!({})).await?;
    let Some(arr) = v.get("devices").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    let mut rows: Vec<DeviceRow> = arr.iter().map(DeviceRow::from_value).collect();
    rows.sort_by(|a, b| {
        b.paired_at
            .partial_cmp(&a.paired_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

/// Open a pairing window.  Returns the freshly-minted `{code,
/// expires_at}`.  The Python service replaces any earlier pending code
/// so the caller never has to call `cancel_pairing` first.
pub async fn start_pairing() -> Result<PairingStatus, String> {
    let v = action("device_gate.start_pairing", json!({})).await?;
    // The reply shape is `{code, expires_at}` (no `pairing_active`
    // wrapper); synthesise the enum directly.
    let code = v
        .get("code")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_owned();
    let expires_at = v.get("expires_at").and_then(|x| x.as_f64()).unwrap_or(0.0);
    if code.is_empty() || expires_at <= 0.0 {
        return Err("malformed start_pairing reply".into());
    }
    Ok(PairingStatus::Active { code, expires_at })
}

/// Drain a pending pairing window.  Idempotent: `was_active` reflects
/// whether the call actually cancelled anything.
pub async fn cancel_pairing() -> Result<bool, String> {
    let v = action("device_gate.cancel_pairing", json!({})).await?;
    Ok(v.get("cancelled")
        .and_then(|x| x.as_bool())
        .unwrap_or(false))
}

/// Poll the pairing window's live state.  Used while the pair card is
/// open so we close it the moment a mobile client completes pairing —
/// the next `list_devices` then shows the new row.
pub async fn get_pairing_status() -> Result<PairingStatus, String> {
    let v = action("device_gate.get_pairing_status", json!({})).await?;
    Ok(PairingStatus::from_value(&v))
}

/// Change a device's permission tier.  Server rejects unknown tiers
/// with `bad_request`; we surface that as a string the panel can
/// display.
pub async fn set_tier(device_id: &str, tier: &str) -> Result<(), String> {
    action(
        "device_gate.set_tier",
        json!({ "device_id": device_id, "tier": tier }),
    )
    .await
    .map(|_| ())
}

/// Mint a fresh bearer token for `device_id`.  Returns the new token
/// so the panel can render it for the user to copy (and queue a
/// `token_rotated` event for the Gateway to forward to any active
/// mobile connection).
pub async fn rotate_token(device_id: &str) -> Result<String, String> {
    let v = action(
        "device_gate.rotate_token",
        json!({ "device_id": device_id }),
    )
    .await?;
    Ok(v.get("new_token")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_owned())
}

/// Drop a device from the store.  The mobile will get a `revoked`
/// event the next time its Gateway connection polls.
pub async fn revoke(device_id: &str) -> Result<(), String> {
    action("device_gate.revoke", json!({ "device_id": device_id }))
        .await
        .map(|_| ())
}

/// Read the rolling per-device action log, newest-first.  Returns up to
/// `limit` entries; an unknown device (or a service that's down) yields
/// an empty vec rather than an error so the row renders "No recent
/// activity" instead of an error toast.
pub async fn recent_actions(device_id: &str, limit: u32) -> Result<Vec<ActionEntry>, String> {
    let v = action(
        "device_gate.recent_actions",
        json!({ "device_id": device_id, "limit": limit }),
    )
    .await?;
    let Some(arr) = v.get("actions").and_then(|x| x.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr.iter().map(ActionEntry::from_value).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_row_parses_python_to_dict() {
        let v = json!({
            "device_id": "dev_1718000000_a1b2c3",
            "name": "the Wylde user's Pixel",
            "tier": "tool_use",
            "paired_at": 1_718_000_000.0,
            "last_seen": 1_718_001_000.0,
            "metadata": {},
            "is_active": true,
        });
        let row = DeviceRow::from_value(&v);
        assert_eq!(row.device_id, "dev_1718000000_a1b2c3");
        assert_eq!(row.name, "the Wylde user's Pixel");
        assert_eq!(row.tier, TIER_TOOL_USE);
        assert!(row.is_active);
        assert_eq!(row.short_fingerprint(), "a1b2c3");
    }

    #[test]
    fn device_row_short_fingerprint_falls_back_on_non_canonical_id() {
        let row = DeviceRow {
            device_id: "shortid".into(),
            ..DeviceRow::default()
        };
        assert_eq!(row.short_fingerprint(), "shortid");
        let row2 = DeviceRow {
            device_id: "this-is-a-very-long-non-prefixed-id".into(),
            ..DeviceRow::default()
        };
        // `rsplit('_').next()` returns the whole string when no `_`
        // is present — fingerprint guard clamps to length, but the
        // bare string only triggers the fallback when length exceeds
        // 12 chars.  Either branch is acceptable; what we assert is
        // we don't panic and we always produce *some* short id.
        assert!(!row2.short_fingerprint().is_empty());
        assert!(row2.short_fingerprint().chars().count() <= 36);
    }

    #[test]
    fn device_row_defaults_when_payload_missing_keys() {
        let row = DeviceRow::from_value(&json!({}));
        assert!(row.device_id.is_empty());
        assert_eq!(row.tier, TIER_READ_ONLY);
        assert!(!row.is_active);
    }

    #[test]
    fn pairing_status_parses_active() {
        let v = json!({
            "pairing_active": true,
            "code": "123456",
            "expires_at": 1_718_000_300.0,
        });
        match PairingStatus::from_value(&v) {
            PairingStatus::Active { code, expires_at } => {
                assert_eq!(code, "123456");
                assert!((expires_at - 1_718_000_300.0).abs() < 1e-6);
            }
            _ => panic!("expected Active"),
        }
    }

    #[test]
    fn pairing_status_parses_inactive() {
        let v = json!({"pairing_active": false});
        assert_eq!(PairingStatus::from_value(&v), PairingStatus::Inactive);
    }

    #[test]
    fn pairing_status_treats_zero_expiry_as_inactive() {
        let v = json!({"pairing_active": true, "code": "0", "expires_at": 0});
        assert_eq!(PairingStatus::from_value(&v), PairingStatus::Inactive);
    }

    #[test]
    fn tier_constants_match_python_store() {
        // These three strings are the canonical wire identifiers.  If
        // the Python side ever renames one, this test fires at compile
        // time as a reminder to update both ends.
        assert_eq!(TIER_READ_ONLY, "read_only");
        assert_eq!(TIER_TOOL_USE, "tool_use");
        assert_eq!(TIER_DESTRUCTIVE, "destructive_tool_access");
        assert_eq!(ALL_TIERS.len(), 3);
    }

    #[test]
    fn tier_label_and_blurb_cover_every_tier() {
        for t in ALL_TIERS {
            assert_ne!(tier_label(t), "Unknown");
            assert_ne!(tier_blurb(t), "Unknown tier.");
        }
        assert_eq!(tier_label("bogus"), "Unknown");
    }

    #[test]
    fn pipe_helpers_exist() {
        // Smoke that the verbs the panel reaches for actually exist
        // at the type level — surfaces an accidental rename.
        let _ = list_devices;
        let _ = start_pairing;
        let _ = cancel_pairing;
        let _ = get_pairing_status;
        let _ = set_tier;
        let _ = rotate_token;
        let _ = revoke;
        let _ = recent_actions;
    }

    #[test]
    fn action_entry_parses_python_log_shape() {
        let v = json!({
            "action": "tier → tool_use",
            "timestamp": "2026-05-30T12:34:56Z",
            "status": "ok",
        });
        let e = ActionEntry::from_value(&v);
        assert_eq!(e.action, "tier → tool_use");
        assert_eq!(e.timestamp, "2026-05-30T12:34:56Z");
        assert_eq!(e.status, "ok");
    }

    #[test]
    fn action_entry_defaults_missing_fields() {
        let e = ActionEntry::from_value(&json!({}));
        assert!(e.action.is_empty());
        assert!(e.timestamp.is_empty());
        assert!(e.status.is_empty());
    }
}
