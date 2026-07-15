//! Integration tests for the Phase 2.B action surface.
//!
//! Lives in `tests/` so it links against the crate's public API exactly
//! as a downstream caller would.
//!
//! Tests deliberately do NOT spin up the tunnel data plane (that
//! requires admin + wintun.dll + risks disrupting the host network).
//! Lifecycle correctness for that layer is covered by the unit tests
//! in [`wylde_vpn::tunnel::state`] using the stubbed backend.

// pairing.rs uses a process-wide token table; tests serialise via the
// `test_lock()` mutex held across an `await`. parking_lot::Mutex doesn't
// poison on panic, so holding across await is safe — clippy can't tell.
#![allow(clippy::await_holding_lock)]

use std::sync::OnceLock;

use parking_lot::Mutex;
use serde_json::json;
use tempfile::TempDir;

// pairing.rs uses process-wide state; serialise tests here too so the
// rate-limit + token-table assertions don't see neighbours. parking_lot's
// Mutex doesn't poison on panic, so a single failing test doesn't
// cascade into PoisonError noise on the rest of the file.
fn test_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn link_pair_then_register_round_trips_through_action_layer() {
    let _g = test_lock().lock();
    let pair = wylde_vpn::actions::handle_link_pair(json!({"label": "phone"})).await;
    assert!(pair.ok, "link.pair failed: {pair:?}");
    let token = pair.data["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 27, "urlsafe(20)-shape token");
    assert!(pair.data["uri"]
        .as_str()
        .unwrap()
        .starts_with("wylde://link/pair?token="));
    assert!(pair.data["expires_in"].as_u64().is_some());

    let pubkey = format!("RUST-INTEG-{}", uuid::Uuid::new_v4());
    let reg = wylde_vpn::actions::handle_link_register(json!({
        "token": token,
        "public_key": pubkey,
        "label": "phone",
    }))
    .await;
    assert!(reg.ok, "link.register failed: {reg:?}");
    assert_eq!(reg.data["status"], "ok");
    assert_eq!(reg.data["peer"]["public_key"], pubkey);
    assert!(reg.data["peer"]["tunnel_ip"]
        .as_str()
        .unwrap()
        .starts_with("192.0.2."));
    assert!(reg.data["server_pubkey"].is_string());
    assert!(reg.data["endpoint"].is_string());
}

#[tokio::test]
async fn link_register_with_replayed_token_errors() {
    let _g = test_lock().lock();
    let pair = wylde_vpn::actions::handle_link_pair(json!({"label": "x"})).await;
    let token = pair.data["token"].as_str().unwrap().to_string();
    let pubkey = format!("REPLAY-{}", uuid::Uuid::new_v4());
    let _ = wylde_vpn::actions::handle_link_register(json!({
        "token": token,
        "public_key": pubkey,
    }))
    .await;
    let replay = wylde_vpn::actions::handle_link_register(json!({
        "token": token,
        "public_key": pubkey,
    }))
    .await;
    assert!(!replay.ok);
    assert_eq!(
        replay.error.unwrap().code,
        "pairing_token_invalid_or_expired"
    );
}

#[tokio::test]
async fn link_register_rejects_unknown_token() {
    let _g = test_lock().lock();
    let r = wylde_vpn::actions::handle_link_register(json!({
        "token": "definitely-not-a-real-token",
        "public_key": "key",
    }))
    .await;
    assert!(!r.ok);
    assert_eq!(r.error.unwrap().code, "pairing_token_invalid_or_expired");
}

#[tokio::test]
async fn link_qr_returns_svg_for_issued_token() {
    let _g = test_lock().lock();
    let pair = wylde_vpn::actions::handle_link_pair(json!({"label": "q"})).await;
    let token = pair.data["token"].as_str().unwrap().to_string();
    let qr = wylde_vpn::actions::handle_link_qr(json!({"token": token})).await;
    assert!(qr.ok);
    let svg = qr.data["svg"].as_str().unwrap();
    assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
}

#[tokio::test]
async fn link_config_patch_writes_yaml_and_preserves_other_sections() {
    // Use the path-injected helper so this test doesn't depend on the
    // process-wide Config singleton or `WYLDE_ROOT` ordering.
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(
        &cfg_path,
        "service:\n  port: 8020\nvpn:\n  enabled: false\n  endpoint: ''\nlink:\n  enabled: false\n  listen_port: 51821\n  public_host: ''\ndns_stub:\n  port: 5300\n",
    )
    .unwrap();

    let result = wylde_vpn::config::patch_link_config_at(
        &cfg_path,
        &json!({
            "enabled": true,
            "listen_port": 52000,
            "public_host": "wylde.example.com",
            "relay": {
                "host": "turn.example.com",
                "port": 3478,
            }
        }),
    )
    .expect("patch should succeed");

    // The view echoes the patched values.
    assert_eq!(result.view["enabled"], true);
    assert_eq!(result.view["listen_port"], 52000);
    assert_eq!(result.view["public_host"], "wylde.example.com");
    assert_eq!(result.view["relay"]["host"], "turn.example.com");

    // The YAML on disk preserves sections we don't own and applies the
    // requested values.
    let written = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(written.contains("listen_port: 52000"), "got: {written}");
    assert!(
        written.contains("public_host: wylde.example.com"),
        "got: {written}"
    );
    assert!(written.contains("service:"), "service section preserved");
    assert!(written.contains("vpn:"), "vpn section preserved");
    assert!(written.contains("dns_stub:"), "dns_stub section preserved");
}

#[tokio::test]
async fn link_config_patch_rejects_unknown_fields() {
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "link:\n  enabled: false\n").unwrap();

    let err =
        wylde_vpn::config::patch_link_config_at(&cfg_path, &json!({"totally_made_up_key": 42}))
            .expect_err("should reject unknown field");
    let msg = format!("{err:#}");
    assert!(msg.contains("totally_made_up_key"), "got: {msg}");
}

#[tokio::test]
async fn link_config_patch_coerces_string_bool_to_real_bool() {
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, "link:\n  enabled: false\n").unwrap();

    let r =
        wylde_vpn::config::patch_link_config_at(&cfg_path, &json!({"enabled": "true"})).unwrap();
    assert_eq!(r.view["enabled"], true);
    let written = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(written.contains("enabled: true"));
}
