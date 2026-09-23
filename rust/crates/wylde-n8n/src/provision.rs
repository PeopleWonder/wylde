//! One-shot startup provisioning of the managed n8n daemon.
//!
//! After [`crate::runtime::spawn`] launches n8n, this module:
//!   1. waits for the daemon's REST surface to answer (`/rest/settings`),
//!   2. provisions the Wylde-owned owner account on first run
//!      (`POST /rest/owner/setup`) so the editor has an identity to log
//!      in as — the user never sees n8n's setup wizard.
//!
//! It uses its own short-lived `reqwest` client rather than the
//! steady-state [`crate::client::N8nClient`]: provisioning is a
//! once-per-lifetime bootstrap with different endpoints, and keeping it
//! separate leaves the action client's contract untouched.
//!
//! Idempotent: on every restart after the first, `showSetupOnFirstLoad`
//! is already false, so we skip setup and return [`Provisioned::Existing`].

use std::time::Duration;

use serde_json::json;

use crate::secret::N8nIdentity;

/// Outcome of [`ensure_ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioned {
    /// Owner account was created this run.
    Created,
    /// Owner account already existed (a restart against a populated DB).
    Existing,
    /// The daemon never became reachable within the deadline.
    Unreachable,
    /// The daemon answered but owner setup failed (left to logs; the
    /// editor falls back to its own login screen).
    SetupFailed(String),
}

/// Poll `GET {base}/rest/settings` until it answers `200`, then ensure
/// the owner account exists. `deadline` bounds the wait for the daemon
/// to finish its DB migrations and bind the port.
pub async fn ensure_ready(
    base_url: &str,
    identity: &N8nIdentity,
    deadline: Duration,
) -> Provisioned {
    let http = match reqwest::Client::builder().cookie_store(true).build() {
        Ok(c) => c,
        Err(e) => return Provisioned::SetupFailed(format!("client build: {e}")),
    };

    let settings = match wait_for_settings(&http, base_url, deadline).await {
        Some(v) => v,
        None => return Provisioned::Unreachable,
    };

    if !needs_owner_setup(&settings) {
        tracing::info!("wylde-n8n: n8n owner already provisioned (restart path)");
        return Provisioned::Existing;
    }

    match setup_owner(&http, base_url, identity).await {
        Ok(()) => {
            tracing::info!("wylde-n8n: provisioned the Wylde-owned n8n owner account");
            Provisioned::Created
        }
        Err(e) => {
            tracing::warn!("wylde-n8n: owner setup failed: {e}");
            Provisioned::SetupFailed(e)
        }
    }
}

/// Poll `/rest/settings` until a 200 lands or the deadline passes.
/// Returns the parsed JSON body on success.
async fn wait_for_settings(
    http: &reqwest::Client,
    base_url: &str,
    deadline: Duration,
) -> Option<serde_json::Value> {
    let start = std::time::Instant::now();
    let url = format!("{base_url}/rest/settings");
    let mut attempt: u32 = 0;
    while start.elapsed() < deadline {
        attempt += 1;
        if let Ok(r) = http.get(&url).timeout(Duration::from_secs(3)).send().await {
            if r.status().is_success() {
                // During boot n8n's catch-all serves the SPA's index.html
                // (200, but HTML) for /rest/settings before the REST
                // routes register. Only accept a body that parses AND
                // carries the settings shape — otherwise keep waiting.
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    if v.get("data")
                        .and_then(|d| d.get("userManagement"))
                        .is_some()
                    {
                        return Some(v);
                    }
                }
            }
        }
        if attempt == 1 {
            tracing::info!("wylde-n8n: waiting for n8n REST surface at {url}");
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
    None
}

/// True when n8n still wants an owner created
/// (`data.userManagement.showSetupOnFirstLoad`).
fn needs_owner_setup(settings: &serde_json::Value) -> bool {
    settings
        .get("data")
        .and_then(|d| d.get("userManagement"))
        .and_then(|u| u.get("showSetupOnFirstLoad"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// `POST /rest/owner/setup` with the Wylde-owned identity. n8n returns
/// 200 + the owner user on success.
async fn setup_owner(
    http: &reqwest::Client,
    base_url: &str,
    identity: &N8nIdentity,
) -> Result<(), String> {
    let url = format!("{base_url}/rest/owner/setup");
    let resp = http
        .post(&url)
        .header("browser-id", "wylde-n8n-provision")
        .json(&json!({
            "email": identity.owner_email,
            "firstName": "Wylde",
            "lastName": "Owner",
            "password": identity.owner_password,
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("owner setup transport: {e}"))?;
    let status = resp.status().as_u16();
    if status == 200 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    let excerpt: String = body.chars().take(200).collect();
    Err(format!("owner setup returned {status}: {excerpt}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_owner_setup_reads_the_flag() {
        let yes = json!({"data": {"userManagement": {"showSetupOnFirstLoad": true}}});
        let no = json!({"data": {"userManagement": {"showSetupOnFirstLoad": false}}});
        let missing = json!({"data": {"userManagement": {}}});
        assert!(needs_owner_setup(&yes));
        assert!(!needs_owner_setup(&no));
        // Absent flag = treat as already-provisioned (don't re-run setup
        // against a daemon we can't read the state of).
        assert!(!needs_owner_setup(&missing));
    }
}
