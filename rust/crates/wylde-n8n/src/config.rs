//! Env-driven configuration for wylde-n8n.
//!
//! Same shape as wylde-ollama's `config.rs` — read once at first access,
//! cached in a process-wide `OnceLock`. Mutating env after start does
//! not retroactively change behaviour (the Python client had identical
//! read-at-import semantics).
//!
//! Every credential var keeps the Python client's two-name fallback:
//! the `WYLDE_N8N_*` form wins, the bare `N8N_*` form is honoured for
//! installs that exported the n8n daemon's own variable names.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Connection + credential block, separated from [`Config`] so the HTTP
/// client (and its tests) can be built from explicit values without
/// touching process env.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Base URL of the external n8n daemon. `WYLDE_N8N_URL` / `N8N_URL`;
    /// default `http://127.0.0.1:5678`. Trailing slash stripped.
    pub url: String,
    /// API-key auth (preferred): sent as `X-N8N-API-KEY` on every
    /// request. Stateless. `WYLDE_N8N_API_KEY` / `N8N_API_KEY`.
    pub api_key: String,
    /// Login-session auth (fallback): `POST /rest/login` with
    /// email+password, cookie kept on the client's jar, ONE re-login
    /// retry on a mid-session 401. `WYLDE_N8N_EMAIL` + `WYLDE_N8N_PASSWORD`.
    pub email: String,
    pub password: String,
    /// Optional reverse-proxy basic-auth pair sent on every request,
    /// independent of the n8n-level auth mode.
    /// `WYLDE_N8N_BASIC_AUTH_USER` / `WYLDE_N8N_BASIC_AUTH_PASSWORD`.
    pub basic_user: String,
    pub basic_pass: String,
}

impl AuthConfig {
    /// True when at least one credential mode is configured. Calls made
    /// without auth return the structured `auth_not_configured` error
    /// envelope rather than a transport failure — this keeps the
    /// harness catalog buildable on a machine that hasn't wired n8n yet
    /// (the Python client's `_AUTH_READY` contract).
    pub fn auth_ready(&self) -> bool {
        !self.api_key.is_empty() || (!self.email.is_empty() && !self.password.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub auth: AuthConfig,
    pub wylde_root: PathBuf,
}

impl Config {
    fn load() -> Self {
        let url = env_with_alias("WYLDE_N8N_URL", "N8N_URL", "http://127.0.0.1:5678")
            .trim_end_matches('/')
            .to_owned();
        Self {
            auth: AuthConfig {
                url,
                api_key: env_with_alias("WYLDE_N8N_API_KEY", "N8N_API_KEY", ""),
                email: env_with_alias("WYLDE_N8N_EMAIL", "N8N_EMAIL", ""),
                password: env_with_alias("WYLDE_N8N_PASSWORD", "N8N_PASSWORD", ""),
                basic_user: env_with_alias("WYLDE_N8N_BASIC_AUTH_USER", "N8N_BASIC_AUTH_USER", ""),
                basic_pass: env_with_alias(
                    "WYLDE_N8N_BASIC_AUTH_PASSWORD",
                    "N8N_BASIC_AUTH_PASSWORD",
                    "",
                ),
            },
            wylde_root: std::env::var_os("WYLDE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }

    pub fn get() -> &'static Self {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(Self::load)
    }
}

/// `primary` env var wins; `alias` (the bare n8n-native name) is the
/// fallback; otherwise `default`. Mirrors the Python client's
/// `os.getenv("WYLDE_N8N_X", os.getenv("N8N_X", default))` chains.
fn env_with_alias(primary: &str, alias: &str, default: &str) -> String {
    std::env::var(primary)
        .or_else(|_| std::env::var(alias))
        .unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_ready_requires_a_complete_mode() {
        // Nothing configured → not ready.
        let mut a = AuthConfig::default();
        assert!(!a.auth_ready());
        // API key alone is sufficient (preferred mode).
        a.api_key = "k".into();
        assert!(a.auth_ready());
        // Email without password is NOT a usable session mode.
        let mut b = AuthConfig {
            email: "a@b.c".into(),
            ..Default::default()
        };
        assert!(!b.auth_ready());
        b.password = "pw".into();
        assert!(b.auth_ready());
        // Basic-auth alone never satisfies auth (it's a proxy layer,
        // not an n8n credential) — matches the Python `_AUTH_READY`.
        let c = AuthConfig {
            basic_user: "u".into(),
            basic_pass: "p".into(),
            ..Default::default()
        };
        assert!(!c.auth_ready());
    }

    #[test]
    fn default_url_is_local_5678_without_trailing_slash() {
        // Only assert when the host env hasn't overridden the URL —
        // config tests must not mutate process env (parallel runner).
        if std::env::var("WYLDE_N8N_URL").is_err() && std::env::var("N8N_URL").is_err() {
            let cfg = Config::load();
            assert_eq!(cfg.auth.url, "http://127.0.0.1:5678");
        }
        let cfg = Config::load();
        assert!(!cfg.auth.url.ends_with('/'));
    }
}
