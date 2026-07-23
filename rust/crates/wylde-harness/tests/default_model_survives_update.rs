//! The persistent default model must survive an **update**, not just a
//! shutdown (#243) — the guarantee #132 gave installed models, applied to
//! the #235 default.
//!
//! ## What an update actually does
//!
//! `wylde_updater::install_stack` stages a fully-verified stack into
//! `<home>/versions/<version>/`, flips the one-line pointer at
//! `%LOCALAPPDATA%\Wylde\current` to it, then prunes older version
//! directories. Its entire write surface is:
//!
//! ```text
//! %LOCALAPPDATA%\Wylde\versions\<v>\   staged binaries
//! %LOCALAPPDATA%\Wylde\current         the pointer
//! %LOCALAPPDATA%\Wylde\versions\*      the prune
//! ```
//!
//! The estate root — where `launch_wylde.ps1` exports `WYLDE_ROOT` from,
//! and therefore the working directory lifecycle spawns every service with
//! (`state/services.rs`, `cmd.current_dir(wylde_root())`) — is never
//! written. The default-model store resolves under that root, so it sits
//! *outside* the replaced tree and survives.
//!
//! ## Why a test, if it already survives
//!
//! Because it survives by circumstance, not by construction, and nothing
//! asserted it. The store lives in the stack/estate tree rather than a
//! designated user-data directory; it stays safe only while the updater's
//! blast radius stays narrow. These tests turn that from an accident into
//! a checked property: rooting model-selection state inside the stack
//! directory — the change that would make every update silently reset the
//! user's default — now turns the build red.
//!
//! Deliberately NOT asserted here: *which* root the store uses. #250 has
//! since moved it onto convention A (`<WYLDE_ROOT>/.wylde/data`) along with
//! the model registry, device gate and ollama overrides — but update-survival
//! never depended on that choice, only on the root being outside `versions/`,
//! which both the canonical and the legacy root are. Keeping this suite
//! root-agnostic is the point: it stays a check on the updater's blast
//! radius, not a second copy of the convention-A gate (that lives in
//! `wylde-shared/tests/single_data_dir_resolver.rs`, and the store's own
//! `#[test] selection_stores_root_under_convention_a`).
//!
//! These tests bind `DATA_DIR`, which convention A honours as-is, so they
//! exercise the same resolution path before and after #250.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use wylde_harness::model_registry::model_state;

/// Env vars are process-wide and every test here rebinds `DATA_DIR`, so
/// they serialize. Binary-local is the right scope: cargo gives each
/// integration-test binary its own process, so nothing outside this file
/// can race these vars.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Lay out a fake install: an estate root holding the user's data, and a
/// separate `versions/` tree holding the swappable stack. Mirrors the real
/// split — `%LOCALAPPDATA%\Wylde\versions\<v>` for binaries, the estate
/// root for everything the user owns.
struct FakeInstall {
    _tmp: tempfile::TempDir,
    estate: PathBuf,
    versions: PathBuf,
}

impl FakeInstall {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let estate = tmp.path().join("estate");
        let versions = tmp.path().join("home").join("versions");
        std::fs::create_dir_all(estate.join("data")).expect("estate data");
        std::fs::create_dir_all(&versions).expect("versions");
        Self {
            _tmp: tmp,
            estate,
            versions,
        }
    }

    /// Stage a stack directory the way the updater does, and return it.
    fn stage_stack(&self, version: &str) -> PathBuf {
        let dir = self.versions.join(version);
        std::fs::create_dir_all(&dir).expect("stage");
        // A stand-in for the staged binaries — enough that removing the
        // directory is a real deletion of real bytes.
        std::fs::write(dir.join("wylde-harness.exe"), b"stack binary").expect("write binary");
        dir
    }

    /// Point the model-selection store at this install's estate data dir,
    /// the way a launched harness resolves it (`DATA_DIR`, else `"data"`
    /// relative to the working directory lifecycle pins to `WYLDE_ROOT`).
    fn activate(&self) {
        std::env::set_var("DATA_DIR", self.estate.join("data"));
        std::env::remove_var("DEFAULT_MODEL_PATH");
        std::env::remove_var("ACTIVE_MODEL_PATH");
        std::env::remove_var("WYLDE_DEFAULT_MODEL");
        model_state::reset_for_tests();
    }
}

/// Apply an update the way `install_stack` does: stage the new version,
/// then prune the old one. The estate root is deliberately untouched —
/// that is the behaviour under test.
fn apply_update(install: &FakeInstall, from: &Path, to_version: &str) -> PathBuf {
    let new_stack = install.stage_stack(to_version);
    std::fs::remove_dir_all(from).expect("prune the superseded version dir");
    assert!(!from.exists(), "the old stack really was removed");
    new_stack
}

/// THE guarantee: set a default, update the stack underneath it, and the
/// default is still there — no reset, no fallback to first-available.
#[test]
fn persisted_default_survives_a_stack_update() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let install = FakeInstall::new();
    let old_stack = install.stage_stack("0.2.0");
    install.activate();

    model_state::set_default_model(Some("qwen3.5:9b"));
    assert_eq!(
        model_state::get_default_model(),
        Some("qwen3.5:9b".to_owned())
    );

    apply_update(&install, &old_stack, "0.2.1");

    // A fresh stack means a fresh process: drop every in-memory cache so
    // the read has to come off disk, exactly as it would after restart.
    model_state::reset_for_tests();

    assert_eq!(
        model_state::get_default_model(),
        Some("qwen3.5:9b".to_owned()),
        "the persisted default must survive an update, not just a shutdown \
         (#243) — a user who starred a model does not re-star it every release"
    );

    std::env::remove_var("DATA_DIR");
}

/// The structural half, and the one with teeth: the store must not resolve
/// *inside* the directory an update replaces. This is what actually
/// guarantees survival — the round-trip above would still pass if the
/// store moved into the stack dir and the test simply never deleted it.
#[test]
fn selection_state_never_resolves_inside_the_replaced_stack_tree() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let install = FakeInstall::new();
    let stack = install.stage_stack("0.2.0");
    install.activate();

    for (label, path) in [
        ("default", model_state::default_model_store_path()),
        ("active", model_state::active_model_store_path()),
    ] {
        assert!(
            !path.starts_with(&install.versions),
            "{label}-model store resolved to {} — inside the updater's \
             versions/ tree, which install_stack prunes. Every update would \
             silently reset the user's selection.",
            path.display()
        );
        assert!(
            !path.starts_with(&stack),
            "{label}-model store resolved inside the live stack directory {}",
            stack.display()
        );
        assert!(
            path.starts_with(install.estate.join("data")),
            "{label}-model store should sit under the estate data dir, got {}",
            path.display()
        );
    }

    std::env::remove_var("DATA_DIR");
}

/// An update that lands while a default is set must not resurrect an older
/// default from a superseded version directory. Pins that the store is
/// read from one place, not merged across stacks.
#[test]
fn a_superseded_stack_cannot_shadow_the_live_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let install = FakeInstall::new();
    let old_stack = install.stage_stack("0.2.0");
    install.activate();

    // A stale copy inside the OLD stack dir — the shape a pre-#243 layout
    // (or a future regression) would leave behind.
    std::fs::write(
        old_stack.join("default_model.json"),
        br#"{"model":"llama3.2:3b"}"#,
    )
    .expect("stale copy");

    model_state::set_default_model(Some("qwen3.5:9b"));
    apply_update(&install, &old_stack, "0.2.1");
    model_state::reset_for_tests();

    assert_eq!(
        model_state::get_default_model(),
        Some("qwen3.5:9b".to_owned()),
        "the live default wins; a copy inside a superseded stack dir is not \
         a source of truth"
    );

    std::env::remove_var("DATA_DIR");
}
