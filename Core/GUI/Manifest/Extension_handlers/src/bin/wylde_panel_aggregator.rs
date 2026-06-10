//! Build-time panel aggregator.
//!
//! Scans the Wylde repository for panel manifests (first-party panels
//! under `Core/GUI/Frontend/Panels/<Name>/manifest.json` plus per-
//! service `<Service>/Frontend/manifest.json` files) and emits a
//! deterministic `panel_registry.generated.rs` populating a
//! `OnceCell<Vec<PanelEntry>>`-flavoured `register_all(...)` function.
//!
//! Usage:
//!
//!   wylde-panel-aggregator [--repo-root <PATH>] [--output <PATH>]
//!
//! With no flags the binary walks up from the current directory
//! looking for `pyproject.toml` (the repo-root sentinel — see the
//! GPUI rewrite plan §12.4 recommendation) and writes
//! `Manifest/Extension_handlers/src/generated.rs` under it.
//!
//! The aggregator is also exposed as a library function
//! (`aggregate`) so unit tests can run it against a fixture tree
//! without spawning a process.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wylde_panel_registry::{parse_panel_manifest, PanelEntry, PanelManifest, PanelSource};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut repo_root: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo-root" => {
                i += 1;
                repo_root = Some(PathBuf::from(args.get(i).cloned().unwrap_or_default()));
            }
            "--output" => {
                i += 1;
                output = Some(PathBuf::from(args.get(i).cloned().unwrap_or_default()));
            }
            "-h" | "--help" => {
                println!(
                    "wylde-panel-aggregator [--repo-root PATH] [--output PATH]\n\
                     \n\
                     Scans the repo for panel manifests and emits a generated\n\
                     register_all() Rust source file.\n",
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    let root = match repo_root {
        Some(r) => r,
        None => match locate_repo_root() {
            Some(r) => r,
            None => {
                eprintln!(
                    "could not locate repo root — no `pyproject.toml` found walking up from {:?}",
                    std::env::current_dir().ok()
                );
                return ExitCode::FAILURE;
            }
        },
    };
    let output = output
        .unwrap_or_else(|| root.join("Core/GUI/Manifest/Extension_handlers/src/generated.rs"));

    let manifests = match discover_manifests(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("manifest discovery failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let rendered = match render_generated(&manifests) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("render failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = atomic_write(&output, rendered.as_bytes()) {
        eprintln!("write to {}: {e}", output.display());
        return ExitCode::FAILURE;
    }

    println!(
        "wrote {} panels to {}",
        manifests
            .iter()
            .map(|d| d.manifest.panels.len())
            .sum::<usize>(),
        normalise_for_log(&output),
    );
    ExitCode::SUCCESS
}

/// A discovered manifest plus its on-disk location (for error
/// messages and the determinism comment in the generated file).
#[derive(Debug, Clone)]
pub struct DiscoveredManifest {
    pub path: PathBuf,
    /// Path relative to the repo root, using forward slashes.  Used in
    /// the generated comment so the file is byte-stable across
    /// machines.
    pub relative_path: String,
    pub manifest: PanelManifest,
}

/// Walk up from `start` looking for `pyproject.toml`.  Returns the
/// directory containing it.
pub fn locate_repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    locate_repo_root_from(&cwd)
}

pub fn locate_repo_root_from(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(p) = cur {
        if p.join("pyproject.toml").exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Find every `manifest.json` under:
///
///   * `Core/GUI/Frontend/Panels/*/manifest.json`            (first-party)
///   * `<service>/Frontend/manifest.json`                    (per-service)
///
/// Returns them in a deterministic order (sorted by `relative_path`)
/// so the generated file's row ordering doesn't depend on the
/// filesystem.
pub fn discover_manifests(root: &Path) -> anyhow::Result<Vec<DiscoveredManifest>> {
    let mut out = Vec::new();

    let panels_root = root.join("Core/GUI/Frontend/Panels");
    if panels_root.is_dir() {
        for entry in std::fs::read_dir(&panels_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let m = path.join("manifest.json");
            if m.is_file() {
                push_manifest(root, &m, &mut out)?;
            }
        }
    }

    // Per-service Frontends.  We scan the obvious roots; the search is
    // intentionally narrow to avoid wandering into `node_modules` /
    // `target` / `.venv`.
    for service_root in candidate_service_roots(root) {
        if !service_root.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&service_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let m = path.join("Frontend").join("manifest.json");
            if m.is_file() {
                push_manifest(root, &m, &mut out)?;
            }
        }
    }

    out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(out)
}

fn candidate_service_roots(root: &Path) -> Vec<PathBuf> {
    // Wylde groups services into a couple of top-level folders.  The
    // panel registry only cares about those that *might* ship a
    // Frontend; skip Voice/Memgraph etc that are headless.
    vec![root.join("Core"), root.join("Services")]
}

fn push_manifest(
    root: &Path,
    path: &Path,
    out: &mut Vec<DiscoveredManifest>,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let manifest = match parse_panel_manifest(&raw) {
        Ok(m) => m,
        // Not all `manifest.json` files in the repo are panel
        // manifests — extension `manifest.json` (Phase 12.7), service
        // descriptors, etc.  Treat a parse error as "this isn't ours,
        // skip" rather than aborting the whole discovery.
        Err(e) => {
            // Helpful breadcrumb on stderr, but don't fail the build.
            eprintln!(
                "skipping {}: not a panel manifest ({e})",
                normalise_for_log(path)
            );
            return Ok(());
        }
    };
    let rel = path
        .strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf());
    out.push(DiscoveredManifest {
        path: path.to_path_buf(),
        relative_path: normalise_path(&rel),
        manifest,
    });
    Ok(())
}

/// Render the generated source.  Pure function over the discovered
/// manifests so a snapshot test can compare its output byte-for-byte.
pub fn render_generated(manifests: &[DiscoveredManifest]) -> anyhow::Result<String> {
    let mut s = String::new();
    s.push_str("//! Generated by `wylde-panel-aggregator`.\n");
    s.push_str("//!\n");
    s.push_str("//! Do not edit by hand — your changes will be overwritten the next\n");
    s.push_str("//! time the aggregator runs against the repo.\n");
    s.push_str("//!\n");
    s.push_str("//! Sources read for this generation:\n");
    for d in manifests {
        s.push_str(&format!("//!   - {}\n", d.relative_path));
    }
    if manifests.is_empty() {
        s.push_str("//!   (none — registry will be empty)\n");
    }
    s.push_str("//!\n");
    s.push_str("//! Build-time aggregator design lives in the\n");
    s.push_str("//! `wylde_panel_aggregator` binary; see its CLI usage there.\n\n");

    s.push_str("use crate::factories::FactoryMap;\n");
    s.push_str("use crate::manifest::{PanelEntry, PanelOrigin, PanelSource};\n");
    s.push_str("use crate::registry::{PanelRegistry, RegistryError, RegistryRow};\n\n");

    s.push_str("/// Populate the supplied registry with every compiled-in first-party\n");
    s.push_str("/// panel.  `factories` carries the wiring from factory strings to real\n");
    s.push_str("/// closures; each panel takes its closure out of the map as it is\n");
    s.push_str("/// registered.\n");
    s.push_str("///\n");
    s.push_str("/// Returns an error if a factory referenced in the generated source\n");
    s.push_str("/// isn't present in the map (caught at startup so it's a loud failure,\n");
    s.push_str("/// not a silent missing-tab at runtime).\n");
    s.push_str("pub fn register_all(\n");
    s.push_str("    registry: &mut PanelRegistry,\n");
    s.push_str("    factories: &mut FactoryMap,\n");
    s.push_str(") -> Result<(), RegistryError> {\n");

    for d in manifests {
        for p in &d.manifest.panels {
            render_panel_block(&mut s, &d.manifest.service, &d.relative_path, p)?;
        }
    }

    s.push_str("    Ok(())\n");
    s.push_str("}\n");
    Ok(s)
}

fn render_panel_block(
    out: &mut String,
    service: &str,
    relative_path: &str,
    panel: &PanelEntry,
) -> anyhow::Result<()> {
    out.push_str(&format!(
        "\n    // ── {service} / {id}  (from {rel}) ──\n",
        service = service,
        id = panel.id,
        rel = relative_path,
    ));
    out.push_str("    {\n");

    let icon_lit = match &panel.icon {
        Some(s) => format!("Some({}.into())", rust_str(s)),
        None => "None".into(),
    };
    let required = if panel.required_services.is_empty() {
        "vec![]".to_string()
    } else {
        let inner = panel
            .required_services
            .iter()
            .map(|s| format!("{}.into()", rust_str(s)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("vec![{inner}]")
    };

    match &panel.source {
        PanelSource::GpuiView { factory } => {
            out.push_str(&format!(
                "        let factory_key = {key};\n",
                key = rust_str(factory),
            ));
            out.push_str(
                "        let factory = factories\n            .take(factory_key)\n            .ok_or_else(|| RegistryError::MissingFactory(factory_key.into()))?;\n",
            );
            out.push_str("        registry.register_internal(RegistryRow {\n");
            out.push_str(&format!(
                "            origin: PanelOrigin::FirstParty {{ service: {svc}.into() }},\n",
                svc = rust_str(service),
            ));
            out.push_str("            entry: PanelEntry {\n");
            out.push_str(&format!(
                "                id: {id}.into(),\n",
                id = rust_str(&panel.id),
            ));
            out.push_str(&format!(
                "                title: {t}.into(),\n",
                t = rust_str(&panel.title),
            ));
            out.push_str(&format!("                icon: {icon_lit},\n"));
            out.push_str(&format!(
                "                order: {order},\n",
                order = panel.order
            ));
            out.push_str(&format!(
                "                version: {v}.into(),\n",
                v = rust_str(&panel.version),
            ));
            out.push_str(&format!("                required_services: {required},\n"));
            out.push_str(
                "                source: PanelSource::GpuiView { factory: factory_key.into() },\n",
            );
            out.push_str("            },\n");
            out.push_str("            factory: Some(factory),\n");
            out.push_str("        })?;\n");
        }
        PanelSource::Iframe {
            url,
            sandbox,
            health_check,
        } => {
            let sandbox_lit = optional_str(sandbox);
            let health_lit = optional_str(health_check);
            out.push_str("        registry.register_internal(RegistryRow {\n");
            out.push_str(&format!(
                "            origin: PanelOrigin::FirstParty {{ service: {svc}.into() }},\n",
                svc = rust_str(service),
            ));
            out.push_str("            entry: PanelEntry {\n");
            out.push_str(&format!(
                "                id: {id}.into(),\n",
                id = rust_str(&panel.id),
            ));
            out.push_str(&format!(
                "                title: {t}.into(),\n",
                t = rust_str(&panel.title),
            ));
            out.push_str(&format!("                icon: {icon_lit},\n"));
            out.push_str(&format!(
                "                order: {order},\n",
                order = panel.order
            ));
            out.push_str(&format!(
                "                version: {v}.into(),\n",
                v = rust_str(&panel.version),
            ));
            out.push_str(&format!("                required_services: {required},\n"));
            out.push_str(&format!(
                "                source: PanelSource::Iframe {{\n                    url: {u}.into(),\n                    sandbox: {sb},\n                    health_check: {hc},\n                }},\n",
                u = rust_str(url),
                sb = sandbox_lit,
                hc = health_lit,
            ));
            out.push_str("            },\n");
            out.push_str("            factory: None,\n");
            out.push_str("        })?;\n");
        }
    }
    out.push_str("    }\n");
    Ok(())
}

fn rust_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn optional_str(s: &Option<String>) -> String {
    match s {
        Some(v) => format!("Some({}.into())", rust_str(v)),
        None => "None".into(),
    }
}

/// Normalise a path to forward slashes — keeps generated output the
/// same on Windows and Unix.
fn normalise_path(p: &Path) -> String {
    p.components()
        .map(|c| match c {
            std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().to_string(),
            std::path::Component::RootDir => "".into(),
            std::path::Component::CurDir => ".".into(),
            std::path::Component::ParentDir => "..".into(),
            std::path::Component::Normal(s) => s.to_string_lossy().to_string(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn normalise_for_log(p: &Path) -> String {
    normalise_path(p)
}

/// Atomic write — temp file + rename so a crashing aggregator can't
/// leave a half-written `generated.rs` that breaks the next build.
fn atomic_write(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("rs.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write a fixture tree with two panel manifests + one non-panel
    /// manifest (which must be silently skipped) and confirm the
    /// rendered source is deterministic.
    #[test]
    fn aggregator_snapshot_from_fixture_tree() {
        let td = tempdir();
        let root = td.path();

        // Repo-root sentinel — the aggregator's discovery uses
        // `Core/GUI/...` under it.
        fs::write(root.join("pyproject.toml"), "").unwrap();

        // First-party Settings panel.
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

        // Service iframe panel under Services/.
        let pho = root.join("Services/Photos/Frontend");
        fs::create_dir_all(&pho).unwrap();
        fs::write(
            pho.join("manifest.json"),
            r#"{
                "schema_version": 2,
                "service": "photos",
                "panels": [{
                    "id":"main","title":"Photos",
                    "order":40,"version":"0.1.0",
                    "source":{"kind":"iframe","url":"http://127.0.0.1:9300"}
                }]
            }"#,
        )
        .unwrap();

        // A non-panel manifest.json that must be skipped (the
        // extension-bridge plugin uses this same filename).
        let plugin = root.join("Services/SomePlugin/Frontend");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join("manifest.json"),
            r#"{"name":"plugin","transport":"stdio","command":["x"]}"#,
        )
        .unwrap();

        let manifests = discover_manifests(root).unwrap();
        assert_eq!(
            manifests.len(),
            2,
            "two panel manifests should be discovered, got: {:?}",
            manifests
                .iter()
                .map(|m| &m.relative_path)
                .collect::<Vec<_>>()
        );

        let rendered = render_generated(&manifests).expect("render");
        // The two key blocks land in the output.
        assert!(
            rendered.contains("wylde_panel_settings::SettingsPanel::view"),
            "settings factory must appear in rendered source",
        );
        assert!(rendered.contains("\"core\""), "core service must appear");
        assert!(
            rendered.contains("\"photos\""),
            "photos service must appear"
        );
        assert!(
            rendered.contains("http://127.0.0.1:9300"),
            "iframe URL must appear",
        );
        // Determinism: render twice, compare byte-for-byte.
        let rendered_again = render_generated(&manifests).unwrap();
        assert_eq!(rendered, rendered_again, "render must be deterministic");
    }

    #[test]
    fn discover_skips_directories_without_pyproject_sentinel_path_collisions() {
        // The discovery walk doesn't rely on the sentinel — it only
        // searches under `Core/GUI/Frontend/Panels/` and `Core|Services`
        // — but `locate_repo_root_from` does.  Confirm the sentinel
        // walk stops at the right level.
        let td = tempdir();
        let root = td.path();
        let nested = root.join("a/b/c/d");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let found = locate_repo_root_from(&nested).unwrap();
        assert_eq!(found.canonicalize().unwrap(), root.canonicalize().unwrap());
    }

    #[test]
    fn render_paths_use_forward_slashes() {
        let manifest = DiscoveredManifest {
            path: PathBuf::from(r"C:\repo\Core\GUI\Frontend\Panels\Settings\manifest.json"),
            relative_path: "Core/GUI/Frontend/Panels/Settings/manifest.json".into(),
            manifest: PanelManifest {
                schema_version: 2,
                service: "core".into(),
                panels: vec![PanelEntry {
                    id: "settings".into(),
                    title: "Settings".into(),
                    icon: Some("settings".into()),
                    order: 95,
                    version: "0.1.0".into(),
                    required_services: vec![],
                    source: PanelSource::GpuiView {
                        factory: "wylde_panel_settings::SettingsPanel::view".into(),
                    },
                }],
            },
        };
        let rendered = render_generated(&[manifest]).unwrap();
        assert!(
            !rendered.contains('\\'),
            "no backslashes in generated paths"
        );
        assert!(rendered.contains("Core/GUI/Frontend/Panels/Settings/manifest.json"));
    }

    /// Helpers — `tempfile` isn't on the workspace dep list, so we
    /// hand-roll a tiny version that creates a unique temp dir under
    /// `std::env::temp_dir()`.
    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> TempDirGuard {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "wylde-panel-aggregator-{}-{}",
            std::process::id(),
            n,
        ));
        std::fs::create_dir_all(&path).expect("create tempdir");
        TempDirGuard { path }
    }
}
