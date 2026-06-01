//! Secrets backend — pluggable file/Vault provider for sensitive config.
//!
//! Rust port of `Gateway/secrets/`. Two backends: [`file_backend`] (the
//! dev default — `.env` reader with OS-environ pass-through, and what
//! egress's `resolve_secret` falls through to) and [`vault_backend`] (a
//! HashiCorp Vault KV-v2 client built on a direct `reqwest` wrapper).
//!
//! Selection: [`get_secrets`] reads `WYLDE_GATEWAY_SECRETS_PROVIDER`
//! (default `"file"`) and caches the resulting provider per process. An
//! unknown value, or `"vault"` with a missing `VAULT_*` env contract,
//! falls through to the file backend with a one-line warning.

pub mod file_backend;
pub mod vault_backend;

use std::sync::{Arc, OnceLock};

use file_backend::FileBackend;
use vault_backend::VaultBackend;

/// Minimal secrets API — every backend implements this.
pub trait SecretsProvider: Send + Sync {
    /// Return the secret value for `key`, or `default` if missing.
    fn get(&self, key: &str, default: Option<&str>) -> Option<String>;

    /// True if this provider can serve requests right now.
    fn health_check(&self) -> bool;

    /// Like [`Self::get`], but returns an error on miss.
    fn require(&self, key: &str) -> Result<String, SecretsError> {
        self.get(key, None)
            .ok_or_else(|| SecretsError(format!("required secret missing: {key}")))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SecretsError(pub String);

/// Process-wide secrets provider, lazily constructed on first call.
pub fn get_secrets() -> Arc<dyn SecretsProvider> {
    static CELL: OnceLock<Arc<dyn SecretsProvider>> = OnceLock::new();
    CELL.get_or_init(build_provider).clone()
}

fn build_provider() -> Arc<dyn SecretsProvider> {
    let kind = std::env::var("WYLDE_GATEWAY_SECRETS_PROVIDER")
        .unwrap_or_else(|_| "file".to_owned())
        .to_ascii_lowercase();
    match kind.as_str() {
        "vault" => match VaultBackend::from_env() {
            Ok(backend) => Arc::new(backend),
            Err(e) => {
                tracing::warn!(
                    "vault secrets backend unavailable ({e}) — falling back to file backend"
                );
                Arc::new(FileBackend::default_paths())
            }
        },
        "file" | "" => Arc::new(FileBackend::default_paths()),
        other => {
            tracing::warn!(
                "unknown secrets provider {:?} — falling back to file backend",
                other
            );
            Arc::new(FileBackend::default_paths())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend;
    impl SecretsProvider for StubBackend {
        fn get(&self, key: &str, default: Option<&str>) -> Option<String> {
            if key == "FOO" {
                Some("bar".into())
            } else {
                default.map(str::to_owned)
            }
        }
        fn health_check(&self) -> bool {
            true
        }
    }

    #[test]
    fn require_succeeds_when_present() {
        let b = StubBackend;
        assert_eq!(b.require("FOO").unwrap(), "bar");
    }

    #[test]
    fn require_errors_when_missing() {
        let b = StubBackend;
        let err = b.require("MISSING").expect_err("must miss");
        assert!(err.0.contains("required secret missing"));
    }

    #[test]
    fn get_falls_back_to_default() {
        let b = StubBackend;
        assert_eq!(b.get("MISSING", Some("fallback")), Some("fallback".into()));
    }
}
