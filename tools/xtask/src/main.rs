//! `cargo xtask build-all` — the all-Rust multi-workspace build
//! orchestrator for Wylde's out-of-tree runtime model (locked decision 3;
//! plan §4).
//!
//! Wylde is two deliberately-separate Cargo workspaces (`rust/` backend +
//! `Core/GUI/` gpui, so gpui's graphics deps never ripple into the backend
//! lock file), plus — in the out-of-tree model — any number of populated
//! bucket repos under `Services/*` and `Extensions/*`, each its OWN Cargo
//! workspace with its own `target/`. A redeploy must cover all of them or a
//! dropped artifact silently lags (the deploy-gap, plan §4.2).
//!
//! `build-all` walks them and:
//!   1. builds `rust/` (backend) and `Core/GUI/` (GUI) — `cargo build
//!      --release` in each;
//!   2. for every populated bucket repo (a `Services/*` or `Extensions/*`
//!      child with a `Cargo.toml`): `cargo build --release` in its own
//!      folder, then **stages the produced binary beside its
//!      `manifest.json`** (`Services/<svc>/<bin>.exe`) — the exact release
//!      drop location the lifecycle daemon's sibling resolver
//!      (`sibling_binary_path`) reads.
//!
//! Build → drop → discover is the whole chain: the registry then discovers
//! `Services/<svc>/manifest.json` and the resolver finds the staged binary
//! beside it. Clean no-op when the buckets are absent/empty (core-only
//! build still runs).
//!
//! ## Tie to the F1 staleness / deploy-gap guard
//!
//! Staging a fresh artifact is only half of "deployed": a running service
//! that wasn't bounced is now on a STALE binary. `service.list`'s F1 guard
//! (`binary_predates_process`) reports `stale:true` for exactly that — and
//! it is now sibling-aware (it resolves a sibling's beside-manifest
//! binary), so the same `stale:0` gate W0 uses for core services covers
//! siblings too. `build-all` prints the deploy-gap reminder pointing at
//! that gate; the authoritative live `stale:0` assertion stays with the
//! redeploy/preflight step that queries `service.list` against a running
//! daemon.
//!
//! Standalone, all-Rust, shells out to `cargo`. A thin `tools/build-all.ps1`
//! wrapper and a `cargo xtask` alias (`.cargo/config.toml`) invoke it, but
//! the logic lives here.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

/// Out-of-tree buckets that hold per-item Cargo repos build-all compiles +
/// stages. `Core/Plugins/` is intentionally absent: plugins are compiled
/// INTO Core (the `rust/` workspace), not built as standalone repos.
const BUILD_BUCKETS: &[&str] = &["Services", "Extensions"];

#[derive(Parser)]
#[command(name = "xtask", about = "Wylde multi-workspace build orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Subcommand)]
enum XtaskCommand {
    /// Build Core (rust/ + Core/GUI/) and every populated bucket repo,
    /// staging each bucket binary beside its manifest.json.
    BuildAll(BuildAllArgs),
}

#[derive(Parser)]
struct BuildAllArgs {
    /// Repo root (the dir containing Core/ + rust/). Defaults to the
    /// detected root walking up from the current directory.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Build the `dev` (debug) profile instead of `release`.
    #[arg(long)]
    debug: bool,
    /// Skip the Core/GUI/ workspace (backend + buckets only).
    #[arg(long)]
    skip_gui: bool,
    /// Build only the out-of-tree bucket repos (skip Core's two workspaces).
    #[arg(long)]
    buckets_only: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        XtaskCommand::BuildAll(args) => build_all(args),
    }
}

/// One workspace/repo build-all compiles.
struct BuildRoot {
    /// Human label for the summary.
    label: String,
    /// Directory holding the `Cargo.toml` to build in.
    dir: PathBuf,
    /// `Some(folder)` ⇒ a bucket repo whose artifact must be staged beside
    /// the manifest in `folder`. `None` ⇒ a Core workspace (no staging).
    stage_into: Option<PathBuf>,
    /// Canonical service name (for staging) — bucket repos only.
    service_name: Option<String>,
}

fn build_all(args: BuildAllArgs) -> Result<()> {
    let root = match args.root {
        Some(r) => r,
        None => detect_root().context("could not locate the Wylde repo root (no rust/Cargo.toml found walking up); pass --root")?,
    };
    let profile = if args.debug { "debug" } else { "release" };
    println!("xtask build-all: root={} profile={profile}", root.display());

    let mut roots: Vec<BuildRoot> = Vec::new();
    if !args.buckets_only {
        roots.push(BuildRoot {
            label: "Core backend (rust/)".into(),
            dir: root.join("rust"),
            stage_into: None,
            service_name: None,
        });
        if !args.skip_gui {
            roots.push(BuildRoot {
                label: "Core GUI (Core/GUI/)".into(),
                dir: root.join("Core").join("GUI"),
                stage_into: None,
                service_name: None,
            });
        }
    }

    let buckets = discover_bucket_repos(&root);
    if buckets.is_empty() {
        println!("xtask: no populated bucket repos found (Services/*, Extensions/* absent, empty, or sourceless) — building Core only");
    }
    roots.extend(buckets);

    let mut failures: Vec<String> = Vec::new();
    let mut staged: Vec<String> = Vec::new();

    for br in &roots {
        if !br.dir.join("Cargo.toml").exists() {
            println!(
                "  SKIP  {} — no Cargo.toml at {}",
                br.label,
                br.dir.display()
            );
            continue;
        }
        println!("\n==> building {} ({})", br.label, br.dir.display());
        match run_cargo_build(&br.dir, args.debug) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("  FAIL  {}: {e:#}", br.label);
                failures.push(br.label.clone());
                continue; // don't stage a failed build
            }
        }
        // Stage a bucket artifact beside its manifest.
        if let (Some(stage_dir), Some(name)) = (&br.stage_into, &br.service_name) {
            match stage_artifact(&br.dir, stage_dir, name, profile) {
                Ok(Some(dest)) => {
                    println!("  STAGED {} -> {}", name, dest.display());
                    staged.push(format!("{name} ({})", dest.display()));
                }
                Ok(None) => {
                    eprintln!(
                        "  WARN  {}: build succeeded but no binary matched beside its manifest \
                         (looked for {:?} in {}/target/{profile}/)",
                        name,
                        staged_binary_candidates(name),
                        br.dir.display()
                    );
                }
                Err(e) => {
                    eprintln!("  WARN  {}: staging failed: {e:#}", name);
                    failures.push(format!("{} (stage)", br.label));
                }
            }
        }
    }

    println!("\n── build-all summary ──");
    println!("  built/attempted: {}", roots.len());
    if !staged.is_empty() {
        println!("  staged siblings:");
        for s in &staged {
            println!("    - {s}");
        }
        println!(
            "  NOTE (deploy-gap): a staged artifact is not live until its service is bounced. \
             Run `service.stop`+`service.start` (or dev `dev.restart_service`); `service.list` \
             reports `stale:true` for any still-running sibling on the old binary (F1 guard, now \
             sibling-aware) until you do."
        );
    }
    if !failures.is_empty() {
        bail!(
            "build-all: {} target(s) failed: {}",
            failures.len(),
            failures.join(", ")
        );
    }
    println!("  all targets green.");
    Ok(())
}

/// Walk up from the current dir to find the repo root (the dir containing
/// `rust/Cargo.toml` and a `Core` dir). Honours `WYLDE_ROOT` first.
fn detect_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("WYLDE_ROOT") {
        let p = PathBuf::from(v);
        if is_repo_root(&p) {
            return Some(p);
        }
    }
    let mut cur = std::env::current_dir().ok()?;
    loop {
        if is_repo_root(&cur) {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn is_repo_root(dir: &Path) -> bool {
    dir.join("rust").join("Cargo.toml").is_file() && dir.join("Core").is_dir()
}

/// Discover the populated bucket repos under `Services/*` / `Extensions/*`:
/// each immediate child with a `Cargo.toml`. A `_`/`.`-prefixed child is
/// skipped (matches the registry's discovery filter). Absent/empty buckets
/// yield nothing (clean no-op). Sorted by directory name for deterministic
/// build order.
fn discover_bucket_repos(root: &Path) -> Vec<BuildRoot> {
    let mut out: Vec<BuildRoot> = Vec::new();
    for bucket in BUILD_BUCKETS {
        let dir = root.join(bucket);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // bucket absent ⇒ no-op
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| !n.starts_with(['_', '.']))
                    .unwrap_or(false)
            })
            .collect();
        children.sort();
        for child in children {
            if !child.join("Cargo.toml").is_file() {
                continue; // a sourceless item (e.g. an iframe-only extension)
            }
            let dir_name = child
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            let service_name = manifest_service_name(&child).unwrap_or(dir_name.clone());
            out.push(BuildRoot {
                label: format!("{bucket}/{dir_name}"),
                stage_into: Some(child.clone()),
                service_name: Some(service_name),
                dir: child,
            });
        }
    }
    out
}

/// Read `<repo>/manifest.json`'s `name` field (the canonical service name).
fn manifest_service_name(repo: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(repo.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Candidate binary file names a bucket repo may produce, in resolution
/// order, matching the daemon's `sibling_binary_path`: `wylde-<stripped>`
/// then `<stripped>` (host exe suffix). `<stripped>` drops a `wylde-`
/// prefix from the service name.
fn staged_binary_candidates(service_name: &str) -> Vec<String> {
    let stripped = service_name.strip_prefix("wylde-").unwrap_or(service_name);
    let suffix = std::env::consts::EXE_SUFFIX;
    vec![
        format!("wylde-{stripped}{suffix}"),
        format!("{stripped}{suffix}"),
    ]
}

/// Copy the bucket repo's freshly-built binary from its own
/// `target/<profile>/` to beside its `manifest.json` (the stage dir).
/// Returns the dest path, or `None` if no candidate binary was produced.
fn stage_artifact(
    repo: &Path,
    stage_dir: &Path,
    service_name: &str,
    profile: &str,
) -> Result<Option<PathBuf>> {
    let target = repo.join("target").join(profile);
    for name in staged_binary_candidates(service_name) {
        let src = target.join(&name);
        if src.is_file() {
            let dest = stage_dir.join(&name);
            std::fs::copy(&src, &dest)
                .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
            return Ok(Some(dest));
        }
    }
    Ok(None)
}

/// Run `cargo build [--release]` in `dir`, inheriting stdio so the
/// compiler output streams to the operator.
fn run_cargo_build(dir: &Path, debug: bool) -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(dir).arg("build");
    if !debug {
        cmd.arg("--release");
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawn cargo build in {}", dir.display()))?;
    if !status.success() {
        bail!(
            "cargo build failed in {} (exit {:?})",
            dir.display(),
            status.code()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn staged_candidates_strip_and_suffix() {
        let cands = staged_binary_candidates("wylde-example");
        let suffix = std::env::consts::EXE_SUFFIX;
        assert_eq!(cands[0], format!("wylde-example{suffix}"));
        assert_eq!(cands[1], format!("example{suffix}"));
        // No prefix ⇒ used as-is for both forms.
        assert_eq!(
            staged_binary_candidates("example")[0],
            format!("wylde-example{suffix}")
        );
    }

    #[test]
    fn discover_skips_sourceless_and_underscore_and_handles_absent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // A real bucket repo (has Cargo.toml + manifest).
        touch(
            &root
                .join("Services")
                .join("wylde-example")
                .join("Cargo.toml"),
        );
        touch(
            &root
                .join("Services")
                .join("wylde-example")
                .join("manifest.json"),
        );
        fs::write(
            root.join("Services")
                .join("wylde-example")
                .join("manifest.json"),
            br#"{"name":"wylde-example"}"#,
        )
        .unwrap();
        // A sourceless item (iframe-only extension) — no Cargo.toml.
        touch(&root.join("Extensions").join("n8n").join("mcp-server.json"));
        // A scratch dir — underscore-prefixed.
        touch(&root.join("Services").join("_scratch").join("Cargo.toml"));

        let repos = discover_bucket_repos(root);
        let labels: Vec<&str> = repos.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["Services/wylde-example"]);
        assert_eq!(repos[0].service_name.as_deref(), Some("wylde-example"));
        assert!(repos[0].stage_into.is_some());
    }

    #[test]
    fn discover_is_noop_without_buckets() {
        let tmp = TempDir::new().unwrap();
        assert!(discover_bucket_repos(tmp.path()).is_empty());
    }

    #[test]
    fn discover_falls_back_to_dir_name_without_manifest_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        touch(&root.join("Services").join("gallery").join("Cargo.toml"));
        let repos = discover_bucket_repos(root);
        assert_eq!(repos.len(), 1);
        // No manifest ⇒ the dir name is the staging name.
        assert_eq!(repos[0].service_name.as_deref(), Some("gallery"));
    }

    #[test]
    fn stage_artifact_copies_beside_manifest() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("Services").join("wylde-example");
        let suffix = std::env::consts::EXE_SUFFIX;
        let built = repo
            .join("target")
            .join("release")
            .join(format!("wylde-example{suffix}"));
        touch(&built);
        let dest = stage_artifact(&repo, &repo, "wylde-example", "release")
            .unwrap()
            .expect("a binary was staged");
        assert_eq!(dest, repo.join(format!("wylde-example{suffix}")));
        assert!(dest.is_file());
    }

    #[test]
    fn stage_artifact_returns_none_when_no_binary() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("Services").join("wylde-example");
        fs::create_dir_all(repo.join("target").join("release")).unwrap();
        let got = stage_artifact(&repo, &repo, "wylde-example", "release").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn is_repo_root_requires_rust_and_core() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        assert!(!is_repo_root(root));
        touch(&root.join("rust").join("Cargo.toml"));
        fs::create_dir_all(root.join("Core")).unwrap();
        assert!(is_repo_root(root));
    }
}
