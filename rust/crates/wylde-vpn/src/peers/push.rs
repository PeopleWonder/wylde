//! Push-notification subscription + delivery store.
//! Port of `Wylde/VPN/peers/push.py`.
//!
//! A paired peer (mobile app) registers a push endpoint — one of:
//!
//! * `webhook` — the peer's backend receives a POST when a notification
//!   fires. Webhook URL is supplied at subscribe time.
//! * `poll`    — the peer drains `/api/link/push/pending` itself; the
//!   broadcaster only enqueues.
//!
//! Delivery is best-effort: webhook subscriptions get a POST first
//! (`User-Agent: wylde-link-push/2`, 3s timeout) and the payload is
//! queued only if the POST fails. Poll-mode peers always enqueue.
//!
//! Backed by a JSON file at `<LINK_DATA_DIR>/push.json` (byte-equivalent
//! with the Python store, so a strangler-fig flip back doesn't lose
//! queued notifications):
//!
//! ```text
//! {
//!   "subscriptions": { "<pubkey>": {kind, endpoint, subscribed_at} },
//!   "queued":        { "<pubkey>": [ {id, ts, title, body, data}, ... ] }
//! }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Per-peer queue cap — matches `_QUEUE_CAP` in `VPN/peers/push.py`.
pub const QUEUE_CAP: usize = 64;

/// Webhook delivery timeout — matches `_WEBHOOK_TIMEOUT`.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(3);

const USER_AGENT: &str = "wylde-link-push/2";

/// A registered push subscription. `kind` is `webhook` or `poll`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscription {
    pub kind: String,
    pub endpoint: String,
    pub subscribed_at: String,
}

/// One enqueued notification, mirroring the Python payload shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub ts: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

/// Snapshot of one subscription for `list_subscriptions` output.
#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionView {
    pub public_key: String,
    pub kind: String,
    pub endpoint: String,
    pub subscribed_at: String,
    pub queued: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    subscriptions: std::collections::BTreeMap<String, Subscription>,
    #[serde(default)]
    queued: std::collections::BTreeMap<String, Vec<Notification>>,
}

/// Result of a single `notify()` call.
#[derive(Debug, Clone, Serialize)]
pub struct NotifyResult {
    pub delivered: bool,
    pub queued: bool,
    pub id: String,
}

/// Result of a `broadcast()` call — counts across all subscriptions.
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastResult {
    pub delivered: u32,
    pub queued: u32,
    pub recipients: usize,
}

/// Async HTTP delivery primitive used by [`PushStore::notify`]. Pulled
/// behind a trait so tests can stub the network without ever opening a
/// socket.
#[async_trait::async_trait]
pub trait WebhookClient: Send + Sync {
    /// Returns `true` on 2xx, `false` on any failure (4xx, 5xx, timeout,
    /// DNS, TLS — anything). Mirrors Python's catch-all behaviour.
    async fn post(&self, url: &str, payload: &Value) -> bool;
}

/// Default async webhook client — `reqwest` with the timeout + UA the
/// Python implementation uses.
pub struct ReqwestWebhookClient {
    client: reqwest::Client,
}

impl Default for ReqwestWebhookClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestWebhookClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client builder should never fail for static config");
        Self { client }
    }
}

#[async_trait::async_trait]
impl WebhookClient for ReqwestWebhookClient {
    async fn post(&self, url: &str, payload: &Value) -> bool {
        match self.client.post(url).json(payload).send().await {
            Ok(resp) => {
                let s = resp.status();
                s.is_success()
            }
            Err(e) => {
                tracing::debug!("push: webhook {} failed: {e}", url);
                false
            }
        }
    }
}

/// JSON-file-backed push store.
pub struct PushStore {
    path: PathBuf,
    lock: Mutex<()>,
    client: Box<dyn WebhookClient>,
}

/// Process-wide store rooted under `<LINK_DATA_DIR>/push.json`. Lazily
/// initialised on first call so unit tests that don't touch push
/// delivery never hit the filesystem.
pub fn shared() -> &'static std::sync::Arc<PushStore> {
    use std::sync::{Arc, OnceLock};
    static STORE: OnceLock<Arc<PushStore>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(PushStore::new(&crate::config::Config::get().link_data_dir)))
}

impl PushStore {
    /// Construct a store rooted under `<data_dir>/push.json` and using
    /// the default reqwest-based webhook client.
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        Self::with_client(data_dir, Box::new(ReqwestWebhookClient::new()))
    }

    /// Construct with an injectable webhook client — used by unit tests
    /// to avoid touching the network.
    pub fn with_client<P: AsRef<Path>>(
        data_dir: P,
        client: Box<dyn WebhookClient>,
    ) -> Self {
        Self {
            path: data_dir.as_ref().join("push.json"),
            lock: Mutex::new(()),
            client,
        }
    }

    /// Register / replace a peer's push subscription.
    ///
    /// `kind` must be `"webhook"` or `"poll"`; for `webhook` the
    /// `endpoint` URL must be non-empty.
    pub fn subscribe(
        &self,
        public_key: &str,
        kind: &str,
        endpoint: &str,
    ) -> Result<Subscription> {
        if kind != "webhook" && kind != "poll" {
            return Err(anyhow!(r#"kind must be "webhook" or "poll""#));
        }
        if kind == "webhook" && endpoint.is_empty() {
            return Err(anyhow!("endpoint URL required for webhook subscriptions"));
        }
        let sub = Subscription {
            kind: kind.to_string(),
            endpoint: endpoint.to_string(),
            subscribed_at: Utc::now().to_rfc3339(),
        };
        let _g = self.lock.lock().unwrap();
        let mut state = self.load();
        state.subscriptions.insert(public_key.to_string(), sub.clone());
        self.save(&state)?;
        Ok(sub)
    }

    /// Drop a peer's subscription + any queued payloads.
    pub fn unsubscribe(&self, public_key: &str) -> Result<bool> {
        let _g = self.lock.lock().unwrap();
        let mut state = self.load();
        if !state.subscriptions.contains_key(public_key) {
            return Ok(false);
        }
        state.subscriptions.remove(public_key);
        state.queued.remove(public_key);
        self.save(&state)?;
        Ok(true)
    }

    /// Snapshot of every subscription (with queue depth).
    pub fn list_subscriptions(&self) -> Vec<SubscriptionView> {
        let _g = self.lock.lock().unwrap();
        let state = self.load();
        state
            .subscriptions
            .iter()
            .map(|(k, v)| SubscriptionView {
                public_key: k.clone(),
                kind: v.kind.clone(),
                endpoint: v.endpoint.clone(),
                subscribed_at: v.subscribed_at.clone(),
                queued: state.queued.get(k).map(|q| q.len()).unwrap_or(0),
            })
            .collect()
    }

    /// Drain (and return) every queued notification for a peer.
    pub fn drain_pending(&self, public_key: &str) -> Vec<Notification> {
        let _g = self.lock.lock().unwrap();
        let mut state = self.load();
        let items = state.queued.remove(public_key).unwrap_or_default();
        if !items.is_empty() {
            let _ = self.save(&state);
        }
        items
    }

    /// Deliver — or enqueue — a notification for a single peer. Webhook
    /// subs get a POST attempt first; everything else (and webhook
    /// failures) falls through to the queue.
    pub async fn notify(
        &self,
        public_key: &str,
        title: &str,
        body: &str,
        data: Map<String, Value>,
    ) -> NotifyResult {
        let id = random_id_hex(8);
        let ts = Utc::now().to_rfc3339();
        let payload = Notification {
            id: id.clone(),
            ts: ts.clone(),
            title: title.to_string(),
            body: body.to_string(),
            data,
        };

        // Read the subscription (briefly hold the lock) so the webhook
        // POST happens without it.
        let sub_opt = {
            let _g = self.lock.lock().unwrap();
            self.load().subscriptions.get(public_key).cloned()
        };

        if let Some(sub) = sub_opt.as_ref() {
            if sub.kind == "webhook" && !sub.endpoint.is_empty() {
                let mut wire = serde_json::to_value(&payload).unwrap_or(Value::Null);
                if let Value::Object(ref mut obj) = wire {
                    obj.insert("peer".to_string(), Value::String(public_key.to_string()));
                }
                if self.client.post(&sub.endpoint, &wire).await {
                    return NotifyResult {
                        delivered: true,
                        queued: false,
                        id,
                    };
                }
            }
        }

        // Webhook failed (or subscription was poll-mode or absent) — enqueue.
        let _g = self.lock.lock().unwrap();
        let mut state = self.load();
        let q = state
            .queued
            .entry(public_key.to_string())
            .or_default();
        q.push(payload);
        if q.len() > QUEUE_CAP {
            let drop = q.len() - QUEUE_CAP;
            q.drain(..drop);
        }
        if let Err(e) = self.save(&state) {
            tracing::warn!("push: queue save failed for {public_key}: {e}");
        }
        NotifyResult {
            delivered: false,
            queued: true,
            id,
        }
    }

    /// Fire a notification at every subscribed peer.
    pub async fn broadcast(
        &self,
        title: &str,
        body: &str,
        data: Map<String, Value>,
    ) -> BroadcastResult {
        let peers: Vec<String> = {
            let _g = self.lock.lock().unwrap();
            self.load().subscriptions.keys().cloned().collect()
        };
        let mut delivered = 0u32;
        let mut queued = 0u32;
        for pub_key in &peers {
            let r = self.notify(pub_key, title, body, data.clone()).await;
            if r.delivered {
                delivered += 1;
            }
            if r.queued {
                queued += 1;
            }
        }
        BroadcastResult {
            delivered,
            queued,
            recipients: peers.len(),
        }
    }

    // ── persistence ─────────────────────────────────────────────────────

    fn load(&self) -> State {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => State::default(),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("push: create dir {}", parent.display()))?;
        }
        let body = serde_json::to_vec_pretty(state).context("push: serialize")?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &body)
            .with_context(|| format!("push: write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("push: rename to {}", self.path.display()))?;
        Ok(())
    }
}

/// 8 random bytes rendered as 16 lowercase hex chars — matches
/// Python's `secrets.token_hex(8)` shape used in the notification id.
fn random_id_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand_core::OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(n_bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// Captures every POST attempt; returns whatever `responses` says.
    struct ScriptedClient {
        responses: StdMutex<std::collections::VecDeque<bool>>,
        calls: StdMutex<Vec<(String, Value)>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<bool>) -> Arc<Self> {
            Arc::new(Self {
                responses: StdMutex::new(responses.into_iter().collect()),
                calls: StdMutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    struct ScriptedClientHandle(Arc<ScriptedClient>);

    #[async_trait::async_trait]
    impl WebhookClient for ScriptedClientHandle {
        async fn post(&self, url: &str, payload: &Value) -> bool {
            self.0
                .calls
                .lock()
                .unwrap()
                .push((url.to_string(), payload.clone()));
            self.0
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(false)
        }
    }

    fn fresh_store(responses: Vec<bool>) -> (TempDir, PushStore, Arc<ScriptedClient>) {
        let dir = TempDir::new().unwrap();
        let client = ScriptedClient::new(responses);
        let store =
            PushStore::with_client(dir.path(), Box::new(ScriptedClientHandle(client.clone())));
        (dir, store, client)
    }

    #[test]
    fn subscribe_requires_known_kind() {
        let (_dir, store, _) = fresh_store(vec![]);
        let err = store.subscribe("k", "garbage", "https://x").unwrap_err();
        assert!(err.to_string().contains("kind"));
    }

    #[test]
    fn webhook_subscribe_requires_endpoint() {
        let (_dir, store, _) = fresh_store(vec![]);
        let err = store.subscribe("k", "webhook", "").unwrap_err();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn poll_subscribe_round_trips_with_empty_endpoint() {
        let (_dir, store, _) = fresh_store(vec![]);
        let sub = store.subscribe("k", "poll", "").unwrap();
        assert_eq!(sub.kind, "poll");
        let subs = store.list_subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].public_key, "k");
        assert_eq!(subs[0].queued, 0);
    }

    #[tokio::test]
    async fn notify_webhook_delivers_when_post_ok() {
        let (_dir, store, client) = fresh_store(vec![true]);
        store
            .subscribe("peer1", "webhook", "https://wh.example/p")
            .unwrap();
        let r = store
            .notify("peer1", "hello", "body", Map::new())
            .await;
        assert!(r.delivered);
        assert!(!r.queued);

        let calls = client.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "https://wh.example/p");
        // Payload should include peer + id + ts + title + body.
        let body = &calls[0].1;
        assert_eq!(body["peer"], "peer1");
        assert_eq!(body["title"], "hello");
        assert!(body["id"].as_str().unwrap().len() == 16);
    }

    #[tokio::test]
    async fn notify_webhook_falls_back_to_queue_on_failure() {
        let (_dir, store, _) = fresh_store(vec![false]);
        store
            .subscribe("peer1", "webhook", "https://wh.example/p")
            .unwrap();
        let r = store
            .notify("peer1", "hello", "body", Map::new())
            .await;
        assert!(!r.delivered);
        assert!(r.queued);

        let drained = store.drain_pending("peer1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].title, "hello");
        // Drain is destructive.
        assert!(store.drain_pending("peer1").is_empty());
    }

    #[tokio::test]
    async fn notify_poll_subscription_always_queues() {
        let (_dir, store, client) = fresh_store(vec![]);
        store.subscribe("peer1", "poll", "").unwrap();
        let r = store
            .notify("peer1", "hi", "hi", Map::new())
            .await;
        assert!(!r.delivered);
        assert!(r.queued);
        // Webhook client must not have been called.
        assert!(client.calls().is_empty());
    }

    #[tokio::test]
    async fn notify_without_subscription_still_queues() {
        // Mirrors Python: unknown peer just enqueues into push.json
        // under that pubkey — the queue is the source of truth for
        // pending notifications regardless of subscription state.
        let (_dir, store, _) = fresh_store(vec![]);
        let r = store.notify("ghost", "t", "b", Map::new()).await;
        assert!(!r.delivered);
        assert!(r.queued);
        assert_eq!(store.drain_pending("ghost").len(), 1);
    }

    #[tokio::test]
    async fn queue_caps_at_64_per_peer() {
        let (_dir, store, _) = fresh_store(vec![]);
        store.subscribe("p", "poll", "").unwrap();
        for i in 0..70 {
            store
                .notify("p", &format!("t{i}"), "", Map::new())
                .await;
        }
        let q = store.drain_pending("p");
        assert_eq!(q.len(), QUEUE_CAP);
        // Oldest 6 are dropped — newest survive.
        assert_eq!(q.first().unwrap().title, "t6");
        assert_eq!(q.last().unwrap().title, "t69");
    }

    #[tokio::test]
    async fn broadcast_visits_every_subscription_once() {
        // Two subscribers, both webhook, first delivers, second queues.
        let (_dir, store, client) = fresh_store(vec![true, false]);
        store
            .subscribe("a", "webhook", "https://a.example/")
            .unwrap();
        store
            .subscribe("b", "webhook", "https://b.example/")
            .unwrap();
        let r = store
            .broadcast("title", "body", Map::new())
            .await;
        assert_eq!(r.recipients, 2);
        assert_eq!(r.delivered, 1);
        assert_eq!(r.queued, 1);
        assert_eq!(client.calls().len(), 2);
    }

    #[test]
    fn unsubscribe_drops_subscription_and_queue() {
        let (_dir, store, _) = fresh_store(vec![]);
        store.subscribe("k", "poll", "").unwrap();
        assert!(store.unsubscribe("k").unwrap());
        assert!(!store.unsubscribe("k").unwrap());
        assert!(store.list_subscriptions().is_empty());
    }
}
