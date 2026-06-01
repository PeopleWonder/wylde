//! wylde-vpn parity gate — Phase 2 foundation slice.
//!
//! Per `docs/wylde-rust-migration-master-plan.md` §Phase 2 the parity
//! suite for VPN has two halves:
//!
//! 1. **Pure-control parity** (always-on) — exercise the read-only +
//!    pure-crypto actions cross-impl: `vpn.keygen`, `link.peers`,
//!    `link.qr`, `link.status`, `link.config.get`. These don't touch
//!    the network and they don't require root, so they can run on any
//!    host where both binaries are available.
//! 2. **Tunnel-lifecycle parity** (opt-in via `WYLDE_PARITY_VPN_LIVE=1`)
//!    — needs root + a real WireGuard environment. Brings up a wg0/wg1
//!    pair against a loopback peer and asserts transfer counters across
//!    impls. Phase 2 sub-phase 2.B+ will fill this in once boringtun
//!    lands; currently scaffolded as `#[ignore]` placeholders so the
//!    test gate documents the cutover boundary.
//!
//! The Rust crate as it stands in this commit is a FOUNDATION SLICE:
//! `vpn.enable` / `vpn.disable` / `link.pair` / `link.register` /
//! `link.stun` / `link.connect` / `link.config.patch` all return
//! `service_unavailable`. Their parity counterparts are written below
//! as `#[ignore]` tests with clear TODOs naming the sub-phase that
//! unlocks them. This is intentional — it makes the surface that must
//! reach parity explicit and grep-able.
//!
//! Opt-in to live tunnel parity:
//! ```
//! WYLDE_PARITY_VPN_LIVE=1 cargo test --features parity --test vpn
//! ```

#![cfg(feature = "parity")]

use std::process::Command;
use std::time::Duration;

use serde_json::json;
use wylde_parity::{paths, pipe, proc};

const SERVICE: &str = "wylde-vpn";

fn live_mode() -> bool {
    std::env::var("WYLDE_PARITY_VPN_LIVE")
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn spawn_rust_vpn() -> proc::Service {
    let bin = paths::rust_release_bin("wylde-vpn")
        .expect("rust release binary for wylde-vpn must exist; cargo build --release first");
    let cmd = Command::new(bin);
    let svc = proc::Service::spawn("wylde-vpn", cmd).expect("spawn wylde-vpn");
    // Pipe + axum HTTP both bind from main.rs; the pipe is the parity
    // surface so we wait for that.
    let ready = futures::executor::block_on(pipe::wait_ready(
        SERVICE,
        "vpn.keygen",
        Duration::from_secs(15),
    ));
    assert!(ready, "wylde-vpn pipe never became ready");
    svc
}

// ── Pure-control parity (always-on, no network) ────────────────────────

#[tokio::test]
async fn vpn_keygen_returns_a_valid_base64_pair() {
    if !paths::rust_release_bin("wylde-vpn").is_some() {
        eprintln!("SKIP: wylde-vpn release binary not built");
        return;
    }

    let mut svc = spawn_rust_vpn();
    let reply = pipe::capture(SERVICE, "vpn.keygen", json!({})).await;
    assert_eq!(reply["ok"], true, "vpn.keygen failed: {reply:#}");

    use base64::Engine;
    let priv_b64 = reply["data"]["private_key"].as_str().unwrap();
    let pub_b64 = reply["data"]["public_key"].as_str().unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(priv_b64)
            .unwrap()
            .len(),
        32
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(pub_b64)
            .unwrap()
            .len(),
        32
    );

    let _ = svc.stop(Duration::from_secs(10)).await;
}

#[tokio::test]
async fn link_peers_returns_array() {
    if !paths::rust_release_bin("wylde-vpn").is_some() {
        eprintln!("SKIP: wylde-vpn release binary not built");
        return;
    }

    let mut svc = spawn_rust_vpn();
    let reply = pipe::capture(SERVICE, "link.peers", json!({})).await;
    assert_eq!(reply["ok"], true, "link.peers failed: {reply:#}");
    assert!(
        reply["data"]["peers"].is_array(),
        "link.peers missing peers[]: {reply:#}"
    );

    let _ = svc.stop(Duration::from_secs(10)).await;
}

#[tokio::test]
async fn link_qr_renders_svg_for_token() {
    if !paths::rust_release_bin("wylde-vpn").is_some() {
        eprintln!("SKIP: wylde-vpn release binary not built");
        return;
    }

    let mut svc = spawn_rust_vpn();
    let reply = pipe::capture(SERVICE, "link.qr", json!({"token": "parity-test"})).await;
    assert_eq!(reply["ok"], true, "link.qr failed: {reply:#}");
    let svg = reply["data"]["svg"].as_str().unwrap();
    assert!(
        svg.contains("<svg") || svg.contains("<?xml"),
        "link.qr should return SVG: {svg:.80}"
    );
    assert_eq!(reply["data"]["content_type"], "image/svg+xml");

    let _ = svc.stop(Duration::from_secs(10)).await;
}

#[tokio::test]
async fn link_status_envelope_has_expected_keys() {
    if !paths::rust_release_bin("wylde-vpn").is_some() {
        eprintln!("SKIP: wylde-vpn release binary not built");
        return;
    }

    let mut svc = spawn_rust_vpn();
    let reply = pipe::capture(SERVICE, "link.status", json!({})).await;
    assert_eq!(reply["ok"], true, "link.status failed: {reply:#}");
    for key in ["enabled", "listen_port", "peer_count", "phase"] {
        assert!(
            reply["data"].get(key).is_some(),
            "link.status missing {key}: {reply:#}"
        );
    }

    let _ = svc.stop(Duration::from_secs(10)).await;
}

#[tokio::test]
async fn deferred_actions_return_service_unavailable() {
    if !paths::rust_release_bin("wylde-vpn").is_some() {
        eprintln!("SKIP: wylde-vpn release binary not built");
        return;
    }

    let mut svc = spawn_rust_vpn();
    for action in [
        "vpn.enable",
        "vpn.disable",
        "link.pair",
        "link.register",
        "link.stun",
        "link.connect",
        "link.config.patch",
    ] {
        let reply = pipe::capture(SERVICE, action, json!({})).await;
        assert_eq!(
            reply["ok"], false,
            "{action} should still be deferred: {reply:#}"
        );
        assert_eq!(
            reply["error"]["code"], "service_unavailable",
            "{action} returned unexpected error code: {reply:#}"
        );
    }

    let _ = svc.stop(Duration::from_secs(10)).await;
}

// ── Tunnel-lifecycle parity (opt-in, needs root + boringtun) ────────────

#[tokio::test]
#[ignore = "WYLDE_PARITY_VPN_LIVE=1 + root required; boringtun lands in Phase 2 sub-phase 2.B"]
async fn tunnel_lifecycle_parity() {
    if !live_mode() {
        eprintln!("SKIP: WYLDE_PARITY_VPN_LIVE not set");
        return;
    }
    // TODO(Phase 2 sub-phase 2.B): when boringtun + wintun lands,
    //   1. Bring wg0 up via the Rust impl pointing at a loopback peer.
    //   2. Send a handful of UDP packets across the tunnel.
    //   3. Assert tx/rx counters via vpn.status.
    //   4. Tear wg0 down.
    //   5. Repeat against the Python impl (WYLDE_WYLDE_VPN_IMPL=python).
    //   6. Assert tx/rx counters match within ±10%.
    panic!("tunnel-lifecycle parity not yet wired — see master plan §Phase 2");
}

#[tokio::test]
#[ignore = "stun-rs lands in Phase 2 sub-phase 2.C — NAT category parity gate"]
async fn stun_classification_parity() {
    if !live_mode() {
        eprintln!("SKIP: WYLDE_PARITY_VPN_LIVE not set");
        return;
    }
    // TODO(Phase 2 sub-phase 2.C): Risk #11 in the master plan — STUN
    // classification drift between stun-rs and the Python impl. This
    // test fires link.stun against both impls (pointing at a known
    // STUN server) and asserts the returned NAT category strings match
    // byte-for-byte. Cone categories that pair the same way must remain
    // the same across the cutover or the pairing UX silently breaks.
    panic!("STUN classification parity not yet wired — see master plan Risk #11");
}
