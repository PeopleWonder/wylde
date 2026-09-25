//! The `/v1` gate: device-token auth plus the `/v1` rate limit, with
//! OpenAI-shaped rejections.
//!
//! An OpenAI client's `Authorization: Bearer <api key>` carries a Wylde
//! device token, verified by the same [`verify_bearer`] as
//! `require_device` (60 s token cache, then device-gate). Any verified
//! device may call `/v1`, including `read_only` ones: the gateway never
//! runs tools on these routes, so the tool tier doesn't apply. The
//! verified [`Device`](crate::auth::Device) is attached to the request for
//! handlers.
//!
//! The rate limit is a separate `/v1` bucket per device
//! (`WYLDE_RATE_LIMIT_OPENAI_PER_MIN`, default 600): autocomplete alone can
//! exceed the 60/min per-device tier, and must not use up the budget of
//! `chat/run_turn`.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::errors::OpenAiError;
use crate::auth::verify_bearer;
use crate::middleware::rate_limit::{seconds_until_window_reset, RateLimiter};

/// `from_fn_with_state` middleware guarding every `/v1` route.
pub async fn openai_gate(
    State(limiter): State<RateLimiter>,
    mut req: Request,
    next: Next,
) -> Response {
    let device = match verify_bearer(req.headers()).await {
        Ok(d) => d,
        Err(e) => return OpenAiError::from_auth(&e).into_response(),
    };
    if !limiter.allow(&format!("dev:{}", device.device_id)) {
        return OpenAiError::rate_limited(limiter.limit(), seconds_until_window_reset())
            .into_response();
    }
    req.extensions_mut().insert(device);
    next.run(req).await
}
