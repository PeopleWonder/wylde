//! wylde-vpn service entry point.
//!
//! Boots the manifest, registers the 15 callable actions on the pipe at
//! `\\.\pipe\wylde-vpn`, brings up the axum HTTP control plane on
//! 127.0.0.1:8020 in parallel (port-equivalent with the Python Flask
//! service), and serves until Ctrl-C.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::task::JoinHandle;
use tracing::Level;
use wylde_shared::ipc;
use wylde_shared::ipc::http_routes::{HttpResponse, HttpRouteTable};
use wylde_shared::logging::configure_logging;
use wylde_shared::manifest::ManifestWriter;

const SERVICE_NAME: &str = "wylde-vpn";

#[tokio::main]
async fn main() -> Result<()> {
    configure_logging(Some(SERVICE_NAME), Level::INFO);
    tracing::info!("wylde-vpn: starting (rust foundation slice)");

    let cfg = wylde_vpn::config::Config::get();
    let manifest = ManifestWriter::write(
        SERVICE_NAME,
        Some(cfg.port),
        "optional",
        "WyldeLink — peer-to-peer VPN with WireGuard tunnels (Phase 2 foundation slice, Rust).",
        json!({
            "wylde_vpn": {
                "actions": wylde_vpn::actions::all_action_names(),
                "http_port": cfg.port,
                "link_listen_port": cfg.link_listen_port,
                "vpn_enabled": cfg.vpn_enabled,
                "link_enabled": cfg.link_enabled,
                "impl": "rust-2.D",
                // Phase 2.D wired mdns + ddns + push webhook + handshake
                // monitor. Phase 2.E (Gateway cutover to IPC actions) is
                // the only remaining VPN sub-phase before the
                // strangler-fig flag can flip to rust by default.
                "deferred": [],
                "sub_phases_landed": ["2.A", "2.B", "2.C", "2.D"],
            },
            // Mirror the Python manifest's egress block verbatim so the
            // Gateway egress allowlist still picks up the right keys.
            "egress": [
                {
                    "key": "ddns_duckdns",
                    "url_prefix": "https://www.duckdns.org/update",
                    "purpose": "DDNS update for the home WireGuard endpoint (DuckDNS provider).",
                },
                {
                    "key": "ddns_noip",
                    "url_prefix": "https://dynupdate.no-ip.com/nic/update",
                    "purpose": "DDNS update for the home WireGuard endpoint (No-IP provider).",
                },
                {
                    "key": "ddns_cloudflare",
                    "url_prefix": "https://api.cloudflare.com/client/v4/zones/",
                    "purpose": "DDNS update via Cloudflare DNS API.",
                },
                {
                    "key": "ddns_afraid",
                    "url_prefix": "https://freedns.afraid.org/dynamic/update.php",
                    "purpose": "DDNS update via FreeDNS / Afraid.org.",
                },
                {
                    "key": "stun",
                    "url_prefix": "udp://stun.l.google.com:19302",
                    "purpose": "RFC 5389 STUN binding requests for NAT classification.",
                    "transport": "udp",
                },
                {
                    "key": "turn",
                    "url_prefix": "udp://",
                    "purpose": "RFC 5766 TURN allocation against the operator-supplied coturn relay.",
                    "transport": "udp",
                },
                {
                    "key": "push_webhook",
                    "url_prefix": "https://",
                    "purpose": "Outbound POST to a peer's webhook URL when push_store delivers a notification.",
                    "transport": "http",
                },
                {
                    "key": "wg_peer_endpoint",
                    "url_prefix": "udp://",
                    "purpose": "WireGuard handshake + data plane to the configured outbound VPN endpoint (wg0).",
                    "transport": "udp",
                },
            ],
        }),
        Some("rust:wylde-vpn"),
    )?;
    let _heartbeat = manifest.start_heartbeat(Duration::from_secs(60));

    // Register the 15 actions on the process-wide registry. install()
    // must precede serve() so the registry is populated when the first
    // pipe client connects.
    wylde_vpn::service::install();

    // Write the action contract on disk for `wylde_check` and the
    // cross-language registry. Path resolves to
    // `data/contracts/actions/wylde-vpn.json` under WYLDE_ROOT.
    if let Err(e) = ipc::write_action_contract(SERVICE_NAME, &cfg.wylde_root) {
        tracing::warn!("wylde-vpn: action contract write failed: {e}");
    }

    tracing::info!(
        "wylde-vpn: actions registered ({}); opening pipe at \\\\.\\pipe\\wylde-vpn and HTTP on :{}",
        wylde_vpn::actions::all_action_names().len(),
        cfg.port,
    );

    // ── Phase 2.D background workers ──────────────────────────────────
    //
    // Each side-car gates on its own enable flag and degrades silently
    // (no panics, warn logs only) if the underlying resource isn't
    // available — non-Linux hosts, missing DDNS config, no STUN servers,
    // mDNS port already bound by another process, etc. Wrapped in
    // `Option` so the shutdown path can `take()` them and exits clean
    // even if a worker never started.
    let mdns = start_mdns(cfg);
    let endpoint_updater = start_endpoint_updater(cfg);
    let ddns_handle = start_ddns_scheduler(cfg);
    let health_handle = start_health_monitor(cfg);

    // HTTP-shaped routes over the pipe. The GUI RemoteAccess panel
    // addresses wylde-vpn with `GET /api/link/{status,peers,config,services}`
    // (HTTP-verb + path envelope) rather than the `/__action__` surface —
    // the same shape the Python "Flask-over-pipe" server answered before
    // the cutover. Each handler is a thin envelope over the shared action
    // business-logic fn (`handle_link_*`), so the action verb and the HTTP
    // route can never drift. `serve_with_http_routes` keeps the
    // action/health/handshake paths intact and matches these last.
    let routes = HttpRouteTable::new()
        .route("GET", "/api/link/status", |req| async move {
            HttpResponse::from(wylde_vpn::actions::handle_link_status(req.body).await)
        })
        .route("GET", "/api/link/peers", |req| async move {
            HttpResponse::from(wylde_vpn::actions::handle_link_peers(req.body).await)
        })
        .route("GET", "/api/link/config", |req| async move {
            HttpResponse::from(wylde_vpn::actions::handle_link_config_get(req.body).await)
        })
        .route("GET", "/api/link/services", |req| async move {
            HttpResponse::from(wylde_vpn::actions::handle_link_services(req.body).await)
        });

    // Pipe server + HTTP server run in parallel. Either exiting (or
    // ctrl-c) tears the process down.
    let pipe_fut = ipc::serve_with_http_routes(SERVICE_NAME, None, routes);
    let http_fut = wylde_vpn::http::serve(cfg.port);

    tokio::select! {
        result = pipe_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-vpn: pipe serve() exited with error: {e}");
            } else {
                tracing::info!("wylde-vpn: pipe serve() exited cleanly");
            }
        }
        result = http_fut => {
            if let Err(e) = result {
                tracing::error!("wylde-vpn: HTTP serve() exited with error: {e}");
            } else {
                tracing::info!("wylde-vpn: HTTP serve() exited cleanly");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("wylde-vpn: ctrl-c received, shutting down");
        }
    }

    wylde_vpn::service::stop();
    if let Some(adv) = mdns {
        adv.stop();
    }
    if let Some((updater, handle)) = endpoint_updater {
        updater.stop();
        // Bounded wait so a stuck STUN socket doesn't block shutdown.
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    if let Some((stop_flag, handle)) = ddns_handle {
        stop_flag.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    if let Some((health, handle)) = health_handle {
        health.stop();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    if let Err(e) = manifest.mark_stopped() {
        tracing::warn!("wylde-vpn: mark_stopped failed: {e}");
    }
    Ok(())
}

/// mDNS LAN advertisement. Registers `_wylde-link._udp.local.` if the
/// `discovery.mdns.enabled` YAML key is true (the default).
fn start_mdns(cfg: &wylde_vpn::config::Config) -> Option<wylde_vpn::discovery::mdns::MdnsAdvertiser> {
    if !cfg.mdns_enabled {
        tracing::info!("mdns: disabled via discovery.mdns.enabled = false");
        return None;
    }
    let hostname = std::env::var("COMPUTERNAME")
        .ok()
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "wylde-desktop".to_string());
    let mut service_type = cfg.mdns_service_name.clone();
    if !service_type.ends_with(".local.") {
        if service_type.ends_with(".local") {
            service_type.push('.');
        } else {
            service_type.push_str(".local.");
        }
    }
    let adv = wylde_vpn::discovery::mdns::MdnsAdvertiser::new(
        wylde_vpn::discovery::mdns::MdnsConfig {
            hostname,
            port: cfg.link_listen_port,
            service_type,
            instance_name: cfg.mdns_instance_name.clone(),
            gateway_port: 8021,
            version: "1.0".to_string(),
        },
    );
    if adv.start() {
        Some(adv)
    } else {
        None
    }
}

/// Endpoint poller — periodic STUN probe + change notification. Wires
/// the Phase 2.D real `on_change` callback that fires the push
/// broadcast to every subscribed peer (replaces the 2.C logging-only
/// stub).
fn start_endpoint_updater(
    cfg: &wylde_vpn::config::Config,
) -> Option<(
    wylde_vpn::nat::endpoint_updater::EndpointUpdater,
    JoinHandle<()>,
)> {
    if !cfg.link_enabled || cfg.link_stun_servers.is_empty() {
        return None;
    }
    let history_path = cfg.link_data_dir.join("endpoint-history.json");

    // Capture the runtime handle now so the (synchronous) callback can
    // spawn its async push-broadcast task — `on_change` fires from
    // inside the updater's tokio task, but the callback signature is
    // `Fn(_, _)`, so we need an explicit spawn to call the async store.
    let handle = tokio::runtime::Handle::current();
    let on_change: wylde_vpn::nat::endpoint_updater::OnChange =
        Arc::new(move |previous, current| {
            let prev = previous.map(|s| s.to_string()).unwrap_or_default();
            let curr = current.to_string();
            let store = wylde_vpn::peers::push::shared().clone();
            tracing::info!("endpoint changed: {prev} → {curr} — broadcasting to peers");
            handle.spawn(async move {
                let mut data = serde_json::Map::new();
                data.insert(
                    "type".to_string(),
                    serde_json::Value::String("endpoint_change".to_string()),
                );
                data.insert("previous".to_string(), serde_json::Value::String(prev));
                data.insert(
                    "new_endpoint".to_string(),
                    serde_json::Value::String(curr.clone()),
                );
                let res = store
                    .broadcast(
                        "WyldeLink endpoint changed",
                        &format!("New endpoint: {curr}"),
                        data,
                    )
                    .await;
                tracing::info!(
                    "push: endpoint_change broadcast → {} recipients, {} delivered, {} queued",
                    res.recipients,
                    res.delivered,
                    res.queued,
                );
            });
        });
    let updater = wylde_vpn::nat::endpoint_updater::EndpointUpdater::new(
        cfg.link_stun_servers.clone(),
        Duration::from_secs(300),
        history_path,
        on_change,
    );
    let handle = updater.start();
    Some((updater, handle))
}

/// DDNS scheduler — re-syncs the configured provider with the current
/// public endpoint on a fixed cadence (default 5min, override via
/// `discovery.ddns.update_interval_s`).
///
/// Only runs when DDNS is configured AND link is enabled. Looks up
/// "current public IP" from the endpoint_updater's last STUN result —
/// which means the very first tick may be a no-op if STUN hasn't
/// completed yet. That's fine; the next tick picks it up.
fn start_ddns_scheduler(
    cfg: &wylde_vpn::config::Config,
) -> Option<(Arc<tokio::sync::Notify>, JoinHandle<()>)> {
    if !cfg.ddns_enabled
        || cfg.ddns_provider.trim().is_empty()
        || cfg.ddns_domain.trim().is_empty()
        || cfg.ddns_token.trim().is_empty()
    {
        if cfg.ddns_enabled {
            tracing::warn!(
                "ddns: enabled but provider/domain/token missing — scheduler not starting"
            );
        }
        return None;
    }

    let provider = cfg.ddns_provider.clone();
    let domain = cfg.ddns_domain.clone();
    let token = cfg.ddns_token.clone();
    let extra: BTreeMap<String, String> = cfg.ddns_extra.clone();
    let interval = Duration::from_secs(cfg.ddns_update_interval_s.max(30));
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_for_task = stop.clone();

    let handle = tokio::spawn(async move {
        let client = wylde_vpn::discovery::ddns::ReqwestHttpClient::new();
        tracing::info!(
            "ddns: scheduler online (provider={}, domain={}, interval={}s)",
            provider,
            domain,
            interval.as_secs()
        );
        loop {
            // The STUN poller is the source of truth for the home's
            // public IP. Strip the port from the `ip:port` string the
            // updater records.
            let ip = current_public_ip();
            let ip_ref: Option<&str> = ip.as_deref();
            let res = wylde_vpn::discovery::ddns::update(
                &client, &provider, &domain, &token, ip_ref, &extra,
            )
            .await;
            if res.ok {
                tracing::info!(
                    "ddns: provider={} ok ({})",
                    provider,
                    res.message.chars().take(120).collect::<String>()
                );
            } else {
                tracing::warn!(
                    "ddns: provider={} failed: {}",
                    provider,
                    res.message.chars().take(200).collect::<String>()
                );
            }
            tokio::select! {
                _ = stop_for_task.notified() => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
    Some((stop, handle))
}

/// Look up the home's current public IP from the endpoint history
/// (`endpoint-history.json` — written by the endpoint poller). Strips
/// `:port` to leave just the IP. Returns `None` until the poller has
/// recorded its first probe.
fn current_public_ip() -> Option<String> {
    let cfg = wylde_vpn::config::Config::get();
    let path = cfg.link_data_dir.join("endpoint-history.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
    let latest = entries.last()?;
    let current = latest.get("current")?.as_str()?;
    Some(current.split(':').next().unwrap_or(current).to_string())
}

/// Handshake monitor — polls the active wg1 tunnel + peer store and
/// classifies each peer as online/stale/offline.
fn start_health_monitor(
    cfg: &wylde_vpn::config::Config,
) -> Option<(
    wylde_vpn::monitoring::tunnel_health::TunnelHealth,
    JoinHandle<()>,
)> {
    if !cfg.link_enabled {
        return None;
    }
    let interval = Duration::from_secs(cfg.heartbeat_interval_s.max(5));
    let on_change: wylde_vpn::monitoring::tunnel_health::OnStateChange =
        Arc::new(|peer, prev, curr, age| {
            tracing::info!(
                "tunnel-health: peer={} {} → {} (age={:?})",
                peer,
                prev.as_str(),
                curr.as_str(),
                age,
            );
        });
    let health = wylde_vpn::monitoring::tunnel_health::TunnelHealth::with_settings(
        interval,
        wylde_vpn::monitoring::tunnel_health::DEFAULT_STALE_AFTER_S,
        Some(on_change),
    );
    let handle = health.start();
    Some((health, handle))
}
