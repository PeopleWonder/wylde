//! Read-only view over `data/manifests/<svc>.json` runtime manifests.
//!
//! The same files [`crate::manifest::ManifestWriter`] writes from services
//! are read here without locking, ipc, or spawning — just file walk +
//! JSON parse. Two consumers depend on this primitive:
//!
//! 1. The Lifecycle daemon's registry (`wylde-lifecycle::registry`),
//!    which combines this with liveness probes to produce the dashboard
//!    inventory.
//! 2. The pre-build guard (each wylde-* crate's `build.rs`), which uses
//!    the manifest's `status.pid` + `status.heartbeat` to surface a
//!    diagnostic-rich error when a daemon would lock the linker out.
//!
//! Keeping this code path in one place means "what the guard sees" and
//! "what production sees" can never diverge.

use std::path::Path;

/// Per-service snapshot of the fields callers actually consume.
///
/// `pid` and `heartbeat` are optional because a malformed or partially-
/// written manifest is treated as best-effort data — callers downgrade
/// rather than failing on missing fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestStatus {
    /// Service identifier as written into the manifest (`service` field,
    /// falling back to `name` to mirror Python's reader). Note: this is
    /// the *manifest's* declared name, which may have a `wylde-` prefix
    /// stripped (e.g. `vram-broker.json` writes `"service": "vram-broker"`).
    pub name: String,
    pub pid: Option<i64>,
    pub heartbeat: Option<String>,
    pub state: Option<String>,
}

/// Walk `<dir>/*.json` and return one [`ManifestStatus`] per parseable
/// entry. Malformed files are silently skipped — `None` is reserved for
/// "directory doesn't exist or is unreadable", so the caller can tell
/// "no services" apart from "filesystem went sideways".
///
/// Ordering: sorted by `name` so callers get stable output for snapshots
/// and diagnostic messages.
pub fn list_runtime_statuses(dir: &Path) -> Option<Vec<ManifestStatus>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut out: Vec<ManifestStatus> = Vec::with_capacity(paths.len());
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let name = doc
            .get("service")
            .and_then(serde_json::Value::as_str)
            .or_else(|| doc.get("name").and_then(serde_json::Value::as_str))
            .unwrap_or("")
            .to_owned();
        if name.is_empty() {
            continue;
        }
        let status = doc.get("status");
        let pid = status
            .and_then(|s| s.get("pid"))
            .and_then(serde_json::Value::as_i64);
        let heartbeat = status
            .and_then(|s| s.get("heartbeat"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let state = status
            .and_then(|s| s.get("state"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        out.push(ManifestStatus {
            name,
            pid,
            heartbeat,
            state,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// Seconds elapsed since `heartbeat`. Returns [`f64::INFINITY`] for a
/// missing, empty, or unparseable value — same contract Python's
/// `_heartbeat_age` (`Core/Lifecycle/control.py`) uses.
///
/// Accepts the `Z` suffix and explicit offsets via `parse_from_rfc3339`,
/// matching what [`crate::manifest::ManifestWriter`] writes.
pub fn heartbeat_age_secs(heartbeat: Option<&str>) -> f64 {
    let Some(s) = heartbeat else {
        return f64::INFINITY;
    };
    if s.is_empty() {
        return f64::INFINITY;
    }
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) else {
        return f64::INFINITY;
    };
    let delta = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
    delta.num_milliseconds() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_directory_returns_none() {
        let tmp = tempdir().unwrap();
        let gone = tmp.path().join("nope");
        assert!(list_runtime_statuses(&gone).is_none());
    }

    #[test]
    fn empty_directory_returns_empty_vec() {
        let tmp = tempdir().unwrap();
        let v = list_runtime_statuses(tmp.path()).expect("Some(vec)");
        assert!(v.is_empty());
    }

    #[test]
    fn parses_status_fields_and_sorts_by_name() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wylde-gateway.json"),
            r#"{"service": "wylde-gateway",
                "status": {"pid": 32240, "heartbeat": "2026-05-22T23:00:15Z", "state": "alive"}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("vram-broker.json"),
            r#"{"service": "vram-broker",
                "status": {"pid": 13488, "heartbeat": "2026-05-22T23:00:14Z", "state": "alive"}}"#,
        )
        .unwrap();

        let v = list_runtime_statuses(tmp.path()).expect("Some(vec)");
        assert_eq!(v.len(), 2);
        // Sorted lexicographically: "vram-broker" < "wylde-gateway".
        assert_eq!(v[0].name, "vram-broker");
        assert_eq!(v[0].pid, Some(13488));
        assert_eq!(v[0].state.as_deref(), Some("alive"));
        assert_eq!(v[1].name, "wylde-gateway");
        assert_eq!(v[1].heartbeat.as_deref(), Some("2026-05-22T23:00:15Z"));
    }

    #[test]
    fn falls_back_to_name_field_when_service_missing() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.json"),
            r#"{"name": "foo", "status": {"pid": 1}}"#,
        )
        .unwrap();
        let v = list_runtime_statuses(tmp.path()).expect("Some(vec)");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "foo");
        assert_eq!(v[0].pid, Some(1));
    }

    #[test]
    fn skips_malformed_json_and_unnamed_entries() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "not json").unwrap();
        std::fs::write(tmp.path().join("nameless.json"), r#"{"status": {"pid": 9}}"#).unwrap();
        std::fs::write(
            tmp.path().join("good.json"),
            r#"{"service": "good", "status": {"pid": 1}}"#,
        )
        .unwrap();
        let v = list_runtime_statuses(tmp.path()).expect("Some(vec)");
        // Only "good" survives — the other two are dropped silently.
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "good");
    }

    #[test]
    fn heartbeat_age_missing_or_malformed_is_infinite() {
        assert!(heartbeat_age_secs(None).is_infinite());
        assert!(heartbeat_age_secs(Some("")).is_infinite());
        assert!(heartbeat_age_secs(Some("not a date")).is_infinite());
    }

    #[test]
    fn heartbeat_age_recent_is_small_and_finite() {
        let ts = chrono::Utc::now() - chrono::Duration::seconds(30);
        let s = ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let age = heartbeat_age_secs(Some(&s));
        assert!(age.is_finite() && (20.0..=60.0).contains(&age), "age={age}");
    }
}
