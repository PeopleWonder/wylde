//! Pinned load options — stop per-request options from reloading a model.
//!
//! Ollama reloads a resident model whenever a request's load-affecting
//! options differ from the ones it was loaded with. That includes a request
//! that simply *omits* `num_ctx`: it falls back to the model default and
//! reloads (measured: load at 4096 → a request without `num_ctx` reloaded at
//! 32768, ~2 s). When FIM and chat share a model with different context
//! sizes, every switch would pay a full reload.
//!
//! A caller opts in per request with `pin_load_options: true` (a pipe-only
//! knob, always stripped before forwarding). For a model that `/api/ps`
//! reports as loaded, `options.num_ctx` is set to the loaded context and the
//! other load-affecting keys are dropped, so the request reuses the resident
//! model as-is. An unloaded model is left alone: the request's options
//! decide how it loads. Existing callers that deliberately request a larger
//! context don't opt in and keep today's behaviour.

use reqwest::{Method, StatusCode};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::estimate::model_matches;
use crate::upstream::Upstream;

/// Payload knob that opts a request into pinning.
pub const PIN_KNOB: &str = "pin_load_options";

/// `options` keys that change how Ollama loads a model (not how it samples).
/// `num_ctx` is replaced with the loaded value; the rest are dropped.
pub const LOAD_OPTION_KEYS: &[&str] = &[
    "num_ctx",
    "num_batch",
    "num_gpu",
    "main_gpu",
    "num_thread",
    "use_mmap",
    "use_mlock",
    "low_vram",
];

/// Whether a model is resident, per `/api/ps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Loaded; `context_length` is absent on Ollama builds that don't report it.
    Loaded {
        context_length: Option<u64>,
    },
    NotLoaded,
    /// `/api/ps` failed — residency can't be established.
    Unknown,
}

impl Residency {
    pub fn is_loaded(self) -> bool {
        matches!(self, Residency::Loaded { .. })
    }
}

/// Look `model` up in `/api/ps`.
pub async fn residency(up: &Upstream, model: &str) -> Residency {
    let cfg = Config::get();
    let resp = match up
        .request(Method::GET, "/api/ps", None, cfg.list_loaded_timeout_s)
        .await
    {
        Ok(r) if r.status() == StatusCode::OK => r,
        _ => return Residency::Unknown,
    };
    let Ok(body) = resp.json::<Value>().await else {
        return Residency::Unknown;
    };
    let Some(models) = body.get("models").and_then(Value::as_array) else {
        return Residency::Unknown;
    };
    match models.iter().find(|m| model_matches(m, model)) {
        Some(entry) => Residency::Loaded {
            context_length: entry.get("context_length").and_then(Value::as_u64),
        },
        None => Residency::NotLoaded,
    }
}

/// Remove the [`PIN_KNOB`] from `body`, returning whether it was set.
pub fn take_knob(body: &mut Value) -> bool {
    body.as_object_mut()
        .and_then(|m| m.remove(PIN_KNOB))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Pin `body.options` to a loaded model's `context_length`: set `num_ctx`
/// to it (inserting `options` if absent) and drop the other load keys.
/// Sampling options (`temperature`, `stop`, …) are untouched.
pub fn pin(body: &mut Value, context_length: u64) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let options = obj
        .entry("options")
        .or_insert_with(|| Value::Object(Map::new()));
    if !options.is_object() {
        *options = Value::Object(Map::new());
    }
    if let Some(opts) = options.as_object_mut() {
        for key in LOAD_OPTION_KEYS {
            opts.remove(*key);
        }
        opts.insert("num_ctx".to_owned(), Value::from(context_length));
    }
}

/// Apply pinning if the caller asked for it (the knob is always stripped).
/// Returns the residency it looked up, or `None` when pinning wasn't
/// requested, so callers that also need residency can reuse the lookup.
pub async fn apply(up: &Upstream, model: &str, body: &mut Value) -> Option<Residency> {
    if !take_knob(body) {
        return None;
    }
    let res = residency(up, model).await;
    if let Residency::Loaded {
        context_length: Some(ctx),
    } = res
    {
        pin(body, ctx);
    }
    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ps_server(ps: Value) -> (MockServer, std::sync::Arc<Upstream>) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ps))
            .mount(&server)
            .await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    #[test]
    fn pin_replaces_num_ctx_and_drops_other_load_keys() {
        let mut body = json!({"options": {"num_ctx": 4096, "num_gpu": 99, "temperature": 0.2}});
        pin(&mut body, 16384);
        assert_eq!(body["options"]["num_ctx"], 16384);
        assert!(body["options"].get("num_gpu").is_none());
        assert_eq!(
            body["options"]["temperature"], 0.2,
            "sampling options survive"
        );
    }

    #[test]
    fn pin_inserts_num_ctx_when_request_omits_it() {
        // Omitting num_ctx is what reloads at the model default — pin it.
        let mut body = json!({"model": "m"});
        pin(&mut body, 8192);
        assert_eq!(body["options"], json!({"num_ctx": 8192}));
    }

    #[test]
    fn take_knob_strips_and_reports() {
        let mut body = json!({"model": "m", "pin_load_options": true});
        assert!(take_knob(&mut body));
        assert!(body.get(PIN_KNOB).is_none());
        let mut body = json!({"model": "m", "pin_load_options": false});
        assert!(!take_knob(&mut body));
        assert!(body.get(PIN_KNOB).is_none());
        assert!(!take_knob(&mut json!({"model": "m"})));
    }

    #[tokio::test]
    async fn residency_reads_loaded_context_length() {
        let (_s, up) = ps_server(json!({"models": [
            {"name": "qwen:7b", "model": "qwen:7b", "size": 1, "context_length": 16384}
        ]}))
        .await;
        assert_eq!(
            residency(&up, "qwen:7b").await,
            Residency::Loaded {
                context_length: Some(16384)
            }
        );
        assert_eq!(residency(&up, "other").await, Residency::NotLoaded);
    }

    #[tokio::test]
    async fn residency_unknown_when_ps_fails() {
        let up = crate::upstream::for_test("http://127.0.0.1:1");
        assert_eq!(residency(&up, "m").await, Residency::Unknown);
    }

    #[tokio::test]
    async fn apply_pins_a_loaded_model_only_when_asked() {
        let (_s, up) = ps_server(json!({"models": [
            {"name": "qwen:7b", "context_length": 16384}
        ]}))
        .await;
        let mut asked =
            json!({"model": "qwen:7b", "pin_load_options": true, "options": {"num_ctx": 2048}});
        let res = apply(&up, "qwen:7b", &mut asked).await;
        assert!(res.is_some_and(Residency::is_loaded));
        assert_eq!(asked["options"]["num_ctx"], 16384);

        let mut not_asked = json!({"model": "qwen:7b", "options": {"num_ctx": 2048}});
        assert!(apply(&up, "qwen:7b", &mut not_asked).await.is_none());
        assert_eq!(not_asked["options"]["num_ctx"], 2048, "no knob → untouched");
    }

    #[tokio::test]
    async fn apply_leaves_an_unloaded_model_alone() {
        let (_s, up) = ps_server(json!({"models": []})).await;
        let mut body =
            json!({"model": "m", "pin_load_options": true, "options": {"num_ctx": 4096}});
        assert_eq!(apply(&up, "m", &mut body).await, Some(Residency::NotLoaded));
        assert_eq!(body["options"]["num_ctx"], 4096);
        assert!(body.get(PIN_KNOB).is_none());
    }
}
