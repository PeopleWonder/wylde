//! wylde-n8n service entry point.
//!
//! In **managed mode** (the default) the lifecycle daemon launches the
//! local n8n engine as its own service (`wylde-n8n-engine`) and this
//! process attaches to it, delivering the maintainer's three requirements:
//!
//!   1. **Embed** — n8n binds loopback-only at `http://127.0.0.1:5678`,
//!      the URL the Wylde GUI's n8n panel embeds.
//!   2. **Shared auth** — lifecycle mints the Wylde-owned identity in the
//!      out-of-tree data dir; this service provisions the n8n owner
//!      account from it and hands the GUI an auto-login script via
//!      `n8n.editor_bootstrap`, so the user never logs into n8n.
//!   3. **Persistence** — the engine's `N8N_USER_FOLDER` is the durable
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
use wylde_n8n::runtime::RuntimeConfig;
use wylde_n8n::secret::N8nIdentity;

const SERVICE_NAME: &str = "wylde-n8n";

/// How long to wait for n8n to finish migrations and bind the port.
const READY_DEADLINE: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-n8n: starting (rust impl)");

    // Attach to the lifecycle-managed n8n engine BEFORE the config is
    // first read: managed mode derives the upstream URL + owner
    // credentials from the Wylde-owned identity and exports them so the
    // shared action client (and the `n8n.*` verbs) authenticate as the
    // owner with no hand-wiring.
    let managed = attach_managed_n8n();

    let cfg = wylde_n8n::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        None,
        "optional",
        "N8N workflow service — attaches to the lifecycle-managed local n8n \
         engine (loopback editor + out-of-tree persistence), provisions its \
         owner for shared auth, and fronts it as n8n.* pipe actions. Core \
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
                "managed": managed,
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

    // The engine is the lifecycle daemon's to stop (`wylde-n8n-engine`);
    // this service only drops its attachment.
    wylde_n8n::service::stop();
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-n8n: mark_stopped failed: {e}");
    }
    Ok(())
}

/// Managed-mode startup: attach to the lifecycle-launched engine. Returns
/// whether managed mode is active (an identity was found to attach with).
///
/// Every failure is non-fatal: the service still boots and serves the
/// pipe; calls degrade to structured errors until n8n is up. This keeps
/// the "core works without the service" contract intact.
fn attach_managed_n8n() -> bool {
    let wylde_root = std::env::var_os("WYLDE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let rt = RuntimeConfig::from_env(&wylde_root);
    if !rt.managed {
        tracing::info!("wylde-n8n: WYLDE_N8N_MANAGED=0 — legacy proxy mode");
        return false;
    }

    // Read the Wylde-owned identity (encryption key + owner creds) the
    // lifecycle daemon minted when it launched the engine, then export
    // the upstream URL + owner credentials so Config (read just after)
    // wires the action client to authenticate as the owner.
    let identity = match N8nIdentity::load(&rt.data_dir) {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::warn!(
                "wylde-n8n: no n8n identity in {} — the lifecycle daemon mints it when \
                 it launches wylde-n8n-engine; falling back to unmanaged proxy mode",
                rt.data_dir.display()
            );
            return false;
        }
        Err(e) => {
            tracing::warn!(
                "wylde-n8n: could not read the n8n identity ({e:#}); falling back to \
                 unmanaged proxy mode"
            );
            return false;
        }
    };
    std::env::set_var("WYLDE_N8N_URL", rt.base_url());
    std::env::set_var("WYLDE_N8N_EMAIL", &identity.owner_email);
    std::env::set_var("WYLDE_N8N_PASSWORD", &identity.owner_password);

    // Install the editor context so n8n.editor_bootstrap can mint the
    // auto-login script.
    editor::set_context(EditorContext {
        base_url: rt.base_url(),
        identity: identity.clone(),
    });

    // Provision the owner account in the background so the pipe surface
    // opens immediately — n8n.* calls that arrive before the engine
    // finishes booting get the usual fail-soft error envelope, exactly like
    // the wylde-ollama "daemon may come up later" contract. Provisioning is
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

    true
}
