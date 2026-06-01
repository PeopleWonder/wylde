//! Sibling-service IPC wrappers.
//!
//! Rust port of `Gateway/services/`. Wave 1 ships the device-gate
//! wrapper (used by every authenticated request on the Python side and
//! prepared here so the auth layer can plug straight in during wave
//! 2+); egress / extensions / tool-registry wrappers are queued — see
//! `docs/r3_gateway_deferred.md`.

pub mod device_gate;
