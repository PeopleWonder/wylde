//! Model aliases: short names (e.g. `coder`) for real model ids (#348).
//!
//! A first-class part of the model registry, so a client config that names
//! `coder` survives swapping the model behind it. The mapping is persisted
//! to `<data_dir>/model_aliases.json` (`{"aliases": {"coder": "<model id>"}}`)
//! in the same data dir as the default/active model selections
//! ([`super::model_state`]). It is read from disk on every call (the file is
//! tiny) and changed under a process-wide lock, so edits through the
//! `models.set_alias` / `models.remove_alias` verbs apply immediately and
//! survive restarts.
//!
//! ## Semantics ([`effective`])
//!
//! A stored alias is only a *candidate*. Against the set of installed model
//! ids it is effective when:
//! * its target is installed (an alias to a model that was deleted or not
//!   yet pulled is kept on disk, but not offered); and
//! * no installed model has the alias's own name. A real model id always
//!   wins, including through Ollama's implicit `:latest` tag (`coder`
//!   names `coder:latest`).
//!
//! The gateway's `/v1` routes list and resolve aliases through this same
//! function, so the rules live in one place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};
use wylde_shared::paths::data_dir;

/// Filename of the alias store under the data dir.
pub const ALIASES_FILE: &str = "model_aliases.json";

/// Alias → target model id, ordered by alias.
pub type AliasMap = BTreeMap<String, String>;

/// Serialises read-modify-write of the store within the process.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Where the store lives: `<data_dir>/model_aliases.json`.
pub fn store_path() -> PathBuf {
    data_dir().join(ALIASES_FILE)
}

/// Read the store. A missing file is an empty map; an unreadable or
/// malformed one is logged and treated as empty (never a hard failure for
/// callers that only want to resolve names).
pub fn load(path: &Path) -> AliasMap {
    let Ok(text) = std::fs::read_to_string(path) else {
        return AliasMap::new();
    };
    let parsed = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
        v.get("aliases")?.as_object().map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect::<AliasMap>()
        })
    });
    parsed.unwrap_or_else(|| {
        tracing::warn!(
            "model aliases: ignoring malformed store at {}",
            path.display()
        );
        AliasMap::new()
    })
}

/// Write the store atomically (temp file + rename), creating the dir.
fn save(path: &Path, map: &AliasMap) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&json!({ "aliases": map }))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// An alias is a short name: 1–64 of `A-Z a-z 0-9 . _ -`. No `:` or `/`, so
/// it can never be mistaken for a tagged or namespaced model id.
fn validate_alias(alias: &str) -> Result<(), String> {
    let ok_len = (1..=64).contains(&alias.len());
    let ok_chars = alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err(format!(
            "alias {alias:?} must be 1-64 characters of letters, digits, '.', '_' or '-'"
        ))
    }
}

/// Set (or replace) `alias` → `target` in the store at `path`.
pub fn set_at(path: &Path, alias: &str, target: &str) -> Result<AliasMap, String> {
    let (alias, target) = (alias.trim(), target.trim());
    validate_alias(alias)?;
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return Err(format!("target {target:?} must be a model id"));
    }
    if target == alias {
        return Err(format!("alias {alias:?} cannot point at itself"));
    }
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = load(path);
    map.insert(alias.to_owned(), target.to_owned());
    save(path, &map).map_err(|e| format!("could not save aliases: {e}"))?;
    Ok(map)
}

/// Remove `alias` from the store at `path`; returns the map and whether
/// it existed.
pub fn remove_at(path: &Path, alias: &str) -> Result<(AliasMap, bool), String> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = load(path);
    let existed = map.remove(alias.trim()).is_some();
    if existed {
        save(path, &map).map_err(|e| format!("could not save aliases: {e}"))?;
    }
    Ok((map, existed))
}

/// `name` with Ollama's implicit `:latest` tag made explicit, so `coder`
/// and `coder:latest` compare equal. The tag separator is a `:` after the
/// last `/`.
fn with_latest(name: &str) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    if tail.contains(':') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    }
}

/// Whether `a` and `b` name the same model (implicit `:latest` aware,
/// case-insensitive like Ollama).
pub fn same_model(a: &str, b: &str) -> bool {
    with_latest(a).eq_ignore_ascii_case(&with_latest(b))
}

/// The aliases in `map` that are effective against `installed`: the
/// target is installed and no installed model has the alias's name.
pub fn effective(map: &AliasMap, installed: &[String]) -> Vec<(String, String)> {
    let is_installed = |name: &str| installed.iter().any(|i| same_model(i, name));
    map.iter()
        .filter(|(alias, target)| is_installed(target) && !is_installed(alias))
        .map(|(a, t)| (a.clone(), t.clone()))
        .collect()
}

fn list_reply(map: &AliasMap) -> Value {
    let aliases: Vec<Value> = map
        .iter()
        .map(|(a, t)| json!({"alias": a, "target": t}))
        .collect();
    json!({"count": aliases.len(), "aliases": aliases})
}

fn field<'a>(payload: &'a Value, name: &str) -> Result<&'a str, IpcError> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| IpcError::new("invalid_request", format!("payload.{name} is required")))
}

/// `models.list_aliases` over the store at `path`.
pub fn list_at(path: &Path) -> Reply {
    Reply::ok(list_reply(&load(path)))
}

/// `models.set_alias` over the store at `path`. Payload `{alias, target}`.
pub fn handle_set_at(path: &Path, payload: &Value) -> Reply {
    let (alias, target) = match (field(payload, "alias"), field(payload, "target")) {
        (Ok(a), Ok(t)) => (a, t),
        (Err(e), _) | (_, Err(e)) => return Reply::err(e),
    };
    match set_at(path, alias, target) {
        Ok(map) => Reply::ok(list_reply(&map)),
        Err(msg) => Reply::err(IpcError::new("invalid_request", msg)),
    }
}

/// `models.remove_alias` over the store at `path`. Payload `{alias}`.
pub fn handle_remove_at(path: &Path, payload: &Value) -> Reply {
    let alias = match field(payload, "alias") {
        Ok(a) => a,
        Err(e) => return Reply::err(e),
    };
    match remove_at(path, alias) {
        Ok((map, existed)) => {
            let mut reply = list_reply(&map);
            reply["removed"] = json!(existed);
            Reply::ok(reply)
        }
        Err(msg) => Reply::err(IpcError::new("internal_error", msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(ALIASES_FILE);
        (dir, path)
    }

    fn installed(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn set_list_remove_round_trip_and_persist() {
        let (_d, path) = store();
        assert!(load(&path).is_empty(), "missing store is empty");
        set_at(&path, "coder", "hf.co/u/Coder-GGUF:IQ3").unwrap();
        set_at(&path, "embed", "nomic-embed-text:latest").unwrap();
        // A fresh read from disk sees both: the store is the persistence.
        let map = load(&path);
        assert_eq!(
            map.get("coder").map(String::as_str),
            Some("hf.co/u/Coder-GGUF:IQ3")
        );
        assert_eq!(map.len(), 2);
        // Replacing an alias re-points it.
        set_at(&path, "coder", "qwen2.5-coder:14b").unwrap();
        assert_eq!(load(&path)["coder"], "qwen2.5-coder:14b");
        let (map, existed) = remove_at(&path, "coder").unwrap();
        assert!(existed && !map.contains_key("coder"));
        assert!(
            !remove_at(&path, "coder").unwrap().1,
            "removing twice is a no-op"
        );
        assert_eq!(load(&path).len(), 1);
    }

    #[test]
    fn rejects_bad_aliases_and_targets() {
        let (_d, path) = store();
        for bad in ["", "has space", "qwen:7b", "hf.co/x", &"a".repeat(65)] {
            assert!(set_at(&path, bad, "m:1").is_err(), "{bad:?}");
        }
        assert!(set_at(&path, "coder", "").is_err());
        assert!(set_at(&path, "coder", "two words").is_err());
        assert!(set_at(&path, "same", "same").is_err());
        assert!(
            load(&path).is_empty(),
            "rejected sets never touch the store"
        );
    }

    #[test]
    fn malformed_store_reads_as_empty() {
        let (_d, path) = store();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_empty());
        // …and a set rewrites it cleanly.
        set_at(&path, "coder", "m:1").unwrap();
        assert_eq!(load(&path)["coder"], "m:1");
    }

    #[test]
    fn effective_requires_an_installed_target() {
        let mut map = AliasMap::new();
        map.insert("coder".into(), "hf.co/u/Coder-GGUF:IQ3".into());
        map.insert("gone".into(), "deleted:1".into());
        let e = effective(&map, &installed(&["hf.co/u/Coder-GGUF:IQ3"]));
        assert_eq!(
            e,
            vec![("coder".to_owned(), "hf.co/u/Coder-GGUF:IQ3".to_owned())]
        );
    }

    #[test]
    fn a_real_model_id_beats_an_alias_of_the_same_name() {
        let mut map = AliasMap::new();
        map.insert("coder".into(), "hf.co/u/Coder-GGUF:IQ3".into());
        map.insert("llama3".into(), "hf.co/u/Coder-GGUF:IQ3".into());
        // A model literally named `coder:latest` shadows the `coder` alias
        // (Ollama's implicit :latest), and `llama3` is shadowed by
        // `LLAMA3:latest` case-insensitively.
        let e = effective(
            &map,
            &installed(&["hf.co/u/Coder-GGUF:IQ3", "coder:latest", "LLAMA3"]),
        );
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn same_model_understands_the_implicit_latest_tag() {
        assert!(same_model("nomic-embed-text", "nomic-embed-text:latest"));
        assert!(same_model("hf.co/u/Repo-GGUF:Q4", "hf.co/u/Repo-GGUF:Q4"));
        assert!(!same_model("qwen:7b", "qwen:14b"));
        assert!(!same_model("qwen:7b", "qwen"), ":7b is not :latest");
    }

    #[test]
    fn handlers_validate_payloads_and_reply_with_the_list() {
        let (_d, path) = store();
        let r = handle_set_at(&path, &json!({"alias": "coder"}));
        assert_eq!(r.error.unwrap().code, "invalid_request");
        let r = handle_set_at(&path, &json!({"alias": "coder", "target": "m:1"}));
        assert!(r.ok, "{r:?}");
        assert_eq!(
            r.data["aliases"],
            json!([{"alias": "coder", "target": "m:1"}])
        );
        assert_eq!(list_at(&path).data["count"], 1);
        let r = handle_remove_at(&path, &json!({"alias": "coder"}));
        assert_eq!(r.data["removed"], true);
        assert_eq!(r.data["count"], 0);
    }
}
