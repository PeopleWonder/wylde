//! wylde-n8n service entry point.
//!
//! In **managed mode** (the default) this process *owns* the local n8n
//! workflow engine end-to-end, delivering the maintainer's three requirements:
//!
//!   1. **Embed** — n8n binds loopback-only at `http://127.0.0.1:5678`,
//!      the URL the Wylde GUI mounts in a `wry` WebView panel.
//!   2. **Shared auth** — Wylde provisions and owns the n8n owner
//!      account (a Wylde-generated identity in the out-of-tree data
//!      dir) and hands the GUI an auto-login script via
//!      `n8n.editor_bootstrap`, so the user never logs into n8n.
//!   3. **Persistence** — n8n's `N8N_USER_FOLDER` points at the durable
//!      `WyldeData/n8n/` data dir and its encryption key is pinned to
//!      the Wylde-owned identity, so workflows, credentials, and
//!      executions survive restarts.
//!
//! It also fronts the n8n REST API as eight `n8n.*` pipe actions plus
//! `n8n.editor_bootstrap` on `\\.\pipe\wylde-n8n`.
//!
//! Set `WYLDE_N8N_MANAGED=0` to revert to the legacy behaviour: a pure
//! pipe proxy in front of an external, user-managed n8n daemon. Either
//! way an unreachable daemon degrades calls to structured error
//! envelopes, never a crash — core works with or without n8n.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

use wylde_n8n::editor::{self, EditorContext};
use wylde_n8n::provision;
use wylde_n8n::runtime::{self, RuntimeConfig};
use wylde_n8n::secret::N8nIdentity;

const SERVICE_NAME: &str = "wylde-n8n";

/// How long to wait for n8n to finish migrations and bind the port.
const READY_DEADLINE: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-n8n: starting (rust impl)");

    // Bring up (or attach to) the managed n8n daemon BEFORE the config
    // is first read: managed mode derives the upstream URL + owner
    // credentials from the Wylde-owned identity and exports them so the
    // shared action client (and the `n8n.*` verbs) authenticate as the
    // owner with no hand-wiring.
    let mut n8n_child = bring_up_managed_n8n().await;

    let cfg = wylde_n8n::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "optional",
        "N8N workflow service — Wylde-managed local n8n engine. Owns the \
         daemon (loopback editor + out-of-tree persistence + provisioned \
         owner for shared auth) and fronts it as n8n.* pipe actions. Core \
         works with or without it.",
        json!({
            "wylde_n8n": {
                "actions": [
                    "n8n.health",
                    "n8n.list_workflows",
                    "n8n.get_workflow",
                    "n8n.get_execution",
                    "n8n.execute_workflow",
                    "n8n.create_workflow",
                    "n8n.edit_workflow",
                    "n8n.delete_workflow",
                    "n8n.editor_bootstrap",
                ],
                "upstream_url": cfg.auth.url.clone(),
                "auth_configured": cfg.auth.auth_ready(),
                "managed": n8n_child.is_some(),
                // Data/template home per the registry convention — the
                // service folder keeps workflow templates only; the live
                // database lives in WYLDE_N8N_DATA_DIR.
                "data_home": "N8N/workflow_templates",
            },
        }),
        Some("rust:wylde-n8n"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register the actions on the process-wide registry. install() must
    // precede serve() so the registry is populated when the first pipe
    // client connects.
    wylde_n8n::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry.
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-n8n: action contract write failed: {e}");
    }

    tracing::info!("wylde-n8n: actions registered; opening pipe at \\\\.\\pipe\\wylde-n8n");

    let serve_fut = ipc::serve(SERVICE_NAME, None);
    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-n8n: serve() exited with error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-n8n: ctrl-c received, shutting down");
        }
    }

    // Tear down the managed daemon we spawned (no-op in unmanaged /
    // attached mode). kill_on_drop is the backstop; this is the graceful
    // path on a clean exit.
    if let Some(child) = n8n_child.as_mut() {
        tracing::info!("wylde-n8n: stopping managed n8n daemon");
        if let Err(e) = child.start_kill() {
            tracing::warn!("wylde-n8n: could not signal the managed n8n daemon to stop: {e}");
        }
        if let Err(e) = child.wait().await {
            tracing::warn!("wylde-n8n: waiting for the managed n8n daemon to exit failed: {e}");
        }
    }

    wylde_n8n::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-n8n: mark_stopped failed: {e}");
    }
    Ok(())
}

/// Managed-mode startup. Returns the spawned n8n `Child` (so the caller
/// can stop it on shutdown), or `None` in unmanaged mode or when we
/// attached to an already-running daemon.
///
/// Every failure is non-fatal: the service still boots and serves the
/// pipe; calls degrade to structured errors until n8n is up. This keeps
/// the "core works without the service" contract intact.
async fn bring_up_managed_n8n() -> Option<tokio::process::Child> {
    let wylde_root = std::env::var_os("WYLDE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let rt = RuntimeConfig::from_env(&wylde_root);
    if !rt.managed {
        tracing::info!("wylde-n8n: WYLDE_N8N_MANAGED=0 — legacy proxy mode, not launching n8n");
        return None;
    }

    // Load/mint the Wylde-owned identity (encryption key + owner creds)
    // in the durable data dir, then export the upstream URL + owner
    // credentials so Config (read just after) wires the action client to
    // authenticate as the owner.
    let identity = match N8nIdentity::load_or_create(&rt.data_dir) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                "wylde-n8n: could not load/create n8n identity ({e}); \
                 falling back to unmanaged proxy mode"
            );
            return None;
        }
    };
    std::env::set_var("WYLDE_N8N_URL", rt.base_url());
    std::env::set_var("WYLDE_N8N_EMAIL", &identity.owner_email);
    std::env::set_var("WYLDE_N8N_PASSWORD", &identity.owner_password);

    // Install the editor context so n8n.editor_bootstrap can mint the
    // auto-login script regardless of whether we spawn or attach.
    editor::set_context(EditorContext {
        base_url: rt.base_url(),
        identity: identity.clone(),
    });

    // If a daemon is already answering on the port, attach to it rather
    // than spawning a second one (the user may have started n8n by hand).
    let child = if is_reachable(&rt.base_url()).await {
        tracing::info!(
            "wylde-n8n: a daemon already answers on {} — attaching without spawning",
            rt.base_url()
        );
        None
    } else {
        match runtime::resolve_launch() {
            Ok(spec) => match runtime::spawn(&rt, &identity, &spec) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        "wylde-n8n: failed to spawn n8n ({e}); calls degrade to \
                         errors until a daemon is reachable"
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    "wylde-n8n: cannot launch n8n ({e}); calls degrade to errors \
                     until a daemon is reachable at {}",
                    rt.base_url()
                );
                None
            }
        }
    };

    // Provision the owner account in the background so the pipe surface
    // opens immediately — n8n.* calls that arrive before n8n finishes
    // booting get the usual fail-soft error envelope, exactly like the
    // wylde-ollama "daemon may come up later" contract. Provisioning is
    // idempotent across restarts (owner already present → no-op).
    let base = rt.base_url();
    tokio::spawn(async move {
        match provision::ensure_ready(&base, &identity, READY_DEADLINE).await {
            provision::Provisioned::Created => {
                tracing::info!("wylde-n8n: managed n8n ready (owner provisioned)")
            }
            provision::Provisioned::Existing => {
                tracing::info!("wylde-n8n: managed n8n ready (owner already present)")
            }
            provision::Provisioned::Unreachable => tracing::warn!(
                "wylde-n8n: n8n did not become reachable within {}s — calls degrade \
                 to errors until it is",
                READY_DEADLINE.as_secs()
            ),
            provision::Provisioned::SetupFailed(e) => {
                tracing::warn!("wylde-n8n: owner provisioning failed: {e}")
            }
        }
    });

    child
}

/// Quick liveness check: does `GET {base}/healthz` answer at all?
async fn is_reachable(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(format!("{base_url}/healthz"))
        .send()
        .await
        .map(|r| r.status().is_success() || r.status().is_client_error())
        .unwrap_or(false)
}
