//! `voice` resource — the speech inference verb migration (consolidation
//! Slice 4b, `docs/plans/tool-registry-consolidation.md` §6 follow-up).
//! Folds the four `voice.transcribe* / synthesize*` named tools into one
//! action-shaped resource — two actions with a `stream` flag:
//!
//! | Verb call | Delegates to |
//! |---|---|
//! | `wylde_execute("voice", "transcribe", {params:{audio_path \| audio_b64, language?}})` | [`tools_voice::run_transcribe`] |
//! | `wylde_execute("voice", "transcribe", {params:{…, stream:true}})` | [`tools_voice::run_transcribe_stream`] |
//! | `wylde_execute("voice", "synthesize", {params:{text \| phonemes, voice?, speed?}})` | [`tools_voice::run_synthesize`] |
//! | `wylde_execute("voice", "synthesize", {params:{…, stream:true}})` | [`tools_voice::run_synthesize_stream`] |
//!
//! ## Shape — two actions, one stream flag (4 → 2)
//!
//! Transcribe and synthesize are pure inference (no device, no resource
//! identity), so they are `execute` actions, not CRUD. The unary vs
//! aggregated-streaming split that produced four named tools collapses
//! into a single `params.stream` boolean: `stream:true` routes to the
//! aggregator bridge, which still returns one JSON value (the model never
//! consumes a live stream). Pure inference → `destructive_ops` is empty.
//!
//! ## Not the voice *device* tools
//!
//! `voice.mic.{start,stop}` and `voice.wakeword.{start,stop}` are
//! **permanent** imperative survivors (plan §7) — stateful device
//! lifecycle with no resource identity. They are NOT part of this
//! resource and stay named by design.
//!
//! ## Adapter pattern — no logic duplication
//!
//! Each branch passes the verb's `params` object straight to the existing
//! `voice.*` primitive. The named tools stay registered and unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use wylde_shared::ipc::IpcError;

use crate::tooling::resource::definition::{
    describe_value, op_handler, OpHandler, ResourceDefinition, ResourceOp, ResourceRequest, Scope,
    ToolContext,
};
use crate::tooling::resource::ResourceRegistry;
use crate::tooling::tools::voice as tools_voice;

/// Register the `voice` resource into the built-in registry.
pub fn register_voice_resource(reg: &mut ResourceRegistry) {
    let mut operations: HashMap<ResourceOp, Arc<dyn OpHandler>> = HashMap::new();
    operations.insert(ResourceOp::Execute, op_handler(voice_execute));

    reg.register_builtin(ResourceDefinition {
        resource_type: "voice",
        display_name: "Voice (speech inference)",
        description: "Speech inference via wylde-voice. execute action='transcribe' \
                      (STT: audio → text) or action='synthesize' (TTS: text → WAV). \
                      params.stream=true uses the aggregating streaming path. The voice \
                      mic/wake-word device tools stay named (not part of this resource).",
        scope: Scope::Global,
        identifier_fields: &[],
        filter_fields: &[],
        operations,
        // Pure inference — no device opened, nothing to gate.
        destructive_ops: &[],
        describe: describe_value(describe_voice),
    });
}

/// `wylde_execute("voice", "transcribe"|"synthesize", {params:{…, stream?}})`
/// → the matching `voice.*` primitive. `params.stream` selects the
/// aggregated-streaming bridge.
fn voice_execute(
    req: ResourceRequest,
    _cfg: &'static crate::config::Config,
    _ctx: ToolContext,
) -> impl std::future::Future<Output = Result<Value, IpcError>> {
    let action = req.action.clone().unwrap_or_default();
    let stream = req
        .params
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let args = as_object(req.params);
    async move {
        match (action.as_str(), stream) {
            ("transcribe", false) => tools_voice::run_transcribe(Value::Object(args)).await,
            ("transcribe", true) => tools_voice::run_transcribe_stream(Value::Object(args)).await,
            ("synthesize", false) => tools_voice::run_synthesize(Value::Object(args)).await,
            ("synthesize", true) => tools_voice::run_synthesize_stream(Value::Object(args)).await,
            ("", _) => Ok(json!({
                "status": "error",
                "error": "wylde_execute(\"voice\", …) requires an 'action' of \"transcribe\" or \"synthesize\"",
                "known_actions": ["transcribe", "synthesize"],
            })),
            (other, _) => Ok(json!({
                "status": "error",
                "error": format!("unknown voice action {other:?}; expected \"transcribe\" or \"synthesize\""),
                "known_actions": ["transcribe", "synthesize"],
            })),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn as_object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

// ── describe() ───────────────────────────────────────────────────────

fn describe_voice() -> Value {
    json!({
        "resource_type": "voice",
        "display_name": "Voice (speech inference)",
        "description": "STT / TTS via wylde-voice. Device (mic/wake-word) tools are separate, named.",
        "scope": "global",
        "identifier_fields": [],
        "filter_fields": [],
        "operations": {
            "execute": {
                "verb": "wylde_execute",
                "destructive": false,
                "description": "Run speech inference. Two actions; params.stream=true aggregates the streaming path.",
                "actions": [
                    {"name": "transcribe", "description": "Whisper STT: audio_path/audio_b64 → transcript"},
                    {"name": "synthesize", "description": "Kokoro TTS: text/phonemes → 16-bit PCM WAV (base64)"}
                ],
                "schema": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["transcribe", "synthesize"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "stream": {"type": "boolean", "description": "Use the aggregating streaming path (still one JSON reply; default false)"},
                                "audio_path": {"type": "string", "description": "transcribe: path to a WAV file"},
                                "audio_b64": {"type": "string", "description": "transcribe: base64-encoded WAV bytes"},
                                "language": {"type": "string", "description": "transcribe: ISO 639-1 code (default 'en')"},
                                "text": {"type": "string", "description": "synthesize: English text (G2P'd to phonemes)"},
                                "phonemes": {"type": "string", "description": "synthesize: explicit IPA string (overrides text)"},
                                "voice": {"type": "string", "description": "synthesize: Kokoro voice name (default 'af_heart')"},
                                "speed": {"type": "number", "description": "synthesize: playback rate [0.5, 2.0] (default 1.0)"}
                            }
                        }
                    },
                    "required": ["action"]
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> &'static crate::config::Config {
        Box::leak(Box::new(crate::config::Config::default_for_tests()))
    }

    fn reg() -> ResourceRegistry {
        let mut r = ResourceRegistry::empty();
        register_voice_resource(&mut r);
        r
    }

    async fn dispatch(req: ResourceRequest) -> Value {
        let r = reg();
        let def = r.lookup("voice").expect("voice registered");
        let handler = def
            .operations
            .get(&ResourceOp::Execute)
            .expect("execute registered")
            .clone();
        let ctx = ToolContext::for_op("voice", ResourceOp::Execute, None);
        handler.call(req, cfg(), ctx).await.unwrap()
    }

    #[test]
    fn registers_voice_resource() {
        assert!(reg().lookup("voice").is_some());
    }

    #[test]
    fn supports_execute_only_and_is_not_destructive() {
        let r = reg();
        let def = r.lookup("voice").unwrap();
        assert_eq!(def.supported_ops(), vec![ResourceOp::Execute]);
        assert!(!def.is_destructive(ResourceOp::Execute));
    }

    #[test]
    fn describe_enumerates_two_actions() {
        let v = describe_voice();
        let actions = v["operations"]["execute"]["actions"].as_array().unwrap();
        let names: Vec<&str> = actions
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["transcribe", "synthesize"]);
    }

    #[tokio::test]
    async fn execute_missing_action_errors_cleanly() {
        let out = dispatch(ResourceRequest::default()).await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["transcribe", "synthesize"]));
    }

    #[tokio::test]
    async fn execute_unknown_action_errors_cleanly() {
        let out = dispatch(ResourceRequest {
            action: Some("emote".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(out["status"], "error");
        assert_eq!(out["known_actions"], json!(["transcribe", "synthesize"]));
    }

    // With no wylde-voice running, a valid action forwards to the bridge
    // and surfaces an "unreachable" error envelope — NOT a "missing
    // action" error. Proves each (action, stream) branch routes through.
    #[tokio::test]
    async fn transcribe_routes_through_to_the_bridge() {
        for stream in [false, true] {
            let out = dispatch(ResourceRequest {
                action: Some("transcribe".into()),
                params: json!({"audio_path": "/tmp/x.wav", "stream": stream}),
                ..Default::default()
            })
            .await;
            // Either ok or an error that is NOT about a missing/unknown action.
            if out["status"] == "error" {
                let e = out["error"].as_str().unwrap();
                assert!(!e.contains("requires an 'action'"), "stream={stream}: {e}");
                assert!(!e.contains("unknown voice action"), "stream={stream}: {e}");
            }
        }
    }

    #[tokio::test]
    async fn synthesize_routes_through_to_the_bridge() {
        for stream in [false, true] {
            let out = dispatch(ResourceRequest {
                action: Some("synthesize".into()),
                params: json!({"text": "hello", "stream": stream}),
                ..Default::default()
            })
            .await;
            if out["status"] == "error" {
                let e = out["error"].as_str().unwrap();
                assert!(!e.contains("requires an 'action'"), "stream={stream}: {e}");
                assert!(!e.contains("unknown voice action"), "stream={stream}: {e}");
            }
        }
    }
}
