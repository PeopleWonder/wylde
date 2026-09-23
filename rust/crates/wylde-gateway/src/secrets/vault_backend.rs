//! HashiCorp Vault KV-v2 secrets backend.
//!
//! Rust port of `Gateway/secrets/vault_backend.py`. Every Gateway secret
//! (egress upstream API keys, webhook signing secrets, …) lives as a
//! field of one KV-v2 secret at `{mount}/data/wylde/gateway`; [`get`]
//! looks the requested key up among those fields.
//!
//! [`get`]: VaultBackend::get
//!
//! Env-var contract (read by [`VaultBackend::from_env`]):
//!
//! * `VAULT_ADDR` — Vault server URL, e.g. `https://vault.internal:8200`.
//!   A bare host gets an implicit `https://` scheme.
//! * `VAULT_TOKEN` — Vault token. Auth is **token-based only for v1**;
//!   AppRole / JWT / Kubernetes auth methods are deferred.
//! * `VAULT_NAMESPACE` — Vault Enterprise namespace (optional). Sent as
//!   the `X-Vault-Namespace` request header when set.
//! * `WYLDE_VAULT_KV_MOUNT` — KV-v2 mount path (default `secret`).
//!
//! The read result is cached in-memory for 60 seconds — a cache hit
//! returns without touching the wire, a miss re-reads from Vault.
//! Graceful degradation: a connection error, 5xx, or 401/403 logs a
//! warning and falls through to the composed [`FileBackend`], so a Vault
//! outage degrades to the dev-default `.env` / OS-environ path rather
//! than failing hard.
//!
//! `SecretsProvider::get` is a sync trait method, so the KV-v2 read uses
//! `reqwest`'s blocking client (its own runtime on a background thread)
//! — safe to call from within the axum async context.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;

use super::file_backend::FileBackend;
use super::{SecretsError, SecretsProvider};

const DEFAULT_SECRET_PATH: &str = "wylde/gateway";
const DEFAULT_KV_MOUNT: &str = "secret";
const CACHE_TTL: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct VaultBackend {
    address: String,
    token: String,
    namespace: Option<String>,
    kv_mount: String,
    secret_path: String,
    cache_ttl: Duration,
    fallback: FileBackend,
    client: reqwest::blocking::Client,
    cache: Mutex<Option<(HashMap<String, String>, Instant)>>,
}

impl VaultBackend {
    pub fn new(
        address: impl Into<String>,
        token: impl Into<String>,
        namespace: Option<String>,
        kv_mount: impl Into<String>,
        secret_path: impl Into<String>,
        cache_ttl: Duration,
        fallback: FileBackend,
    ) -> Self {
        let mut address = address.into();
        if !address.contains("://") {
            address = format!("https://{address}");
        }
        let mount = kv_mount.into();
        let mount = mount.trim_matches('/');
        let client = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            address: address.trim_end_matches('/').to_owned(),
            token: token.into(),
            namespace: namespace.filter(|s| !s.is_empty()),
            kv_mount: if mount.is_empty() {
                DEFAULT_KV_MOUNT.to_owned()
            } else {
                mount.to_owned()
            },
            secret_path: secret_path.into().trim_matches('/').to_owned(),
            cache_ttl,
            fallback,
            client,
            cache: Mutex::new(None),
        }
    }

    /// Construct from the `VAULT_*` env-var contract. Mirrors Python's
    /// `VaultBackend.from_env()`.
    pub fn from_env() -> Result<Self, SecretsError> {
        let address = std::env::var("VAULT_ADDR")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SecretsError("VAULT_ADDR not set".to_owned()))?;
        let token = std::env::var("VAULT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SecretsError("VAULT_TOKEN not set".to_owned()))?;
        let namespace = std::env::var("VAULT_NAMESPACE")
            .ok()
            .filter(|s| !s.is_empty());
        let kv_mount = std::env::var("WYLDE_VAULT_KV_MOUNT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_KV_MOUNT.to_owned());
        Ok(Self::new(
            address,
            token,
            namespace,
            kv_mount,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            FileBackend::default_paths(),
        ))
    }

    /// Read the KV-v2 secret. Returns `None` on any failure so the
    /// caller falls through to the file backend.
    fn fetch(&self) -> Option<HashMap<String, String>> {
        let url = format!(
            "{}/v1/{}/data/{}",
            self.address, self.kv_mount, self.secret_path
        );
        let mut req = self.client.get(&url).header("X-Vault-Token", &self.token);
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("vault: request failed ({e}) — falling back to file backend");
                return None;
            }
        };
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            tracing::warn!(
                "vault: auth rejected (HTTP {}) — falling back to file backend",
                status.as_u16()
            );
            return None;
        }
        if status == StatusCode::NOT_FOUND {
            tracing::warn!(
                "vault: secret path not found (HTTP 404) — falling back to file backend"
            );
            return None;
        }
        if status.is_server_error() {
            tracing::warn!(
                "vault: server error (HTTP {}) — falling back to file backend",
                status.as_u16()
            );
            return None;
        }
        if !status.is_success() {
            tracing::warn!(
                "vault: unexpected status (HTTP {}) — falling back to file backend",
                status.as_u16()
            );
            return None;
        }
        let payload: Value = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "vault: malformed KV-v2 response ({e}) — falling back to file backend"
                );
                return None;
            }
        };
        let data = payload
            .get("data")
            .and_then(|d| d.get("data"))
            .and_then(Value::as_object);
        let Some(obj) = data else {
            tracing::warn!("vault: KV-v2 data.data missing — falling back to file backend");
            return None;
        };
        let mut out = HashMap::with_capacity(obj.len());
        for (k, v) in obj {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(k.clone(), val);
        }
        Some(out)
    }

    /// Return the cached secret map, re-fetching on a cold or stale
    /// cache. `None` means the fetch failed (graceful-degradation path).
    fn read_cached(&self) -> Option<HashMap<String, String>> {
        {
            let guard = self.cache.lock().expect("vault cache poisoned");
            if let Some((map, at)) = guard.as_ref() {
                if at.elapsed() < self.cache_ttl {
                    return Some(map.clone());
                }
            }
        }
        let fetched = self.fetch()?;
        let mut guard = self.cache.lock().expect("vault cache poisoned");
        *guard = Some((fetched.clone(), Instant::now()));
        Some(fetched)
    }
}

impl SecretsProvider for VaultBackend {
    fn get(&self, key: &str, default: Option<&str>) -> Option<String> {
        if let Some(map) = self.read_cached() {
            if let Some(value) = map.get(key) {
                return Some(value.clone());
            }
        }
        self.fallback.get(key, default)
    }

    fn health_check(&self) -> bool {
        self.read_cached().is_some() || self.fallback.health_check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::extract::{Request, State};
    use axum::response::IntoResponse;
    use axum::Router;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const KV_OK: &str = r#"{"data":{"data":{"api-key":"s3cr3t"}}}"#;

    #[derive(Clone)]
    struct MockState {
        status: u16,
        body: String,
        hits: Arc<AtomicUsize>,
        last_path: Arc<Mutex<String>>,
        last_namespace: Arc<Mutex<Option<String>>>,
    }

    struct MockVault {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
        last_path: Arc<Mutex<String>>,
        last_namespace: Arc<Mutex<Option<String>>>,
    }

    impl MockVault {
        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
        fn last_path(&self) -> String {
            self.last_path.lock().expect("lock").clone()
        }
        fn last_namespace(&self) -> Option<String> {
            self.last_namespace.lock().expect("lock").clone()
        }
    }

    async fn mock_handler(State(st): State<MockState>, req: Request) -> impl IntoResponse {
        st.hits.fetch_add(1, Ordering::SeqCst);
        *st.last_path.lock().expect("lock") = req.uri().path().to_owned();
        *st.last_namespace.lock().expect("lock") = req
            .headers()
            .get("X-Vault-Namespace")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            StatusCode::from_u16(st.status).expect("valid status"),
            st.body.clone(),
        )
    }

    /// Spawn an in-process mock Vault on its own thread + runtime, so a
    /// plain `#[test]` (sync `VaultBackend::get`) can talk to it without
    /// deadlocking on a shared runtime.
    fn spawn_mock(status: u16, body: &str) -> MockVault {
        let hits = Arc::new(AtomicUsize::new(0));
        let last_path = Arc::new(Mutex::new(String::new()));
        let last_namespace = Arc::new(Mutex::new(None));
        let state = MockState {
            status,
            body: body.to_owned(),
            hits: Arc::clone(&hits),
            last_path: Arc::clone(&last_path),
            last_namespace: Arc::clone(&last_namespace),
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock vault");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mock runtime");
            rt.block_on(async move {
                let router = Router::new().fallback(mock_handler).with_state(state);
                let tl = tokio::net::TcpListener::from_std(listener).expect("from_std");
                axum::serve(tl, router).await.expect("serve");
            });
        });
        std::thread::sleep(Duration::from_millis(50));
        MockVault {
            addr,
            hits,
            last_path,
            last_namespace,
        }
    }

    /// A file backend that resolves nothing — no file, no OS-environ —
    /// so a fall-through surfaces as `None`.
    fn empty_fallback() -> FileBackend {
        FileBackend::new(PathBuf::from("/nonexistent-wylde-vault-test/.env"), false)
    }

    fn backend(mock: &MockVault) -> VaultBackend {
        VaultBackend::new(
            mock.url(),
            "test-token",
            None,
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            empty_fallback(),
        )
    }

    #[test]
    fn successful_read_returns_value() {
        let mock = spawn_mock(200, KV_OK);
        assert_eq!(
            backend(&mock).get("api-key", None),
            Some("s3cr3t".to_owned())
        );
    }

    #[test]
    fn missing_path_404_returns_none() {
        let mock = spawn_mock(404, r#"{"errors":[]}"#);
        assert_eq!(backend(&mock).get("api-key", None), None);
    }

    #[test]
    fn auth_failure_401_returns_none() {
        let mock = spawn_mock(401, r#"{"errors":["permission denied"]}"#);
        assert_eq!(backend(&mock).get("api-key", None), None);
    }

    #[test]
    fn auth_failure_403_returns_none() {
        let mock = spawn_mock(403, r#"{"errors":["permission denied"]}"#);
        assert_eq!(backend(&mock).get("api-key", None), None);
    }

    #[test]
    fn server_error_503_returns_none() {
        let mock = spawn_mock(503, r#"{"errors":["sealed"]}"#);
        assert_eq!(backend(&mock).get("api-key", None), None);
    }

    #[test]
    fn connection_refused_returns_none() {
        // Point at loopback port 1: nothing listens there, so the request
        // gets connection-refused immediately. Using a fixed reserved port
        // (rather than binding `:0` and dropping the listener) keeps this
        // deterministic under parallel test threads — `spawn_mock` only ever
        // binds ephemeral ports via `127.0.0.1:0`, so no concurrent mock can
        // ever occupy port 1 and turn the refusal into an HTTP 200.
        let b = VaultBackend::new(
            "http://127.0.0.1:1".to_owned(),
            "test-token",
            None,
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            empty_fallback(),
        );
        assert_eq!(b.get("api-key", None), None);
    }

    #[test]
    fn cache_hit_does_not_hit_wire() {
        let mock = spawn_mock(200, KV_OK);
        let b = backend(&mock);
        assert_eq!(b.get("api-key", None), Some("s3cr3t".to_owned()));
        assert_eq!(b.get("api-key", None), Some("s3cr3t".to_owned()));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn cache_refetches_after_ttl() {
        let mock = spawn_mock(200, KV_OK);
        let b = VaultBackend::new(
            mock.url(),
            "test-token",
            None,
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            Duration::from_millis(40),
            empty_fallback(),
        );
        assert_eq!(b.get("api-key", None), Some("s3cr3t".to_owned()));
        assert_eq!(mock.hits(), 1);
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(b.get("api-key", None), Some("s3cr3t".to_owned()));
        assert_eq!(mock.hits(), 2);
    }

    #[test]
    fn namespace_header_included_when_set() {
        let mock = spawn_mock(200, KV_OK);
        let b = VaultBackend::new(
            mock.url(),
            "test-token",
            Some("team-wylde".to_owned()),
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            empty_fallback(),
        );
        b.get("api-key", None);
        assert_eq!(mock.last_namespace(), Some("team-wylde".to_owned()));
    }

    #[test]
    fn namespace_header_absent_when_unset() {
        let mock = spawn_mock(200, KV_OK);
        backend(&mock).get("api-key", None);
        assert_eq!(mock.last_namespace(), None);
    }

    #[test]
    fn custom_mount_path_in_url() {
        let mock = spawn_mock(200, KV_OK);
        let b = VaultBackend::new(
            mock.url(),
            "test-token",
            None,
            "kv2-prod",
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            empty_fallback(),
        );
        b.get("api-key", None);
        assert_eq!(mock.last_path(), "/v1/kv2-prod/data/wylde/gateway");
    }

    #[test]
    fn falls_through_to_file_value() {
        // Vault is down (5xx) — the composed file backend supplies the value.
        let mock = spawn_mock(503, r#"{"errors":["sealed"]}"#);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let env = tmp.path().join(".env");
        std::fs::write(&env, "API_TOKEN=from-file\n").expect("write env");
        let b = VaultBackend::new(
            mock.url(),
            "test-token",
            None,
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            FileBackend::new(env, false),
        );
        assert_eq!(b.get("API_TOKEN", None), Some("from-file".to_owned()));
    }

    #[test]
    fn new_prepends_https_when_scheme_missing() {
        let b = VaultBackend::new(
            "vault.test:8200",
            "t",
            None,
            DEFAULT_KV_MOUNT,
            DEFAULT_SECRET_PATH,
            CACHE_TTL,
            empty_fallback(),
        );
        assert_eq!(b.address, "https://vault.test:8200");
    }

    #[test]
    fn from_env_requires_addr_and_token() {
        let _g = ENV_LOCK.lock().expect("env lock");
        std::env::remove_var("VAULT_ADDR");
        std::env::remove_var("VAULT_TOKEN");
        assert!(VaultBackend::from_env().is_err());

        std::env::set_var("VAULT_ADDR", "vault.test:8200");
        std::env::set_var("VAULT_TOKEN", "tok");
        let b = VaultBackend::from_env().expect("from_env ok");
        assert_eq!(b.address, "https://vault.test:8200");

        std::env::remove_var("VAULT_ADDR");
        std::env::remove_var("VAULT_TOKEN");
    }
}
