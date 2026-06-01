//! DDNS WAN updates — port of `VPN/discovery/ddns.py`.
//!
//! Four providers (dispatched on the `provider` string, case-insensitive):
//!
//! * **duckdns** — `https://www.duckdns.org/update?domains=NAME&token=&ip=`
//! * **noip** — `https://dynupdate.no-ip.com/nic/update` + HTTP Basic Auth
//!   (`token` is `user:pass`)
//! * **cloudflare** — Cloudflare API v4: optional GET to look up the A
//!   record id, then PUT `{type:A, name, content:IP, ttl:60,
//!   proxied:false}`. Bearer-token auth. Needs `extra.zone_id` (and
//!   optionally `extra.record_id`).
//! * **afraid** — `https://freedns.afraid.org/dynamic/update.php?TOKEN&IP`
//!
//! Every call has a 10s timeout. Returns the same `UpdateResult` shape
//! across providers so callers don't branch on success criteria.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const TIMEOUT: Duration = Duration::from_secs(10);
const NOIP_USER_AGENT: &str = "WyldeLink/1.0 wylde@local";

/// Outcome of a single provider call.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub message: String,
    pub status: Option<u16>,
}

impl UpdateResult {
    pub fn fail(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: msg.into(),
            status: None,
        }
    }
}

/// Configuration block for one DDNS provider. Mirrors the YAML keys
/// the discovery section uses (`provider`, `domain`, `token`, `extra`,
/// `enabled`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DdnsProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// Trait pulled out so tests can stub the HTTP layer.
#[async_trait::async_trait]
pub trait HttpClient: Send + Sync {
    /// GET — returns (status_code, body_text). Errors collapse to
    /// `UpdateResult::fail`; implementations return `Err(String)` on
    /// transport-level failure.
    async fn get(
        &self,
        url: &str,
        basic_auth: Option<(&str, &str)>,
        headers: &[(&str, &str)],
    ) -> Result<(u16, String), String>;
    /// PUT with a JSON body — same return contract as `get`.
    async fn put_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        headers: &[(&str, &str)],
    ) -> Result<(u16, String), String>;
}

/// Default reqwest-backed HTTP client.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("reqwest client builder should not fail for static config");
        Self { client }
    }
}

#[async_trait::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(
        &self,
        url: &str,
        basic_auth: Option<(&str, &str)>,
        headers: &[(&str, &str)],
    ) -> Result<(u16, String), String> {
        let mut req = self.client.get(url);
        if let Some((u, p)) = basic_auth {
            req = req.basic_auth(u, Some(p));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                Ok((status, text))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn put_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        headers: &[(&str, &str)],
    ) -> Result<(u16, String), String> {
        let mut req = self.client.put(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                Ok((status, text))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Dispatch the request to the right provider. Returns a fail-shaped
/// result for unknown providers (no panics — matches Python).
pub async fn update(
    client: &dyn HttpClient,
    provider: &str,
    domain: &str,
    token: &str,
    ip: Option<&str>,
    extra: &BTreeMap<String, String>,
) -> UpdateResult {
    match provider.trim().to_ascii_lowercase().as_str() {
        "duckdns" => duckdns(client, domain, token, ip).await,
        "noip" => noip(client, domain, token, ip).await,
        "cloudflare" => cloudflare(client, domain, token, ip, extra).await,
        "afraid" => afraid(client, token, ip).await,
        other => UpdateResult::fail(format!("unknown provider: {other}")),
    }
}

async fn duckdns(
    client: &dyn HttpClient,
    domain: &str,
    token: &str,
    ip: Option<&str>,
) -> UpdateResult {
    let name = domain.split('.').next().unwrap_or(domain);
    let mut url = format!(
        "https://www.duckdns.org/update?domains={}&token={}",
        urlencode(name),
        urlencode(token)
    );
    if let Some(ip) = ip {
        url.push_str(&format!("&ip={}", urlencode(ip)));
    }
    match client.get(&url, None, &[]).await {
        Ok((status, body)) => {
            let trimmed = body.trim();
            UpdateResult {
                ok: trimmed == "OK",
                message: trimmed.to_string(),
                status: Some(status),
            }
        }
        Err(e) => UpdateResult::fail(e),
    }
}

async fn noip(
    client: &dyn HttpClient,
    domain: &str,
    token: &str,
    ip: Option<&str>,
) -> UpdateResult {
    let Some((user, pass)) = token.split_once(':') else {
        return UpdateResult::fail(r#"noip token must be "user:pass""#);
    };
    let mut url = format!(
        "https://dynupdate.no-ip.com/nic/update?hostname={}",
        urlencode(domain)
    );
    if let Some(ip) = ip {
        url.push_str(&format!("&myip={}", urlencode(ip)));
    }
    let headers = [("User-Agent", NOIP_USER_AGENT)];
    match client.get(&url, Some((user, pass)), &headers).await {
        Ok((status, body)) => {
            let trimmed = body.trim();
            let ok =
                trimmed.starts_with("good ") || trimmed.starts_with("nochg ");
            UpdateResult {
                ok,
                message: trimmed.to_string(),
                status: Some(status),
            }
        }
        Err(e) => UpdateResult::fail(e),
    }
}

async fn cloudflare(
    client: &dyn HttpClient,
    domain: &str,
    token: &str,
    ip_in: Option<&str>,
    extra: &BTreeMap<String, String>,
) -> UpdateResult {
    let zone_id = match extra.get("zone_id") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return UpdateResult::fail("cloudflare requires extra.zone_id"),
    };
    let bearer = format!("Bearer {token}");
    let headers = [
        ("Authorization", bearer.as_str()),
        ("Content-Type", "application/json"),
    ];
    let base = format!(
        "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
        urlencode(&zone_id)
    );

    let mut record_id = extra.get("record_id").cloned().unwrap_or_default();
    let mut ip_owned: Option<String> = ip_in.map(|s| s.to_string());

    if record_id.is_empty() {
        let lookup_url = format!(
            "{}?name={}&type=A",
            base,
            urlencode(domain)
        );
        let (status, body) = match client.get(&lookup_url, None, &headers).await {
            Ok(t) => t,
            Err(e) => return UpdateResult::fail(e),
        };
        if !(200..300).contains(&status) {
            return UpdateResult::fail(format!("cloudflare lookup failed: {body}"));
        }
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or(serde_json::Value::Null);
        let results = parsed
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if results.is_empty() {
            return UpdateResult::fail(format!("no A record for {domain}"));
        }
        record_id = results[0]
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if ip_owned.is_none() {
            ip_owned = results[0]
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }

    let Some(ip) = ip_owned else {
        return UpdateResult::fail("no IP supplied and lookup failed");
    };

    let put_url = format!("{base}/{}", urlencode(&record_id));
    let payload = serde_json::json!({
        "type": "A",
        "name": domain,
        "content": ip,
        "ttl": 60,
        "proxied": false,
    });
    match client.put_json(&put_url, &payload, &headers).await {
        Ok((status, body)) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let ok = parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            UpdateResult {
                ok,
                message: body,
                status: Some(status),
            }
        }
        Err(e) => UpdateResult::fail(e),
    }
}

async fn afraid(
    client: &dyn HttpClient,
    token: &str,
    ip: Option<&str>,
) -> UpdateResult {
    // Afraid.org passes the token + IP as positional query-string
    // tokens, NOT as standard key=value pairs, so we don't urlencode
    // the `?TOKEN&IP` segments.
    let mut url = format!("https://freedns.afraid.org/dynamic/update.php?{token}");
    if let Some(ip) = ip {
        url.push('&');
        url.push_str(ip);
    }
    match client.get(&url, None, &[]).await {
        Ok((status, body)) => {
            let trimmed = body.trim();
            let lower = trimmed.to_ascii_lowercase();
            let ok = lower.contains("updated") || lower.contains("no ip change");
            UpdateResult {
                ok,
                message: trimmed.to_string(),
                status: Some(status),
            }
        }
        Err(e) => UpdateResult::fail(e),
    }
}

/// Minimal URL-encoder for query-string values. Reqwest's
/// `RequestBuilder::query` is the usual route, but we build URLs as
/// strings so the trait stays HTTP-method-shaped (and the tests can
/// match exact URL contents).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Headers = Vec<(String, String)>;
    type BasicAuth = Option<(String, String)>;
    type GetCall = (String, BasicAuth, Headers);
    type PutCall = (String, serde_json::Value, Headers);

    /// HTTP client that returns scripted responses + records every call.
    #[derive(Default)]
    struct Scripted {
        get_responses: Mutex<std::collections::VecDeque<Result<(u16, String), String>>>,
        put_responses: Mutex<std::collections::VecDeque<Result<(u16, String), String>>>,
        get_calls: Mutex<Vec<GetCall>>,
        put_calls: Mutex<Vec<PutCall>>,
    }

    impl Scripted {
        fn new() -> Self {
            Self::default()
        }
        fn enqueue_get(&self, r: Result<(u16, String), String>) {
            self.get_responses.lock().unwrap().push_back(r);
        }
        fn enqueue_put(&self, r: Result<(u16, String), String>) {
            self.put_responses.lock().unwrap().push_back(r);
        }
        fn last_get_url(&self) -> String {
            self.get_calls.lock().unwrap().last().unwrap().0.clone()
        }
        fn last_put_url(&self) -> String {
            self.put_calls.lock().unwrap().last().unwrap().0.clone()
        }
        fn last_put_body(&self) -> serde_json::Value {
            self.put_calls.lock().unwrap().last().unwrap().1.clone()
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for Scripted {
        async fn get(
            &self,
            url: &str,
            basic_auth: Option<(&str, &str)>,
            headers: &[(&str, &str)],
        ) -> Result<(u16, String), String> {
            self.get_calls.lock().unwrap().push((
                url.to_string(),
                basic_auth.map(|(u, p)| (u.to_string(), p.to_string())),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ));
            self.get_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("no scripted response".to_string()))
        }
        async fn put_json(
            &self,
            url: &str,
            body: &serde_json::Value,
            headers: &[(&str, &str)],
        ) -> Result<(u16, String), String> {
            self.put_calls.lock().unwrap().push((
                url.to_string(),
                body.clone(),
                headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ));
            self.put_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("no scripted response".to_string()))
        }
    }

    #[tokio::test]
    async fn duckdns_ok_when_response_is_ok() {
        let s = Scripted::new();
        s.enqueue_get(Ok((200, "OK\n".to_string())));
        let r = duckdns(&s, "subdomain.duckdns.org", "abc", Some("1.2.3.4")).await;
        assert!(r.ok);
        assert_eq!(r.message, "OK");
        let url = s.last_get_url();
        // domain is reduced to the leftmost label.
        assert!(url.contains("domains=subdomain"));
        assert!(url.contains("token=abc"));
        assert!(url.contains("ip=1.2.3.4"));
    }

    #[tokio::test]
    async fn duckdns_not_ok_on_other_response() {
        let s = Scripted::new();
        s.enqueue_get(Ok((200, "KO".to_string())));
        let r = duckdns(&s, "x.duckdns.org", "tok", None).await;
        assert!(!r.ok);
        assert_eq!(r.message, "KO");
    }

    #[tokio::test]
    async fn noip_requires_user_colon_pass_token() {
        let s = Scripted::new();
        let r = noip(&s, "host.example.com", "no-colon", None).await;
        assert!(!r.ok);
        assert!(r.message.contains("user:pass"));
    }

    #[tokio::test]
    async fn noip_sends_basic_auth_and_recognises_good_response() {
        let s = Scripted::new();
        s.enqueue_get(Ok((200, "good 1.2.3.4".to_string())));
        let r = noip(&s, "host.example.com", "alice:s3cret", Some("1.2.3.4")).await;
        assert!(r.ok);
        let call = s.get_calls.lock().unwrap().last().unwrap().clone();
        assert_eq!(call.1, Some(("alice".to_string(), "s3cret".to_string())));
        assert!(call.0.contains("hostname=host.example.com"));
        assert!(call.0.contains("myip=1.2.3.4"));
        // Both "good " and "nochg " count.
        let s2 = Scripted::new();
        s2.enqueue_get(Ok((200, "nochg 1.2.3.4".to_string())));
        let r2 = noip(&s2, "host.example.com", "alice:s3cret", None).await;
        assert!(r2.ok);
    }

    #[tokio::test]
    async fn cloudflare_requires_zone_id() {
        let s = Scripted::new();
        let r = cloudflare(&s, "x.example.com", "tok", Some("1.2.3.4"), &BTreeMap::new()).await;
        assert!(!r.ok);
        assert!(r.message.contains("zone_id"));
    }

    #[tokio::test]
    async fn cloudflare_looks_up_record_id_then_puts_updated_payload() {
        let s = Scripted::new();
        // First GET — list records.
        s.enqueue_get(Ok((
            200,
            serde_json::json!({
                "result": [{"id": "REC-42", "content": "9.9.9.9"}],
                "success": true,
            })
            .to_string(),
        )));
        // Then PUT — the update.
        s.enqueue_put(Ok((
            200,
            serde_json::json!({"success": true, "result": {"id": "REC-42"}}).to_string(),
        )));

        let mut extra = BTreeMap::new();
        extra.insert("zone_id".to_string(), "ZONE-1".to_string());
        let r = cloudflare(&s, "home.example.com", "TOK", Some("1.2.3.4"), &extra).await;
        assert!(r.ok, "should report success when body has success=true");

        let put_url = s.last_put_url();
        assert!(put_url.contains("/zones/ZONE-1/dns_records/REC-42"));
        let body = s.last_put_body();
        assert_eq!(body["name"], "home.example.com");
        assert_eq!(body["content"], "1.2.3.4");
        assert_eq!(body["ttl"], 60);
        assert_eq!(body["proxied"], false);
        assert_eq!(body["type"], "A");
    }

    #[tokio::test]
    async fn cloudflare_uses_lookup_content_when_no_ip_supplied() {
        let s = Scripted::new();
        s.enqueue_get(Ok((
            200,
            serde_json::json!({"result": [{"id": "R", "content": "5.5.5.5"}], "success": true})
                .to_string(),
        )));
        s.enqueue_put(Ok((
            200,
            serde_json::json!({"success": true}).to_string(),
        )));
        let mut extra = BTreeMap::new();
        extra.insert("zone_id".to_string(), "Z".to_string());
        let r = cloudflare(&s, "h.example.com", "TOK", None, &extra).await;
        assert!(r.ok);
        assert_eq!(s.last_put_body()["content"], "5.5.5.5");
    }

    #[tokio::test]
    async fn afraid_recognises_known_success_strings() {
        let s = Scripted::new();
        s.enqueue_get(Ok((200, "ERROR: Updated 1 hostname.".to_string())));
        let r = afraid(&s, "abc123", Some("1.2.3.4")).await;
        assert!(r.ok);
        // No-ip-change is also a success.
        let s2 = Scripted::new();
        s2.enqueue_get(Ok((200, "ERROR: No IP change.".to_string())));
        let r2 = afraid(&s2, "abc123", None).await;
        assert!(r2.ok);
        // Anything else fails.
        let s3 = Scripted::new();
        s3.enqueue_get(Ok((200, "ERROR: Authentication failed.".to_string())));
        let r3 = afraid(&s3, "abc123", None).await;
        assert!(!r3.ok);
    }

    #[tokio::test]
    async fn update_dispatch_normalises_provider_case() {
        let s = Scripted::new();
        s.enqueue_get(Ok((200, "OK".to_string())));
        let r = update(
            &s,
            "DUCKDNS  ",
            "x.duckdns.org",
            "tok",
            Some("1.2.3.4"),
            &BTreeMap::new(),
        )
        .await;
        assert!(r.ok);
    }

    #[tokio::test]
    async fn update_dispatch_returns_fail_for_unknown_provider() {
        let s = Scripted::new();
        let r = update(&s, "wat", "d", "t", None, &BTreeMap::new()).await;
        assert!(!r.ok);
        assert!(r.message.contains("unknown provider"));
    }
}
