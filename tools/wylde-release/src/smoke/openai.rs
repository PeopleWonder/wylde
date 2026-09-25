//! L3.9: the gateway's OpenAI-compatible `/v1` API answers on the running
//! stack (#345).
//!
//! Runs `tools/wylde-release/scripts/openai_v1_smoke.py`, which prints a
//! one-line JSON verdict. Without a token it checks, using only the Python
//! standard library, that `/v1` is mounted and rejects an unauthenticated
//! call with OpenAI's 401 shape. With `WYLDE_OPENAI_SMOKE_TOKEN` set to a
//! device token it drives the official `openai` client through models,
//! chat, a tool call, streaming, embeddings and FIM. Device-gate can't
//! mint a token without the maintainer's pairing credentials, so the full
//! flow is opt-in; the auth-only mode keeps the check passable on every
//! rig while still failing closed when `/v1` isn't served.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::CheckResult;

const KEY: &str = "l3.openai_v1";
const TITLE: &str = "L3.9 openai-v1";

/// The full flow can include a cold model load; the auth-only probe is fast.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(360);

/// Run the `/v1` smoke script and fold its verdict into a check result.
pub(super) fn check(repo_root: &Path) -> CheckResult {
    match run(repo_root) {
        Ok(detail) => CheckResult::pass(KEY, TITLE, detail),
        Err(e) => CheckResult::fail(KEY, TITLE, format!("{e:#}")),
    }
}

fn run(repo_root: &Path) -> Result<String> {
    let script = repo_root
        .join("tools")
        .join("wylde-release")
        .join("scripts")
        .join("openai_v1_smoke.py");
    if !script.is_file() {
        bail!("smoke script missing: {}", script.display());
    }
    let python = std::env::var("WYLDE_PYTHON").unwrap_or_else(|_| "python".to_owned());
    let mut cmd = Command::new(&python);
    cmd.arg(&script);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(cmd.output()); // wylde-check: discard-result-ok
    });
    let output = rx
        .recv_timeout(SMOKE_TIMEOUT)
        .map_err(|_| anyhow::anyhow!("smoke script timed out after {}s", SMOKE_TIMEOUT.as_secs()))?
        .with_context(|| format!("could not run `{python}` (set WYLDE_PYTHON)"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    verdict(&stdout).with_context(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!(
            "stderr: {}",
            stderr.trim().chars().take(300).collect::<String>()
        )
    })
}

/// Turn the script's JSON verdict (its last non-empty stdout line) into a
/// pass detail, or an error naming the failed steps.
fn verdict(stdout: &str) -> Result<String> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .context("smoke script printed nothing")?;
    let v: Value = serde_json::from_str(line).context("smoke script output is not JSON")?;
    let mode = v.get("mode").and_then(Value::as_str).unwrap_or("?");
    let steps = v
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let summary = |ok: bool| -> Vec<String> {
        steps
            .iter()
            .filter(|s| s.get("ok").and_then(Value::as_bool) == Some(ok))
            .map(|s| {
                format!(
                    "{}: {}",
                    s.get("step").and_then(Value::as_str).unwrap_or("?"),
                    s.get("detail").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect()
    };
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        let passed = summary(true);
        let note = if mode == "auth-only" {
            " (set WYLDE_OPENAI_SMOKE_TOKEN for the full client flow)"
        } else {
            ""
        };
        Ok(format!("{mode}: {}{note}", passed.join("; ")))
    } else {
        bail!("{mode}: {}", summary(false).join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passing_auth_only_verdict_says_how_to_run_the_full_flow() {
        let out = "noise\n{\"ok\": true, \"mode\": \"auth-only\", \"steps\": [{\"step\": \"auth-gate\", \"ok\": true, \"detail\": \"401 invalid_api_key\"}]}\n";
        let d = verdict(out).unwrap();
        assert!(d.starts_with("auth-only: auth-gate: 401"), "{d}");
        assert!(d.contains("WYLDE_OPENAI_SMOKE_TOKEN"));
    }

    #[test]
    fn a_failing_full_verdict_names_only_the_failed_steps() {
        let out = r#"{"ok": false, "mode": "full", "steps": [
            {"step": "models", "ok": true, "detail": "4 models"},
            {"step": "fim", "ok": false, "detail": "400 fim_unsupported"}]}"#
            .replace('\n', "");
        let e = verdict(&out).unwrap_err().to_string();
        assert_eq!(e, "full: fim: 400 fim_unsupported");
    }

    #[test]
    fn empty_or_garbled_output_fails_closed() {
        assert!(verdict("").is_err());
        assert!(verdict("Traceback (most recent call last):").is_err());
    }
}
