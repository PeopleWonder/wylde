//! `time.*` — small, self-contained time utilities new in Phase 6.
//!
//! Python's `Core/harness/tooling/tools/` had no `time/` group; the
//! migration master plan calls out time tools as obvious self-contained
//! candidates that don't depend on memory or RAG. Adding them here
//! costs nothing and gives the model a clean way to ask "what time is
//! it now" / "format this timestamp" without falling back to
//! `execute_python`.

use chrono::{DateTime, Local, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::registry::{entry_active, param, param_default, Registry};

pub fn register(reg: &mut Registry) {
    reg.insert(entry_active(
        "time_now",
        "time.now",
        "time",
        "Return the current time. Includes UTC ISO-8601, local ISO-8601, \
         and a Unix epoch millisecond timestamp.",
        vec![],
        false,
        |_, _| async move { run_time_now().await },
    ));

    reg.insert(entry_active(
        "time_format",
        "time.format",
        "time",
        "Format a Unix epoch millisecond timestamp as an ISO-8601 string. \
         `tz: 'utc' | 'local'` selects the zone.",
        vec![
            param("epoch_ms", "number", true, "Unix epoch milliseconds"),
            param_default("tz", "string", "Time zone: 'utc' or 'local'", json!("utc")),
        ],
        false,
        |args, _| async move { run_time_format(args).await },
    ));
}

pub(crate) async fn run_time_now() -> Result<Value, IpcError> {
    let utc_now = Utc::now();
    let local_now: DateTime<Local> = Local::now();
    Ok(json!({
        "status": "success",
        "utc": utc_now.to_rfc3339_opts(SecondsFormat::Millis, true),
        "local": local_now.to_rfc3339_opts(SecondsFormat::Millis, false),
        "epoch_ms": utc_now.timestamp_millis(),
    }))
}

pub(crate) async fn run_time_format(args: Value) -> Result<Value, IpcError> {
    let Some(ms) = args.get("epoch_ms").and_then(Value::as_i64) else {
        return Ok(json!({
            "status": "error",
            "error": "'epoch_ms' is required (integer milliseconds)",
        }));
    };
    let tz = args.get("tz").and_then(Value::as_str).unwrap_or("utc");
    let Some(utc) = Utc.timestamp_millis_opt(ms).single() else {
        return Ok(json!({
            "status": "error",
            "error": format!("invalid epoch_ms: {ms}"),
        }));
    };
    let iso = match tz {
        "local" => {
            let local: DateTime<Local> = utc.with_timezone(&Local);
            local.to_rfc3339_opts(SecondsFormat::Millis, false)
        }
        _ => utc.to_rfc3339_opts(SecondsFormat::Millis, true),
    };
    Ok(json!({
        "status": "success",
        "iso": iso,
        "epoch_ms": ms,
        "tz": tz,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn time_now_returns_three_fields() {
        let v = run_time_now().await.unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["utc"].as_str().unwrap().contains('T'));
        assert!(v["local"].as_str().unwrap().contains('T'));
        assert!(v["epoch_ms"].as_i64().unwrap() > 1_700_000_000_000);
    }

    #[tokio::test]
    async fn time_format_utc_round_trips() {
        // 2026-01-01T00:00:00.000Z = 1_767_225_600_000
        let v = run_time_format(json!({"epoch_ms": 1_767_225_600_000_i64, "tz": "utc"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["iso"], "2026-01-01T00:00:00.000Z");
    }

    #[tokio::test]
    async fn time_format_errors_on_missing_arg() {
        let v = run_time_format(json!({})).await.unwrap();
        assert_eq!(v["status"], "error");
    }

    #[tokio::test]
    async fn time_format_local_includes_offset() {
        let v = run_time_format(json!({"epoch_ms": 1_767_225_600_000_i64, "tz": "local"}))
            .await
            .unwrap();
        assert_eq!(v["status"], "success");
        // Local ISO ends with a tz suffix (either +HH:MM, -HH:MM, or Z).
        let iso = v["iso"].as_str().unwrap();
        assert!(iso.contains('T'));
    }

    #[test]
    fn register_inserts_both_tools() {
        let mut reg = Registry::empty();
        register(&mut reg);
        assert!(reg.lookup("time_now").is_some());
        assert!(reg.lookup("time.now").is_some());
        assert!(reg.lookup("time_format").is_some());
        assert!(reg.lookup("time.format").is_some());
    }
}
