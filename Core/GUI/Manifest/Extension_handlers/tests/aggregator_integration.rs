//! End-to-end: build the aggregator binary, point it at a fixture
//! tree, then `include!`-evaluate its output for shape checks.
//!
//! Snapshot-style assertions live in the binary's own unit tests
//! (`src/bin/wylde_panel_aggregator.rs`); this file ensures the
//! compiled binary actually walks the fixture tree the same way.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn aggregator_binary_emits_generated_file_for_repo_fixture() {
    let td = tempdir();
    let root = td.path();
    fs::write(root.join("pyproject.toml"), "").unwrap();
    let settings = root.join("Core/GUI/Frontend/Panels/Settings");
    fs::create_dir_all(&settings).unwrap();
    fs::write(
        settings.join("manifest.json"),
        r#"{
            "schema_version": 2,
            "service": "core",
            "panels": [{
                "id":"settings","title":"Settings","icon":"settings",
                "order":95,"version":"0.1.0",
                "source":{"kind":"gpui_view","factory":"wylde_panel_settings::SettingsPanel::view"}
            }]
        }"#,
    )
    .unwrap();

    let output = root.join("Core/GUI/Manifest/Extension_handlers/src/generated.rs");
    fs::create_dir_all(output.parent().unwrap()).unwrap();

    let status = Command::new(aggregator_bin())
        .arg("--repo-root")
        .arg(root)
        .arg("--output")
        .arg(&output)
        .status()
        .expect("aggregator binary spawn");
    assert!(
        status.success(),
        "aggregator exited with {:?}",
        status.code()
    );

    let generated = fs::read_to_string(&output).expect("read generated.rs");
    assert!(
        generated.contains("wylde_panel_settings::SettingsPanel::view"),
        "generated source missing settings factory:\n{generated}",
    );
    assert!(
        generated.contains("\"core\""),
        "generated source missing core service",
    );
    assert!(
        generated.contains("\"settings\""),
        "generated source missing settings id",
    );
    assert!(
        generated.contains("pub fn register_all"),
        "generated source missing register_all signature",
    );
}

/// Re-running the aggregator over the same input must produce an
/// identical output file.  The snapshot bit isn't pinned to a string
/// in this test (the source contains absolute paths under tempdir),
/// but byte equality between two runs is achievable.
#[test]
fn aggregator_output_is_deterministic_across_runs() {
    let td = tempdir();
    let root = td.path();
    fs::write(root.join("pyproject.toml"), "").unwrap();
    let panel_dir = root.join("Core/GUI/Frontend/Panels/Settings");
    fs::create_dir_all(&panel_dir).unwrap();
    fs::write(
        panel_dir.join("manifest.json"),
        r#"{
            "schema_version": 2,
            "service": "core",
            "panels": [{
                "id":"settings","title":"Settings","icon":"settings",
                "order":95,"version":"0.1.0",
                "source":{"kind":"gpui_view","factory":"wylde_panel_settings::SettingsPanel::view"}
            }]
        }"#,
    )
    .unwrap();

    let out1 = root.join("out1.rs");
    let out2 = root.join("out2.rs");
    let bin = aggregator_bin();
    for out in [&out1, &out2] {
        let status = Command::new(&bin)
            .arg("--repo-root")
            .arg(root)
            .arg("--output")
            .arg(out)
            .status()
            .expect("aggregator binary spawn");
        assert!(status.success());
    }
    let a = fs::read_to_string(&out1).unwrap();
    let b = fs::read_to_string(&out2).unwrap();
    assert_eq!(a, b, "two aggregator runs must produce byte-identical output");
}

/// Locate the just-built aggregator binary.  Cargo sets
/// `CARGO_BIN_EXE_<name>` for every binary in the package; that's the
/// hermetic way to find it.
fn aggregator_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wylde-panel-aggregator"))
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "wylde-panel-aggregator-it-{}-{}",
        std::process::id(),
        n,
    ));
    fs::create_dir_all(&path).expect("create tempdir");
    TempDir { path }
}
