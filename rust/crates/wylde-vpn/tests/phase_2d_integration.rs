//! Phase 2.D integration tests — wire-up coverage that crosses module
//! boundaries (endpoint_updater → push.broadcast, link.status →
//! handshake classification).
//!
//! Network-bound calls go through the trait-based stub clients each
//! module exposes (`peers::push::WebhookClient`, `discovery::ddns::HttpClient`),
//! so these tests never open a socket.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tempfile::TempDir;

use wylde_vpn::nat::endpoint_updater::{EndpointUpdater, OnChange};
use wylde_vpn::nat::stun::{StunResult, Transport};
use wylde_vpn::peers::push::{PushStore, WebhookClient};

/// Scripted webhook client — records every POST attempt, replays the
/// scripted boolean result. Used by the push-broadcast wire-up tests.
struct RecordingWebhookClient {
    calls: StdMutex<Vec<(String, Value)>>,
    response: bool,
}

impl RecordingWebhookClient {
    fn new(response: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: StdMutex::new(Vec::new()),
            response,
        })
    }
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

struct RecordingHandle(Arc<RecordingWebhookClient>);

#[async_trait]
impl WebhookClient for RecordingHandle {
    async fn post(&self, url: &str, payload: &Value) -> bool {
        self.0
            .calls
            .lock()
            .unwrap()
            .push((url.to_string(), payload.clone()));
        self.0.response
    }
}

/// STUN transport that returns a fixed sequence of mapped endpoints.
struct ScriptedStun {
    queue: StdMutex<std::collections::VecDeque<Option<StunResult>>>,
}

impl Transport for ScriptedStun {
    fn probe(
        &self,
        server: &str,
        _change_flags: u32,
        _local_port: u16,
        _timeout: Duration,
    ) -> Option<StunResult> {
        let mut next = self.queue.lock().unwrap().pop_front().flatten();
        if let Some(ref mut r) = next {
            r.server = server.to_string();
        }
        next
    }
}

fn stun_result(ip: &str, port: u16) -> StunResult {
    StunResult {
        server: String::new(),
        mapped_ip: ip.to_string(),
        mapped_port: port,
        other_address: None,
        rtt_ms: 1.0,
    }
}

#[tokio::test]
async fn endpoint_change_callback_broadcasts_to_every_webhook_subscription() {
    // ── Arrange: temp data dir, push store with scripted client, two
    // peers subscribed via webhook.
    let dir = TempDir::new().unwrap();
    let client = RecordingWebhookClient::new(true);
    let store = Arc::new(PushStore::with_client(
        dir.path(),
        Box::new(RecordingHandle(client.clone())),
    ));
    store
        .subscribe("peer-A", "webhook", "https://a.example.test/push")
        .unwrap();
    store
        .subscribe("peer-B", "webhook", "https://b.example.test/push")
        .unwrap();

    // ── Arrange: endpoint updater with a STUN that surfaces a change
    // on the second tick, and an on_change callback that broadcasts
    // through our injected store.
    let history_path = dir.path().join("endpoint-history.json");
    let store_for_cb = store.clone();
    let handle = tokio::runtime::Handle::current();
    let on_change: OnChange = Arc::new(move |prev, curr| {
        let prev = prev.map(|s| s.to_string()).unwrap_or_default();
        let curr = curr.to_string();
        let store = store_for_cb.clone();
        // The real callback in main.rs uses a JoinHandle that we drop
        // — same shape here, but the test awaits the result so we can
        // assert on what landed.
        handle.spawn(async move {
            let mut data = Map::new();
            data.insert("type".to_string(), Value::String("endpoint_change".to_string()));
            data.insert("previous".to_string(), Value::String(prev));
            data.insert("new_endpoint".to_string(), Value::String(curr.clone()));
            store
                .broadcast(
                    "WyldeLink endpoint changed",
                    &format!("New endpoint: {curr}"),
                    data,
                )
                .await;
        });
    });

    let scripted = Arc::new(ScriptedStun {
        queue: StdMutex::new(
            vec![
                Some(stun_result("1.1.1.1", 5000)),
                Some(stun_result("9.9.9.9", 6000)), // change!
            ]
            .into(),
        ),
    });
    let updater = EndpointUpdater::with_transport(
        vec!["stun.example:3478".to_string()],
        Duration::from_millis(10),
        history_path,
        on_change,
        scripted,
    );

    // ── Act: drive two ticks (first establishes baseline, second
    // observes the change → fires callback → spawns broadcast).
    updater.tick_once().await;
    updater.tick_once().await;
    // Yield twice so the spawned broadcast finishes before we inspect
    // the recorded calls.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── Assert: the webhook client saw four POSTs (one per peer on
    // each of the two endpoint-observations the updater made — the
    // first observation also counts as a change because the cached
    // state was previously empty). The two we care about are the LAST
    // two — they carry the actual `1.1.1.1:5000 → 9.9.9.9:6000`
    // transition.
    let calls = client.calls();
    assert_eq!(
        calls.len(),
        4,
        "expected two POSTs per change × two subscribers"
    );
    let urls: Vec<&str> = calls.iter().map(|(u, _)| u.as_str()).collect();
    assert!(urls.contains(&"https://a.example.test/push"));
    assert!(urls.contains(&"https://b.example.test/push"));

    let last_two: Vec<&(String, Value)> = calls.iter().rev().take(2).collect();
    for (_url, body) in &last_two {
        assert_eq!(body["title"], "WyldeLink endpoint changed");
        assert!(body["body"].as_str().unwrap().contains("9.9.9.9:6000"));
        assert_eq!(body["data"]["type"], "endpoint_change");
        assert_eq!(body["data"]["new_endpoint"], "9.9.9.9:6000");
        assert_eq!(body["data"]["previous"], "1.1.1.1:5000");
        let peer = body["peer"].as_str().unwrap();
        assert!(peer == "peer-A" || peer == "peer-B");
    }
}

#[tokio::test]
async fn endpoint_change_falls_back_to_queue_when_webhook_fails() {
    // Webhook returns 5xx (scripted false) → broadcast still completes
    // and the payload lands in the per-peer queue so the peer can pick
    // it up via the existing /api/link/push/pending poll endpoint.
    let dir = TempDir::new().unwrap();
    let client = RecordingWebhookClient::new(false);
    let store = Arc::new(PushStore::with_client(
        dir.path(),
        Box::new(RecordingHandle(client.clone())),
    ));
    store
        .subscribe("peer-A", "webhook", "https://a.example.test/push")
        .unwrap();

    let mut data = Map::new();
    data.insert("type".to_string(), Value::String("endpoint_change".to_string()));
    data.insert("new_endpoint".to_string(), Value::String("9.9.9.9:6000".to_string()));
    let res = store
        .broadcast("WyldeLink endpoint changed", "New endpoint: 9.9.9.9:6000", data)
        .await;

    assert_eq!(res.recipients, 1);
    assert_eq!(res.delivered, 0);
    assert_eq!(res.queued, 1);
    // The webhook attempt was made before the fall-back to queue.
    assert_eq!(client.calls().len(), 1);
    // ...and the payload is drainable via the poll API.
    let drained = store.drain_pending("peer-A");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].title, "WyldeLink endpoint changed");
}

#[tokio::test]
async fn link_status_emits_handshakes_array_when_link_enabled() {
    // The Phase 2.D extension to `link.status` always emits the
    // `handshakes` array (possibly empty), even before any peers have
    // ever registered. This guards against the array silently
    // disappearing across refactors.
    let reply = wylde_vpn::actions::handle_link_status(Value::Null).await;
    assert!(reply.ok);
    let arr = reply.data["handshakes"]
        .as_array()
        .expect("handshakes must be an array");
    // Per-peer entry shape: every record carries peer_pubkey + state.
    for entry in arr {
        assert!(entry["peer_pubkey"].is_string());
        let state = entry["state"].as_str().expect("state must be present");
        assert!(
            ["online", "stale", "offline"].contains(&state),
            "unexpected state {state:?}"
        );
    }
    // The new impl tag confirms 2.D landed.
    assert_eq!(reply.data["impl"], "rust-2.D");
}
