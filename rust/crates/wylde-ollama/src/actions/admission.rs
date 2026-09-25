//! Lease admission for the generate actions, including the FIM rule.
//!
//! Every generate call estimates the model's VRAM footprint and takes a
//! lease through a [`Leaser`] (the broker in production). A normal call
//! behaves like `ollama.chat`: a refused lease is an error, and an
//! unreachable broker means "proceed without a lease".
//!
//! A FIM (autocomplete) call passes `fim: true`. It gets the FIM lease
//! priority ([`Config::default_fim_priority`]) unless it names a `priority`,
//! and it must never make an agent's model give up VRAM. Our leases never
//! preempt (see [`crate::lease`]), but Ollama itself will unload a resident
//! model to load a new one. So a FIM call for a model that is **not
//! already loaded** is admitted only when its lease fits in free VRAM.
//! A grant that spilled into DRAM, a broker refusal, or an unreachable
//! broker (which can't confirm it fits) all return `insufficient_vram`
//! immediately: the editor shows no suggestion, and the agent keeps its
//! model. A FIM call for a model that's already loaded needs no load, so
//! it is admitted like a normal call.

use serde_json::Value;
use wylde_shared::ipc::IpcError;

use crate::actions::error::{insufficient_vram, model_not_found_err};
use crate::config::Config;
use crate::estimate::{estimate_vram_bytes, VramEstimate};
use crate::lease::{LeaseHold, LeaseRequest, Leaser, Priority};
use crate::load_opts::{self, Residency};
use crate::upstream::Upstream;

/// Payload knob marking a generate call as FIM/autocomplete.
pub const FIM_KNOB: &str = "fim";

/// Broker codes meaning "the lease was refused" (not "unreachable").
const REFUSAL_CODES: &[&str] = &[
    "vram_admission_denied",
    "insufficient_vram",
    "would_exceed_total",
];

/// Remove [`FIM_KNOB`] from `body`, returning whether it was set.
pub fn take_fim(body: &mut Value) -> bool {
    body.as_object_mut()
        .and_then(|m| m.remove(FIM_KNOB))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Lease priority: an explicit `priority` in the payload wins; otherwise a
/// FIM call gets [`Config::default_fim_priority`] and anything else the
/// chat default.
pub fn priority_for(payload: &Value, fim: bool) -> Priority {
    match payload.get("priority").and_then(Value::as_i64) {
        Some(p) => Priority::Explicit(p),
        None if fim => Priority::Explicit(Config::get().default_fim_priority),
        None => Priority::Default,
    }
}

/// Admit one generate call. Returns the held lease (`None` when the call
/// proceeds without one because the broker is down), or the error to
/// return. `residency` reuses a `/api/ps` lookup the caller already made.
pub async fn admit(
    up: &Upstream,
    leaser: &dyn Leaser,
    model: &str,
    payload: &Value,
    fim: bool,
    residency: Option<Residency>,
) -> Result<Option<Box<dyn LeaseHold>>, IpcError> {
    let bytes_hint = match estimate_vram_bytes(up, model).await {
        VramEstimate::Bytes(b) => Some(b),
        VramEstimate::NotPulled => return Err(model_not_found_err(model)),
    };
    // Only FIM needs residency; Unknown counts as not loaded (conservative).
    let needs_load = if fim {
        let res = match residency {
            Some(r) => r,
            None => load_opts::residency(up, model).await,
        };
        !res.is_loaded()
    } else {
        false
    };

    let acquired = leaser
        .acquire(LeaseRequest {
            model: model.to_owned(),
            bytes_hint,
            priority: priority_for(payload, fim),
            nonce: None,
        })
        .await;

    match acquired {
        Ok(hold) if needs_load && hold.spilled() => {
            drop(hold);
            Err(insufficient_vram(
                model,
                "not loaded, and it would not fit in free VRAM without displacing a loaded model",
            ))
        }
        Ok(hold) => Ok(Some(hold)),
        Err(e) if e.code == "broker_unreachable" && needs_load => Err(insufficient_vram(
            model,
            "not loaded, and the VRAM broker is unreachable so it can't be confirmed to fit",
        )),
        Err(e) if e.code == "broker_unreachable" => {
            tracing::warn!(
                "wylde-ollama: generate broker unreachable, proceeding without lease: {}",
                e.message
            );
            Ok(None)
        }
        Err(e) if fim && REFUSAL_CODES.contains(&e.code.as_str()) => Err(insufficient_vram(
            model,
            format!("lease refused: {}", e.message),
        )),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::testing::{FakeLeaser, Outcome};
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Upstream whose `/api/ps` lists `loaded` and whose `/api/tags` lists
    /// both `loaded` and `on_disk` (so estimates resolve without a 404).
    async fn upstream(loaded: &[&str], on_disk: &[&str]) -> (MockServer, Arc<Upstream>) {
        let server = MockServer::start().await;
        let ps: Vec<Value> = loaded
            .iter()
            .map(|m| json!({"name": m, "size": 1_000_000, "context_length": 8192}))
            .collect();
        let tags: Vec<Value> = loaded
            .iter()
            .chain(on_disk)
            .map(|m| json!({"name": m, "size": 1_000_000}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": ps})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": tags})))
            .mount(&server)
            .await;
        let up = crate::upstream::for_test(&server.uri());
        (server, up)
    }

    fn code(r: Result<Option<Box<dyn LeaseHold>>, IpcError>) -> String {
        match r {
            Ok(_) => "ok".into(),
            Err(e) => e.code,
        }
    }

    #[test]
    fn take_fim_strips_and_reports() {
        let mut b = json!({"fim": true});
        assert!(take_fim(&mut b));
        assert!(b.get(FIM_KNOB).is_none());
        assert!(!take_fim(&mut json!({})));
    }

    #[test]
    fn priority_explicit_beats_fim_beats_default() {
        let fim = Config::get().default_fim_priority;
        assert_eq!(priority_for(&json!({}), true).resolve(), fim);
        assert_eq!(priority_for(&json!({"priority": 5}), true).resolve(), 5);
        assert_eq!(
            priority_for(&json!({}), false).resolve(),
            Config::get().default_chat_priority
        );
        assert!(
            fim > Config::get().default_chat_priority,
            "FIM outranks chat"
        );
    }

    #[tokio::test]
    async fn fim_unloaded_model_that_spills_is_refused_and_lease_released() {
        let (_s, up) = upstream(&["agent"], &["fim-model"]).await;
        let leaser = FakeLeaser::new(Outcome::Grant { spilled: true });
        let r = admit(&up, leaser.as_ref(), "fim-model", &json!({}), true, None).await;
        assert_eq!(code(r), "insufficient_vram");
        assert_eq!((leaser.acquired(), leaser.released()), (1, 1));
    }

    #[tokio::test]
    async fn fim_unloaded_model_that_fits_is_admitted_at_fim_priority() {
        let (_s, up) = upstream(&["agent"], &["fim-model"]).await;
        let leaser = FakeLeaser::new(Outcome::Grant { spilled: false });
        let r = admit(&up, leaser.as_ref(), "fim-model", &json!({}), true, None).await;
        assert_eq!(code(r), "ok");
        assert_eq!(
            leaser.priorities(),
            vec![Config::get().default_fim_priority]
        );
    }

    #[tokio::test]
    async fn fim_loaded_model_is_admitted_even_if_grant_spilled() {
        // Already resident → no load → nothing to displace.
        let (_s, up) = upstream(&["coder"], &[]).await;
        let leaser = FakeLeaser::new(Outcome::Grant { spilled: true });
        let r = admit(&up, leaser.as_ref(), "coder", &json!({}), true, None).await;
        assert_eq!(code(r), "ok");
    }

    #[tokio::test]
    async fn fim_unloaded_model_with_broker_down_is_refused() {
        let (_s, up) = upstream(&[], &["fim-model"]).await;
        let leaser = FakeLeaser::new(Outcome::Fail("broker_unreachable"));
        let r = admit(&up, leaser.as_ref(), "fim-model", &json!({}), true, None).await;
        assert_eq!(code(r), "insufficient_vram");
    }

    #[tokio::test]
    async fn fim_broker_refusal_maps_to_insufficient_vram() {
        let (_s, up) = upstream(&["coder"], &[]).await;
        let leaser = FakeLeaser::new(Outcome::Fail("vram_admission_denied"));
        let r = admit(&up, leaser.as_ref(), "coder", &json!({}), true, None).await;
        assert_eq!(code(r), "insufficient_vram");
    }

    #[tokio::test]
    async fn normal_call_with_broker_down_proceeds_without_lease() {
        let (_s, up) = upstream(&[], &["m"]).await;
        let leaser = FakeLeaser::new(Outcome::Fail("broker_unreachable"));
        let r = admit(&up, leaser.as_ref(), "m", &json!({}), false, None).await;
        assert!(matches!(r, Ok(None)));
    }

    #[tokio::test]
    async fn normal_call_refusal_passes_through_unchanged() {
        let (_s, up) = upstream(&[], &["m"]).await;
        let leaser = FakeLeaser::new(Outcome::Fail("vram_admission_denied"));
        let r = admit(&up, leaser.as_ref(), "m", &json!({}), false, None).await;
        assert_eq!(code(r), "vram_admission_denied");
    }

    #[tokio::test]
    async fn model_not_pulled_is_refused_before_any_lease() {
        let (_s, up) = upstream(&[], &[]).await;
        let leaser = FakeLeaser::new(Outcome::Grant { spilled: false });
        let r = admit(&up, leaser.as_ref(), "ghost", &json!({}), true, None).await;
        assert_eq!(code(r), "model_not_found");
        assert_eq!(leaser.acquired(), 0);
    }
}
