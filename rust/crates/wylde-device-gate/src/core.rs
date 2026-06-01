//! Public service surface — pairing, tokens, tier management.
//!
//! Rust port of `device_gate/core.py`. This is the in-process API the pipe
//! layer wraps; every state mutation goes through here and the pipe handlers
//! are thin envelope-translators over these methods.
//!
//! Responsibilities, mirroring Python:
//!   * Pairing-mode lifecycle. One-shot (auto-OFF on success / cancel /
//!     expiry). One active code at a time.
//!   * Device record CRUD via [`DeviceStore`].
//!   * Token generation / verification / rotation / revocation.
//!   * Pending events queue per device, so the Gateway can forward
//!     `token_rotated` / `revoked` to the mobile's active connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::auth::verify_credentials;
use crate::store::{is_valid_tier, Device, DeviceStore, StoreError};

// Re-export tier constants so callers can do `device_gate::core::TIER_READ_ONLY`
// without reaching into `store`. Crate root re-exports these in turn.
pub use crate::store::{ALL_TIERS, TIER_DESTRUCTIVE, TIER_READ_ONLY, TIER_TOOL_USE};

pub const PAIRING_CODE_TTL_SECONDS: f64 = 5.0 * 60.0;
pub const PAIRING_CODE_LENGTH: usize = 6;
const PAIRING_CODE_ALPHABET: &[u8] = b"0123456789";

const DEFAULT_DATA_DIR_ENV: &str = "DEVICE_GATE_DATA_DIR";
const HTPASSWD_PATH_ENV: &str = "DEVICE_GATE_HTPASSWD";

/// Live wall-clock seconds. Matches Python's `time.time()` for wire parity.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os(DEFAULT_DATA_DIR_ENV) {
        return PathBuf::from(p);
    }
    // Python's fallback is `<service folder>/data`; here we anchor on
    // `WYLDE_ROOT/device_gate/data` so the Rust binary stays consistent
    // when launched outside the vault root. Tests always override via env.
    let root = std::env::var_os("WYLDE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("device_gate").join("data")
}

fn devices_path() -> PathBuf {
    data_dir().join("devices.json")
}

fn htpasswd_path() -> PathBuf {
    std::env::var_os(HTPASSWD_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("htpasswd"))
}

// ── Errors ────────────────────────────────────────────────────────────

/// Structured error surfaced through the pipe envelope. Mirrors Python's
/// `DeviceGateError(code, message)` shape so the wire response is identical.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct DeviceGateError {
    pub code: String,
    pub message: String,
}

impl DeviceGateError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<StoreError> for DeviceGateError {
    fn from(e: StoreError) -> Self {
        Self::new("store_error", e.to_string())
    }
}

// ── Pairing state ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PairingState {
    code: String,
    expires_at: f64,
    started_at: f64,
}

impl PairingState {
    fn active(&self, now: f64) -> bool {
        !self.code.is_empty() && now < self.expires_at
    }

    fn reset(&mut self) {
        self.code.clear();
        self.expires_at = 0.0;
        self.started_at = 0.0;
    }

    fn to_status(&self, now: f64) -> Value {
        if self.active(now) {
            json!({
                "pairing_active": true,
                "code": self.code,
                "expires_at": self.expires_at,
            })
        } else {
            json!({ "pairing_active": false })
        }
    }
}

// ── Service ───────────────────────────────────────────────────────────

/// Trait-object clock so tests can inject a fixed clock. Production uses
/// [`now_secs`] (wall-clock seconds since epoch).
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// Holds the device store + pairing state + pending-event queues.
///
/// The pipe layer operates on the process-wide singleton via
/// [`get_service`]; tests construct an instance with a tmpdir-backed store
/// and an injected clock to keep state isolated.
pub struct DeviceGateService {
    store: DeviceStore,
    htpasswd_path: PathBuf,
    pairing: Mutex<PairingState>,
    pending_events: Mutex<HashMap<String, Vec<Value>>>,
    clock: Clock,
}

impl DeviceGateService {
    /// Default construction: read-from-env paths, wall-clock time.
    pub fn new_default() -> Self {
        Self::builder()
            .store(DeviceStore::new(devices_path()))
            .htpasswd_path(htpasswd_path())
            .build()
    }

    pub fn builder() -> ServiceBuilder {
        ServiceBuilder::default()
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    // ── Pairing flow ──────────────────────────────────────────────────

    pub fn start_pairing(&self) -> Value {
        let mut p = self.pairing.lock().expect("pairing lock poisoned");
        let now = self.now();
        p.code = mint_code();
        p.started_at = now;
        p.expires_at = now + PAIRING_CODE_TTL_SECONDS;
        tracing::info!(
            "device_gate: pairing started (expires in {}s)",
            PAIRING_CODE_TTL_SECONDS as i64
        );
        json!({
            "code": p.code,
            "expires_at": p.expires_at,
        })
    }

    pub fn cancel_pairing(&self) -> Value {
        let mut p = self.pairing.lock().expect("pairing lock poisoned");
        let was_active = p.active(self.now());
        p.reset();
        if was_active {
            tracing::info!("device_gate: pairing cancelled");
        }
        json!({ "ok": true, "cancelled": was_active })
    }

    pub fn get_pairing_status(&self) -> Value {
        let mut p = self.pairing.lock().expect("pairing lock poisoned");
        let now = self.now();
        // Lazy-expire: collapse stale state so subsequent calls see a clean
        // pairing window.
        if !p.code.is_empty() && !p.active(now) {
            p.reset();
        }
        p.to_status(now)
    }

    pub fn complete_pairing(
        &self,
        code: &str,
        username: &str,
        password: &str,
        device_metadata: Option<&HashMap<String, Value>>,
    ) -> Result<Value, DeviceGateError> {
        let empty_meta: HashMap<String, Value> = HashMap::new();
        let metadata = device_metadata.unwrap_or(&empty_meta);
        let mut p = self.pairing.lock().expect("pairing lock poisoned");
        let now = self.now();

        if !p.active(now) {
            // Burn the credential check anyway so timing is constant —
            // mirrors Python.
            let _ = verify_credentials(&self.htpasswd_path, username, password);
            return Err(DeviceGateError::new(
                "pairing_inactive",
                "no pairing window is open — start one from the desktop GUI first",
            ));
        }

        // Constant-time code compare so timing doesn't reveal correctness.
        let code_ok: bool = code.as_bytes().ct_eq(p.code.as_bytes()).into();
        let creds_ok = verify_credentials(&self.htpasswd_path, username, password);

        if !code_ok {
            return Err(DeviceGateError::new(
                "code_mismatch",
                "pairing code is wrong or expired",
            ));
        }
        if !creds_ok {
            return Err(DeviceGateError::new(
                "credential_mismatch",
                "username or password is incorrect",
            ));
        }

        let device_id = mint_device_id();
        let token = mint_token();
        let name = device_name_from_metadata(metadata)
            .unwrap_or_else(|| device_id.chars().take(8).collect());
        let device = Device {
            device_id: device_id.clone(),
            name,
            token: token.clone(),
            tier: TIER_READ_ONLY.to_string(),
            paired_at: now,
            last_seen: now,
            metadata: metadata.clone(),
        };
        self.store.add(device.clone())?;
        p.reset();
        tracing::info!(
            "device_gate: paired {} ({}) tier={}",
            device_id,
            device.name,
            device.tier
        );
        Ok(json!({
            "device_id": device_id,
            "token": token,
            "tier": device.tier,
        }))
    }

    // ── Token verification ────────────────────────────────────────────

    pub fn verify(&self, token: &str) -> Result<Value, DeviceGateError> {
        if token.is_empty() {
            return Err(DeviceGateError::new("invalid_token", "token is required"));
        }
        let device = self.store.find_by_token(token).ok_or_else(|| {
            DeviceGateError::new("invalid_token", "token does not match any device")
        })?;
        self.store.touch(&device.device_id, self.now());
        Ok(json!({
            "device_id": device.device_id,
            "tier": device.tier,
        }))
    }

    // ── Tier management ───────────────────────────────────────────────

    pub fn set_tier(&self, device_id: &str, tier: &str) -> Result<Value, DeviceGateError> {
        if !is_valid_tier(tier) {
            return Err(DeviceGateError::new(
                "bad_request",
                format!(
                    "tier must be one of [{:?}, {:?}, {:?}]",
                    ALL_TIERS[0], ALL_TIERS[1], ALL_TIERS[2]
                ),
            ));
        }
        let tier_owned = tier.to_string();
        let updated = self
            .store
            .update(device_id, |d| d.tier = tier_owned.clone());
        if updated.is_none() {
            return Err(DeviceGateError::new(
                "not_found",
                format!("device {device_id:?} not found"),
            ));
        }
        tracing::info!("device_gate: {} tier → {}", device_id, tier);
        self.enqueue_event(device_id, "tier_changed", json!({ "tier": tier }));
        Ok(json!({
            "device_id": device_id,
            "tier": tier,
        }))
    }

    // ── Token rotation ────────────────────────────────────────────────

    pub fn rotate_token(&self, device_id: &str) -> Result<Value, DeviceGateError> {
        if self.store.get(device_id).is_none() {
            return Err(DeviceGateError::new(
                "not_found",
                format!("device {device_id:?} not found"),
            ));
        }
        let new_token = mint_token();
        let new_token_clone = new_token.clone();
        let updated = self
            .store
            .update(device_id, |d| d.token = new_token_clone.clone());
        if updated.is_none() {
            return Err(DeviceGateError::new(
                "not_found",
                format!("device {device_id:?} not found"),
            ));
        }
        tracing::info!("device_gate: rotated token for {}", device_id);
        self.enqueue_event(
            device_id,
            "token_rotated",
            json!({ "new_token": new_token }),
        );
        Ok(json!({
            "device_id": device_id,
            "new_token": new_token,
        }))
    }

    // ── Revocation ────────────────────────────────────────────────────

    pub fn revoke(&self, device_id: &str) -> Result<Value, DeviceGateError> {
        if self.store.get(device_id).is_none() {
            return Err(DeviceGateError::new(
                "not_found",
                format!("device {device_id:?} not found"),
            ));
        }
        if !self.store.remove(device_id) {
            return Err(DeviceGateError::new(
                "not_found",
                format!("device {device_id:?} not found"),
            ));
        }
        // Queue revoked event AFTER the remove — keyed by device_id, the
        // Gateway can still pick it up on the next consume call.
        self.enqueue_event(device_id, "revoked", json!({}));
        tracing::info!("device_gate: revoked {}", device_id);
        Ok(json!({ "device_id": device_id }))
    }

    // ── Event queue ───────────────────────────────────────────────────

    fn enqueue_event(&self, device_id: &str, kind: &str, data: Value) {
        let mut ev = json!({
            "type": kind,
            "device_id": device_id,
            "at": self.now(),
        });
        if let Value::Object(ref data_map) = data {
            if let Value::Object(ref mut ev_map) = ev {
                for (k, v) in data_map {
                    ev_map.insert(k.clone(), v.clone());
                }
            }
        }
        let mut q = self.pending_events.lock().expect("event queue poisoned");
        q.entry(device_id.to_string()).or_default().push(ev);
    }

    pub fn consume_pending_events(&self, device_id: &str) -> Vec<Value> {
        let mut q = self.pending_events.lock().expect("event queue poisoned");
        q.remove(device_id).unwrap_or_default()
    }

    pub fn has_pending_events(&self, device_id: &str) -> bool {
        let q = self.pending_events.lock().expect("event queue poisoned");
        q.get(device_id).map(|v| !v.is_empty()).unwrap_or(false)
    }

    // ── Listing ──────────────────────────────────────────────────────

    /// GUI device list. `is_active` is true iff `last_seen` is within
    /// `active_threshold_s` of the current clock.
    pub fn list_devices(&self, active_threshold_s: f64) -> Vec<Value> {
        let now = self.now();
        self.store
            .list()
            .into_iter()
            .map(|d| {
                let mut entry = d.to_public_value();
                let is_active = d.last_seen > 0.0 && (now - d.last_seen) <= active_threshold_s;
                if let Value::Object(ref mut m) = entry {
                    m.insert("is_active".into(), Value::Bool(is_active));
                }
                entry
            })
            .collect()
    }
}

// ── Builder ──────────────────────────────────────────────────────────

/// Constructor with the same opt-in customization Python's `__init__` exposes
/// (store, htpasswd_path, clock).
#[derive(Default)]
pub struct ServiceBuilder {
    store: Option<DeviceStore>,
    htpasswd_path: Option<PathBuf>,
    clock: Option<Clock>,
}

impl ServiceBuilder {
    pub fn store(mut self, store: DeviceStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn htpasswd_path(mut self, path: impl AsRef<Path>) -> Self {
        self.htpasswd_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    pub fn build(self) -> DeviceGateService {
        DeviceGateService {
            store: self
                .store
                .unwrap_or_else(|| DeviceStore::new(devices_path())),
            htpasswd_path: self.htpasswd_path.unwrap_or_else(htpasswd_path),
            pairing: Mutex::new(PairingState::default()),
            pending_events: Mutex::new(HashMap::new()),
            clock: self.clock.unwrap_or_else(|| Box::new(now_secs)),
        }
    }
}

// ── Module-level singleton ────────────────────────────────────────────

fn service_slot() -> &'static RwLock<Option<DeviceGateService>> {
    static SLOT: OnceLock<RwLock<Option<DeviceGateService>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Get-or-init the process-wide service. First call constructs from env.
///
/// Returns an [`std::sync::RwLockReadGuard`] tied to the storage slot so the
/// service can't be replaced mid-call.
pub fn with_service<R>(f: impl FnOnce(&DeviceGateService) -> R) -> R {
    // Fast path: read lock, init already done.
    if let Some(svc) = service_slot()
        .read()
        .expect("service lock poisoned")
        .as_ref()
    {
        return f(svc);
    }
    // Slow path: upgrade to write, double-check, init.
    {
        let mut w = service_slot().write().expect("service lock poisoned");
        if w.is_none() {
            *w = Some(DeviceGateService::new_default());
        }
    }
    let r = service_slot().read().expect("service lock poisoned");
    f(r.as_ref().expect("service installed"))
}

/// Test seam — install a service explicitly. Pair with [`reset_service`].
pub fn install_service(svc: DeviceGateService) {
    *service_slot().write().expect("service lock poisoned") = Some(svc);
}

/// Test seam — clear the singleton. Subsequent [`with_service`] re-initializes.
pub fn reset_service() {
    *service_slot().write().expect("service lock poisoned") = None;
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Six-digit pairing code. Uses `rand::thread_rng` (seeded from the OS CSPRNG
/// on first use) so codes are cryptographically unguessable within the
/// 5-minute window — matches Python's `secrets.choice` semantics.
fn mint_code() -> String {
    let mut rng = rand::thread_rng();
    (0..PAIRING_CODE_LENGTH)
        .map(|_| {
            let idx = rng.gen_range(0..PAIRING_CODE_ALPHABET.len());
            PAIRING_CODE_ALPHABET[idx] as char
        })
        .collect()
}

/// UUID4 hex — opaque, 32 chars. Mirrors Python's `uuid.uuid4().hex`.
fn mint_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Device id — short timestamped + random suffix. Mirrors Python's
/// `f"dev_{int(time.time())}_{secrets.token_hex(3)}"`.
fn mint_device_id() -> String {
    let ts = now_secs() as i64;
    let mut rng = rand::thread_rng();
    let suffix: [u8; 3] = rng.gen();
    format!(
        "dev_{ts}_{}",
        suffix
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn device_name_from_metadata(meta: &HashMap<String, Value>) -> Option<String> {
    for key in ["name", "device_name", "hostname"] {
        if let Some(Value::String(s)) = meta.get(key) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(64).collect());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::{NamedTempFile, TempDir};

    fn write_apr1_htpasswd(user: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        // apr1 hash of "letmein" — see auth.rs fixture.
        let line = format!("{user}:$apr1$abcdefgh$2/f5Gp5itvzIJXRHg/wa/1\n");
        f.write_all(line.as_bytes()).expect("write");
        f
    }

    fn fresh_service() -> (TempDir, NamedTempFile, DeviceGateService, Arc<TestClock>) {
        let tmp = TempDir::new().expect("tempdir");
        let htpasswd = write_apr1_htpasswd("wylde");
        let store = DeviceStore::new(tmp.path().join("devices.json"));
        let clock = Arc::new(TestClock::new(1_000_000.0));
        let clock_clone = clock.clone();
        let svc = DeviceGateService::builder()
            .store(store)
            .htpasswd_path(htpasswd.path())
            .clock(Box::new(move || clock_clone.now()))
            .build();
        (tmp, htpasswd, svc, clock)
    }

    /// Manual clock — thread-safe so closures held by the service can read it.
    pub struct TestClock {
        t: Mutex<Cell<f64>>,
    }

    // Cell isn't Sync; wrap in Mutex so the service's clock closure can be
    // `Send + Sync`.
    impl TestClock {
        fn new(start: f64) -> Self {
            Self {
                t: Mutex::new(Cell::new(start)),
            }
        }
        fn now(&self) -> f64 {
            self.t.lock().unwrap().get()
        }
        fn advance(&self, seconds: f64) {
            let cell = self.t.lock().unwrap();
            cell.set(cell.get() + seconds);
        }
    }

    #[test]
    fn pairing_happy_path() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let start = svc.start_pairing();
        let code = start["code"].as_str().unwrap().to_string();
        assert!(start["expires_at"].as_f64().is_some());

        let result = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .expect("complete_pairing ok");
        assert!(result["device_id"].is_string());
        assert!(result["token"].is_string());
        assert_eq!(result["tier"], TIER_READ_ONLY);

        // Pairing-mode auto-OFF.
        assert_eq!(svc.get_pairing_status()["pairing_active"], false);

        let devices = svc.list_devices(60.0);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["tier"], TIER_READ_ONLY);
    }

    #[test]
    fn pairing_wrong_code() {
        let (_tmp, _h, svc, _c) = fresh_service();
        svc.start_pairing();
        let err = svc
            .complete_pairing("000000", "wylde", "letmein", None)
            .unwrap_err();
        assert_eq!(err.code, "code_mismatch");
    }

    #[test]
    fn pairing_wrong_credentials() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let err = svc
            .complete_pairing(&code, "wylde", "WRONG", None)
            .unwrap_err();
        assert_eq!(err.code, "credential_mismatch");
    }

    #[test]
    fn pairing_expired_code() {
        let (_tmp, _h, svc, clock) = fresh_service();
        let started = svc.start_pairing();
        clock.advance(PAIRING_CODE_TTL_SECONDS + 1.0);
        let code = started["code"].as_str().unwrap().to_string();
        let err = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap_err();
        // After expiry the lazy-collapse reports pairing_inactive (same as Python).
        assert_eq!(err.code, "pairing_inactive");
    }

    #[test]
    fn pairing_mode_off() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let err = svc
            .complete_pairing("123456", "wylde", "letmein", None)
            .unwrap_err();
        assert_eq!(err.code, "pairing_inactive");
    }

    #[test]
    fn cancel_pairing_idempotent() {
        let (_tmp, _h, svc, _c) = fresh_service();
        svc.start_pairing();
        assert_eq!(svc.get_pairing_status()["pairing_active"], true);
        let out = svc.cancel_pairing();
        assert_eq!(out["cancelled"], true);
        assert_eq!(svc.get_pairing_status()["pairing_active"], false);
        let out = svc.cancel_pairing();
        assert_eq!(out["cancelled"], false);
    }

    #[test]
    fn verify_returns_tier() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let token = paired["token"].as_str().unwrap();
        let out = svc.verify(token).unwrap();
        assert_eq!(out["device_id"], paired["device_id"]);
        assert_eq!(out["tier"], TIER_READ_ONLY);
    }

    #[test]
    fn verify_rejects_invalid_token() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let err = svc.verify("nope").unwrap_err();
        assert_eq!(err.code, "invalid_token");
    }

    #[test]
    fn verify_updates_last_seen() {
        let (_tmp, _h, svc, clock) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        clock.advance(60.0);
        svc.verify(paired["token"].as_str().unwrap()).unwrap();
        let devices = svc.list_devices(120.0);
        assert!(devices[0]["last_seen"].as_f64().unwrap() >= 1_000_060.0);
    }

    #[test]
    fn set_tier_persists() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let did = paired["device_id"].as_str().unwrap();
        let out = svc.set_tier(did, TIER_TOOL_USE).unwrap();
        assert_eq!(out["tier"], TIER_TOOL_USE);
        let token = paired["token"].as_str().unwrap();
        assert_eq!(svc.verify(token).unwrap()["tier"], TIER_TOOL_USE);
    }

    #[test]
    fn set_tier_rejects_unknown() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let err = svc
            .set_tier(paired["device_id"].as_str().unwrap(), "superuser")
            .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn rotate_token_invalidates_old() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let did = paired["device_id"].as_str().unwrap().to_string();
        let old_token = paired["token"].as_str().unwrap().to_string();

        let rotated = svc.rotate_token(&did).unwrap();
        let new_token = rotated["new_token"].as_str().unwrap().to_string();
        assert_ne!(new_token, old_token);

        assert_eq!(svc.verify(&new_token).unwrap()["device_id"], did);
        assert!(svc.verify(&old_token).is_err());
    }

    #[test]
    fn rotate_emits_token_rotated_event() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let did = paired["device_id"].as_str().unwrap().to_string();
        svc.verify(paired["token"].as_str().unwrap()).unwrap();
        let rotated = svc.rotate_token(&did).unwrap();

        let events = svc.consume_pending_events(&did);
        assert!(events.iter().any(|e| e["type"] == "token_rotated"));
        let ev = events
            .iter()
            .find(|e| e["type"] == "token_rotated")
            .unwrap();
        assert_eq!(ev["new_token"], rotated["new_token"]);
        // Drain is at-most-once.
        assert!(svc.consume_pending_events(&did).is_empty());
    }

    #[test]
    fn revoke_removes_device_and_token() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let did = paired["device_id"].as_str().unwrap().to_string();
        let token = paired["token"].as_str().unwrap().to_string();
        svc.revoke(&did).unwrap();
        assert!(svc.list_devices(60.0).is_empty());
        assert!(svc.verify(&token).is_err());
    }

    #[test]
    fn revoke_emits_event() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        let paired = svc
            .complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let did = paired["device_id"].as_str().unwrap().to_string();
        svc.revoke(&did).unwrap();
        let events = svc.consume_pending_events(&did);
        assert!(events.iter().any(|e| e["type"] == "revoked"));
    }

    #[test]
    fn revoke_unknown_device_errors() {
        let (_tmp, _h, svc, _c) = fresh_service();
        let err = svc.revoke("dev_nonexistent").unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn list_devices_active_threshold() {
        let (_tmp, _h, svc, clock) = fresh_service();
        let started = svc.start_pairing();
        let code = started["code"].as_str().unwrap().to_string();
        svc.complete_pairing(&code, "wylde", "letmein", None)
            .unwrap();
        let devices = svc.list_devices(60.0);
        assert_eq!(devices[0]["is_active"], true);

        clock.advance(120.0);
        let devices = svc.list_devices(60.0);
        assert_eq!(devices[0]["is_active"], false);
    }

    #[test]
    fn mint_code_length() {
        let code = mint_code();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn mint_token_is_32_hex_chars() {
        let t = mint_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
