//! Pipe service names for inference, with env overrides.
//!
//! Every gateway path that runs a model goes through `wylde-ollama` (which
//! holds the VRAM lease) rather than straight to Ollama. The pipe names are
//! overridable so an integration test can serve mocks under unique names
//! instead of binding the production pipes (a test on a production pipe
//! name fails whenever the real product is running).

/// Default `wylde-ollama` pipe service.
pub const DEFAULT_OLLAMA_SERVICE: &str = "wylde-ollama";
/// Default harness pipe service (model registry).
pub const DEFAULT_HARNESS_SERVICE: &str = "wylde-harness";

/// Env override for the `wylde-ollama` pipe service name.
pub const OLLAMA_SERVICE_ENV: &str = "WYLDE_GATEWAY_OLLAMA_SERVICE";
/// Env override for the harness pipe service name.
pub const HARNESS_SERVICE_ENV: &str = "WYLDE_GATEWAY_HARNESS_SERVICE";

fn from_env(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// The `wylde-ollama` pipe service to call.
pub fn ollama_service() -> String {
    from_env(OLLAMA_SERVICE_ENV, DEFAULT_OLLAMA_SERVICE)
}

/// The harness pipe service to call.
pub fn harness_service() -> String {
    from_env(HARNESS_SERVICE_ENV, DEFAULT_HARNESS_SERVICE)
}

/// Default `wylde-workspaces` pipe service. The `workspaces.*` verbs live
/// there; the harness retired them in Slice 0d and answers `no_action`.
pub const DEFAULT_WORKSPACES_SERVICE: &str = "wylde-workspaces";
/// Env override for the `wylde-workspaces` pipe service name.
pub const WORKSPACES_SERVICE_ENV: &str = "WYLDE_GATEWAY_WORKSPACES_SERVICE";

/// The `wylde-workspaces` pipe service to call.
pub fn workspaces_service() -> String {
    from_env(WORKSPACES_SERVICE_ENV, DEFAULT_WORKSPACES_SERVICE)
}
