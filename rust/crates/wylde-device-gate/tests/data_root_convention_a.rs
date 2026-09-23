//! The device gate's store follows convention A, and an upgrade does not
//! unpair every device getting there (#250).
//!
//! Until #250 the gate rooted at `<WYLDE_ROOT>/device_gate/data` — a third
//! top-level tree beside `.wylde/data` and `data`. `devices.json` holds the
//! paired mobiles and their tokens and `htpasswd` their credentials, so a
//! resolver that moved without the bytes presents as *every device is
//! unpaired* with nothing logged: the exact failure mode #250 names.
//!
//! These tests bind `WYLDE_ROOT` to a tempdir and clear the overrides, so
//! they exercise the convention-A fallback — the arm that actually changed.
//! `DEVICE_GATE_DATA_DIR` short-circuits everything below it and is checked
//! separately, because several suites depend on it as a test seam.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env vars are process-wide. Cargo gives this integration-test binary its
/// own process, so a binary-local lock is the right scope — nothing outside
/// this file can race these vars.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Clears every root-affecting var, pins `WYLDE_ROOT`, and restores the lot
/// on drop so a failing assertion cannot leak env into the next test.
struct Rooted {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl Rooted {
    fn new() -> Self {
        const VARS: [&str; 5] = [
            "WYLDE_ROOT",
            "WYLDE_DATA_DIR",
            "DATA_DIR",
            "DEVICE_GATE_DATA_DIR",
            "DEVICE_GATE_HTPASSWD",
        ];
        let saved = VARS
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        std::env::set_var("WYLDE_ROOT", &root);
        Self {
            _tmp: tmp,
            root,
            saved,
        }
    }

    /// `<root>/.wylde/data/device_gate` — where the store belongs now.
    fn canonical(&self) -> PathBuf {
        self.root.join(".wylde").join("data").join("device_gate")
    }

    /// `<root>/device_gate/data` — the pre-#250 third top-level tree.
    fn legacy(&self) -> PathBuf {
        self.root.join("device_gate").join("data")
    }
}

impl Drop for Rooted {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn seed(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// The store roots under convention A, absolutely — not under the old
/// `device_gate/` tree and not relative to the process working directory.
#[test]
fn store_roots_under_convention_a() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = Rooted::new();

    let root = wylde_device_gate::store_root_path();
    assert_eq!(root, env.canonical());
    assert!(
        root.is_absolute(),
        "a cwd-relative gate store makes paired devices a property of the \
         working directory: {}",
        root.display()
    );
    assert_eq!(
        wylde_device_gate::devices_store_path(),
        env.canonical().join("devices.json")
    );
}

/// THE upgrade guarantee: devices present only at the legacy path still read
/// after the move. Without adoption the gate starts with an empty roster and
/// every paired mobile has to be paired again by hand.
#[test]
fn a_legacy_only_device_roster_is_still_read_after_the_move() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = Rooted::new();

    seed(
        &env.legacy().join("devices.json"),
        r#"{"devices":[{"device_id":"phone-1","tier":"read_only"}]}"#,
    );
    seed(&env.legacy().join("htpasswd"), "wylde:$2y$hash\n");
    assert!(
        !env.canonical().exists(),
        "precondition: nothing at the canonical root yet"
    );

    let devices = wylde_device_gate::devices_store_path();
    assert_eq!(devices, env.canonical().join("devices.json"));
    assert!(
        devices.is_file(),
        "the legacy roster should have been adopted to {}",
        devices.display()
    );
    let body = std::fs::read_to_string(&devices).expect("read adopted roster");
    assert!(
        body.contains("phone-1"),
        "an upgrade must not unpair every device; got {body}"
    );
    // The credentials came across in the same tree.
    assert!(env.canonical().join("htpasswd").is_file());
    // One-way: the legacy tree is preserved, so a downgrade still reads it.
    assert!(env.legacy().join("devices.json").is_file());
}

/// Idempotent and one-way: state written since the move outranks the legacy
/// copy, however many times resolution re-runs adoption.
#[test]
fn a_stale_legacy_roster_never_overwrites_the_canonical_one() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = Rooted::new();

    seed(
        &env.legacy().join("devices.json"),
        r#"{"devices":["stale"]}"#,
    );
    // First resolution adopts; then the gate writes a newer roster.
    let devices = wylde_device_gate::devices_store_path();
    std::fs::write(&devices, r#"{"devices":["fresh"]}"#).expect("write");

    for _ in 0..3 {
        let p = wylde_device_gate::devices_store_path();
        assert_eq!(
            std::fs::read_to_string(&p).expect("read"),
            r#"{"devices":["fresh"]}"#,
            "adoption must not re-run over a populated canonical store"
        );
    }
    assert_eq!(
        std::fs::read_to_string(env.legacy().join("devices.json")).expect("read"),
        r#"{"devices":["stale"]}"#,
        "the legacy tree is read-only to this migration"
    );
}

/// `DEVICE_GATE_DATA_DIR` is a test seam several suites depend on — it still
/// wins outright, with no adoption behind it.
#[test]
fn the_device_gate_env_override_still_wins() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let env = Rooted::new();

    let elsewhere = env.root.join("operator-chosen");
    std::env::set_var("DEVICE_GATE_DATA_DIR", &elsewhere);
    assert_eq!(wylde_device_gate::store_root_path(), elsewhere);
    assert_eq!(
        wylde_device_gate::devices_store_path(),
        elsewhere.join("devices.json")
    );
    // An override must not drag the legacy tree along with it.
    assert!(!elsewhere.exists() || !elsewhere.join("devices.json").exists());
}
