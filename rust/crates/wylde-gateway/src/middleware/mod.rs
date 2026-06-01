//! Gateway middleware — composable request-path concerns.
//!
//! Rust port of `Gateway/middleware/` plus `Gateway/events.py`:
//!
//! * [`trace`]      — request-id stamping (`X-Wylde-Request-ID`).
//! * [`audit_log`]  — JSONL writer for ingress + egress activity.
//! * [`rate_limit`] — fixed-window per-minute request cap.
//! * [`events`]     — device-state event forwarding (`X-Wylde-Events`).
//!
//! `trace` and `audit_log` are global [`tower`] layers; `rate_limit` is a
//! global `axum::middleware::from_fn_with_state` layer. All three are
//! composed by [`crate::app::build_router`] in the same order the Python
//! `create_app` wired them into Starlette:
//! `CORS → Trace → AuditLog → RateLimit → routes`. `events` and the
//! per-device variant of `rate_limit` ([`per_device_rate_limit`]) are
//! per-route layers mounted inner to `require_device` on the device-tier
//! routes.

pub mod audit_log;
pub mod events;
pub mod rate_limit;
pub mod trace;

pub use self::audit_log::{
    emit_egress, get_audit_logger, reset_audit_writers, AuditLogLayer, AuditLogMiddleware,
};
pub use self::events::{forward_device_events, EVENTS_HEADER};
pub use self::rate_limit::{device_limiter, per_device_rate_limit};
pub use self::trace::{
    get_request_id, RequestId, RequestTraceLayer, RequestTraceMiddleware, REQUEST_ID_HEADER,
};
