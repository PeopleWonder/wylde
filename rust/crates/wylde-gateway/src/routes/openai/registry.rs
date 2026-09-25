//! The model-registry view every `/v1` route lists and resolves against
//! (#348).
//!
//! One snapshot holds three things:
//! * **listed**: the catalog ids `/v1/models` shows (chat-visible LLMs plus
//!   embedding models from the harness `models.list`);
//! * **installed**: every model id the registry knows, used to decide which
//!   aliases are live;
//! * **aliases**: the *effective* aliases.
//!
//! Aliases come from the harness registry (`models.list_aliases`), filtered
//! by the registry's own rules
//! (`wylde_harness::model_registry::aliases::effective`): the target must be
//! installed, and a real model id beats an alias of the same name. The
//! deprecated `WYLDE_OPENAI_MODEL_ALIASES` env var is only a fallback: its
//! entries apply where the registry doesn't define that alias, and cover
//! everything when the harness can't be reached.
//!
//! If the harness is down, the catalog falls back to Ollama's installed
//! list (`ollama.list_models`). If both are down, the catalog is an error
//! for `/v1/models`, but inference still resolves aliases best-effort
//! (unfiltered, since installation can't be checked).
//!
//! The snapshot is cached for [`REGISTRY_TTL`], so autocomplete doesn't pay
//! a registry round-trip per keystroke. An alias change is live within that
//! window.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use wylde_harness::model_registry::aliases::{effective, AliasMap};

use super::aliases::Aliases;
use super::errors::OpenAiError;
use super::OpenAiState;

/// How long a snapshot is reused.
pub const REGISTRY_TTL: Duration = Duration::from_secs(5);

/// One registry snapshot (see the module docs).
#[derive(Debug, Clone)]
pub struct RegistryView {
    pub listed: Vec<String>,
    pub installed: Vec<String>,
    pub aliases: Aliases,
    /// Why the catalog couldn't be built, if it couldn't.
    pub catalog_error: Option<OpenAiError>,
}

/// The per-router snapshot cache.
#[derive(Default)]
pub struct RegistryCache(Mutex<Option<(Instant, Arc<RegistryView>)>>);

/// Registry entries exposed as catalog models.
fn listed_ids(reply: &Value) -> Vec<String> {
    entries(reply)
        .filter(|m| {
            let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
            let visible = m
                .get("chat_visible")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (kind == "llm" && visible) || kind == "embed"
        })
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Every model id in a `models.list` reply.
fn all_ids(reply: &Value) -> Vec<String> {
    entries(reply)
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn entries(reply: &Value) -> impl Iterator<Item = &Value> {
    reply
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Model names from an Ollama `/api/tags` reply.
fn ollama_ids(reply: &Value) -> Vec<String> {
    entries(reply)
        .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// A `models.list_aliases` reply as a map.
fn stored_aliases(reply: &Value) -> AliasMap {
    reply
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                a.get("alias")?.as_str()?.to_owned(),
                a.get("target")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

async fn build(state: &OpenAiState) -> RegistryView {
    let (listed, installed, catalog_error) = match state.backend.registry_models().await {
        Ok(reply) => (listed_ids(&reply), all_ids(&reply), None),
        Err(e) => {
            tracing::warn!(
                "openai: models.list unavailable ({}), falling back to Ollama",
                e.code
            );
            match state.backend.ollama_models().await {
                Ok(reply) => {
                    let ids = ollama_ids(&reply);
                    (ids.clone(), ids, None)
                }
                Err(e) => (Vec::new(), Vec::new(), Some(OpenAiError::from_ipc(&e, ""))),
            }
        }
    };
    let mut map = match state.backend.registry_aliases().await {
        Ok(reply) => stored_aliases(&reply),
        Err(e) => {
            tracing::warn!("openai: models.list_aliases unavailable ({})", e.code);
            AliasMap::new()
        }
    };
    // Deprecated env stopgap: only where the registry has no such alias.
    for (alias, target) in state.env_aliases.iter() {
        map.entry(alias.to_owned())
            .or_insert_with(|| target.to_owned());
    }
    let pairs = if installed.is_empty() {
        // Nothing to check installation against: resolve best-effort.
        map.into_iter().collect()
    } else {
        effective(&map, &installed)
    };
    RegistryView {
        listed,
        installed,
        aliases: Aliases::from_pairs(pairs),
        catalog_error,
    }
}

/// The current snapshot, rebuilt when older than [`REGISTRY_TTL`].
pub async fn view(state: &OpenAiState) -> Arc<RegistryView> {
    if let Some((at, v)) = state.registry.0.lock().expect("registry cache").as_ref() {
        if at.elapsed() < REGISTRY_TTL {
            return v.clone();
        }
    }
    let fresh = Arc::new(build(state).await);
    *state.registry.0.lock().expect("registry cache") = Some((Instant::now(), fresh.clone()));
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn listed_ids_keep_visible_chat_and_all_embed_models() {
        let reply = json!({"models": [
            {"id": "coder", "kind": "llm", "chat_visible": true},
            {"id": "hidden-llm", "kind": "llm", "chat_visible": false},
            {"id": "nomic", "kind": "embed", "chat_visible": false},
            {"id": "whisper", "kind": "stt", "chat_visible": false},
        ]});
        assert_eq!(listed_ids(&reply), vec!["coder", "nomic"]);
        assert_eq!(all_ids(&reply).len(), 4, "installed covers every kind");
    }

    #[test]
    fn ollama_ids_read_tag_names() {
        let reply = json!({"models": [{"name": "a:1"}, {"model": "no-name"}, {"name": "b"}]});
        assert_eq!(ollama_ids(&reply), vec!["a:1", "b"]);
    }

    #[test]
    fn stored_aliases_parse_the_list_reply() {
        let reply = json!({"count": 2, "aliases": [
            {"alias": "coder", "target": "m:1"}, {"alias": "bad"}, {"alias": "embed", "target": "e:1"}
        ]});
        let map = stored_aliases(&reply);
        assert_eq!(map.len(), 2);
        assert_eq!(map["coder"], "m:1");
    }
}
