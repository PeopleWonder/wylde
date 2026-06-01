//! MRU cap settings. Rust port of
//! `Core/harness/memory/workspaces/_mru.py`.
//!
//! The cap is persisted to `<data_dir>/workspace_settings.json` so the
//! user can tune it from the Settings UI without restarting the
//! harness. Activation reads via [`get_mru_limit`] and evicts based on
//! the effective value.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use crate::memory::common::{data_dir, ensure_dir};

pub const MRU_LIMIT_DEFAULT: u64 = 5;
pub const MRU_LIMIT_MIN: u64 = 1;
pub const MRU_LIMIT_MAX: u64 = 20;

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

fn settings_path() -> PathBuf {
    data_dir().join("workspace_settings.json")
}

/// Validate an externally-supplied MRU cap. Returns a structured
/// `Err` so the IPC layer can surface `bad_request` cleanly.
pub fn clamp_mru(value: &Value) -> Result<u64, MruError> {
    // Reject `true`/`false` even though serde_json treats them as ints
    // in some surfaces; matches Python's `isinstance(bool)` reject.
    if value.is_boolean() {
        return Err(MruError::NotInteger {
            display: value.to_string(),
        });
    }
    let n: i64 = match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(f) = n.as_f64() {
                if f.fract() != 0.0 {
                    return Err(MruError::NotInteger {
                        display: f.to_string(),
                    });
                }
                f as i64
            } else {
                return Err(MruError::NotInteger {
                    display: n.to_string(),
                });
            }
        }
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| MruError::NotInteger {
            display: s.clone(),
        })?,
        other => {
            return Err(MruError::NotInteger {
                display: other.to_string(),
            })
        }
    };
    if n < MRU_LIMIT_MIN as i64 || n > MRU_LIMIT_MAX as i64 {
        return Err(MruError::OutOfRange { n });
    }
    Ok(n as u64)
}

/// Errors from [`clamp_mru`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MruError {
    #[error("mru limit must be an integer, got {display}")]
    NotInteger { display: String },
    #[error("mru limit must be in [{min}, {max}], got {n}", min = MRU_LIMIT_MIN, max = MRU_LIMIT_MAX)]
    OutOfRange { n: i64 },
}

fn read_settings() -> Value {
    let path = settings_path();
    if !path.exists() {
        return Value::Null;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Value::Null,
    };
    serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null)
}

fn write_settings(settings: &Value) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings).unwrap())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Effective MRU cap. Returns the persisted value if valid, else the
/// default. Garbage on disk silently falls back to default — matches
/// Python.
pub fn get_mru_limit() -> u64 {
    let settings = read_settings();
    let raw = settings.get("mru_limit").cloned().unwrap_or(Value::Null);
    match clamp_mru(&raw) {
        Ok(n) => n,
        Err(_) => MRU_LIMIT_DEFAULT,
    }
}

/// Persist a new MRU cap. If the new cap is smaller than the current
/// MRU count, immediately evicts the tail past `n` (same semantics as
/// `evict_past_mru` — index dirs removed, workspace memory preserved).
///
/// Returns the clamped value or a structured error.
pub fn set_mru_limit(value: &Value) -> Result<u64, MruError> {
    let n = clamp_mru(value)?;
    let _g = SETTINGS_LOCK.lock().unwrap();

    let mut settings = read_settings();
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    settings["mru_limit"] = Value::from(n);
    let _ = write_settings(&settings);

    // Apply immediately. The store module gates its registry on its
    // own mutex; we don't have to share locks because the only field
    // overlap is the registry file, which we re-read post-write.
    let mut workspaces = super::store::load_registry();
    let evicted = super::store::evict_past_mru(&mut workspaces, n as usize);
    if !evicted.is_empty() {
        let _ = super::store::save_registry(&workspaces);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::workspaces::test_support::TestEnv;
    use serde_json::json;

    #[test]
    fn get_mru_limit_falls_back_to_default_when_unset() {
        let _env = TestEnv::new();
        assert_eq!(get_mru_limit(), MRU_LIMIT_DEFAULT);
    }

    #[test]
    fn set_then_get_round_trips_within_range() {
        let _env = TestEnv::new();
        let v = set_mru_limit(&json!(7)).unwrap();
        assert_eq!(v, 7);
        assert_eq!(get_mru_limit(), 7);
    }

    #[test]
    fn clamp_mru_rejects_bool() {
        assert!(matches!(
            clamp_mru(&json!(true)).unwrap_err(),
            MruError::NotInteger { .. }
        ));
        assert!(matches!(
            clamp_mru(&json!(false)).unwrap_err(),
            MruError::NotInteger { .. }
        ));
    }

    #[test]
    fn clamp_mru_rejects_non_integer() {
        assert!(matches!(
            clamp_mru(&json!("abc")).unwrap_err(),
            MruError::NotInteger { .. }
        ));
        assert!(matches!(
            clamp_mru(&json!(1.5)).unwrap_err(),
            MruError::NotInteger { .. }
        ));
    }

    #[test]
    fn clamp_mru_rejects_out_of_range() {
        assert!(matches!(
            clamp_mru(&json!(0)).unwrap_err(),
            MruError::OutOfRange { .. }
        ));
        assert!(matches!(
            clamp_mru(&json!(21)).unwrap_err(),
            MruError::OutOfRange { .. }
        ));
    }

    #[test]
    fn clamp_mru_accepts_string_integer() {
        assert_eq!(clamp_mru(&json!("3")).unwrap(), 3);
    }

    #[test]
    fn set_mru_limit_evicts_workspaces_past_new_cap() {
        let _env = TestEnv::new();
        // Seed registry with 4 workspaces, cap to 2.
        use crate::memory::workspaces::store::{save_registry, Workspace};
        let entries = (0..4)
            .map(|i| Workspace::new(&format!("/tmp/setcap_{i}")))
            .collect::<Vec<_>>();
        save_registry(&entries).unwrap();
        set_mru_limit(&json!(2)).unwrap();
        let ws = crate::memory::workspaces::store::list_workspaces();
        assert_eq!(ws.len(), 2);
    }

    #[test]
    fn settings_garbage_falls_back_to_default() {
        let _env = TestEnv::new();
        if let Some(parent) = settings_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(settings_path(), "{not json").unwrap();
        assert_eq!(get_mru_limit(), MRU_LIMIT_DEFAULT);
    }
}
