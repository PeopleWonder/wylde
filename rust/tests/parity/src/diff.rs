//! Structural parity diffing.
//!
//! Two correct implementations will never produce byte-identical responses:
//! timestamps, UUID lease ids, process ids, and hardware readings differ on
//! every run. [`normalize`] rewrites those volatile fields to a fixed
//! placeholder so [`assert_parity`] can compare what actually matters — the
//! shape and the stable values — and still fail loudly on a real divergence.

use serde_json::Value;

/// Value substituted in for every normalized (volatile) field.
const PLACEHOLDER: &str = "<normalized>";

/// Replace the value at each dotted `path` with [`PLACEHOLDER`].
///
/// Path syntax is dotted segments. A `*` segment matches every key of an
/// object or every element of an array, so `data.leases.*.lease_id` reaches
/// the `lease_id` of every lease. A numeric segment indexes an array.
///
/// A path that does not resolve is a no-op — so one broad ignore list can
/// be reused across cases whose responses have different shapes.
pub fn normalize(value: &mut Value, paths: &[&str]) {
    for path in paths {
        let segments: Vec<&str> = path.split('.').collect();
        normalize_at(value, &segments);
    }
}

fn normalize_at(value: &mut Value, segments: &[&str]) {
    let Some((head, rest)) = segments.split_first() else {
        *value = Value::String(PLACEHOLDER.to_string());
        return;
    };
    match value {
        Value::Object(map) => {
            if *head == "*" {
                for child in map.values_mut() {
                    normalize_at(child, rest);
                }
            } else if let Some(child) = map.get_mut(*head) {
                normalize_at(child, rest);
            }
        }
        Value::Array(items) => {
            if *head == "*" {
                for child in items.iter_mut() {
                    normalize_at(child, rest);
                }
            } else if let Ok(index) = head.parse::<usize>() {
                if let Some(child) = items.get_mut(index) {
                    normalize_at(child, rest);
                }
            }
        }
        _ => {}
    }
}

/// Compare the Python and Rust responses after normalizing `volatile` paths
/// out of both. Returns `Ok(())` on parity, or `Err(<readable diff>)` —
/// the diff is a `pretty_assertions` rendering, left = Python, right = Rust.
///
/// Returning a `Result` (rather than panicking) lets a test run every case
/// and report all divergences at once instead of stopping at the first.
pub fn compare(case: &str, python: &Value, rust: &Value, volatile: &[&str]) -> Result<(), String> {
    let mut py = python.clone();
    let mut rs = rust.clone();
    normalize(&mut py, volatile);
    normalize(&mut rs, volatile);
    if py == rs {
        Ok(())
    } else {
        Err(format!(
            "case '{case}' diverged (left = Python, right = Rust):\n{}",
            pretty_assertions::Comparison::new(&py, &rs),
        ))
    }
}

/// [`compare`], but panics on mismatch. Convenient for a single-case test.
pub fn assert_parity(case: &str, python: &Value, rust: &Value, volatile: &[&str]) {
    if let Err(diff) = compare(case, python, rust, volatile) {
        panic!("{diff}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_nested_and_wildcard_paths() {
        let mut v = json!({
            "ts": "2026-05-20T00:00:00Z",
            "leases": [{ "id": "abc" }, { "id": "def" }],
        });
        normalize(&mut v, &["ts", "leases.*.id"]);
        assert_eq!(v["ts"], json!("<normalized>"));
        assert_eq!(v["leases"][0]["id"], json!("<normalized>"));
        assert_eq!(v["leases"][1]["id"], json!("<normalized>"));
    }

    #[test]
    fn missing_path_is_a_noop() {
        let mut v = json!({ "ok": true });
        normalize(&mut v, &["error.message", "data.lease_id"]);
        assert_eq!(v, json!({ "ok": true }));
    }

    #[test]
    fn parity_holds_when_only_volatile_fields_differ() {
        let py = json!({ "ok": true, "data": { "lease_id": "py-uuid", "bytes": 1024 } });
        let rs = json!({ "ok": true, "data": { "lease_id": "rs-uuid", "bytes": 1024 } });
        assert_parity("reserve", &py, &rs, &["data.lease_id"]);
    }
}
