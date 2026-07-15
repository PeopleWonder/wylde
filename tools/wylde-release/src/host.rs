//! Git + host-environment capture for the preflight receipt and baseline.
//!
//! Thin wrappers over `git` and a couple of OS facts. Kept out of the pure
//! `bench`/`receipt` cores so those stay unit-testable without a repo or a
//! machine; everything here shells out and is exercised by the live preflight.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench::HostEnv;

/// The full commit SHA at `HEAD`.
pub fn head_commit(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("spawning git rev-parse (is git on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the working tree (tracked files) has uncommitted changes. Untracked
/// files are ignored — a stray scratch file next to the repo shouldn't mark a
/// clean commit as un-releasable; only modifications to tracked content do.
pub fn is_dirty(repo_root: &Path) -> Result<bool> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .context("spawning git status")?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Best-effort Ollama server version via the local HTTP API, using `curl` if
/// present (no HTTP client dependency for a dev tool). Empty string on failure —
/// the version is informational, never gating.
fn ollama_version() -> String {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let url = format!("{}/api/version", host.trim_end_matches('/'));
    let Ok(out) = Command::new("curl").args(["-s", "--max-time", "3", &url]).output() else {
        return String::new();
    };
    if !out.status.success() {
        return String::new();
    }
    // Body looks like {"version":"0.x.y"} — pull the value without a JSON dep.
    let body = String::from_utf8_lossy(&out.stdout);
    body.split("\"version\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .unwrap_or("")
        .to_string()
}

/// Capture the host environment. The rig specifics (CPU/GPU/RAM) default to
/// Aaron's known release machine — the numbers only mean something against a
/// named rig, and this tool only ever runs there — but every field is
/// overridable so a second machine records itself honestly.
///
/// * `label` — human rig name (`--host-label`).
/// * `model` — the reasoner model the arms ran against.
pub fn capture(label: &str, model: &str) -> HostEnv {
    HostEnv {
        label: label.to_string(),
        cpu: env_or("WYLDE_BENCH_CPU", "Intel Core Ultra 9 285K"),
        gpu: env_or("WYLDE_BENCH_GPU", "NVIDIA RTX 5080 16GB"),
        ram: env_or("WYLDE_BENCH_RAM", "DDR5-4800"),
        os: os_string(),
        model: model.to_string(),
        ollama: ollama_version(),
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn os_string() -> String {
    // `cmd /c ver` gives the Windows build; fall back to the compile-time OS.
    if cfg!(windows) {
        if let Ok(out) = Command::new("cmd").args(["/c", "ver"]).output() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    std::env::consts::OS.to_string()
}
