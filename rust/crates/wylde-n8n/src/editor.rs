//! Embedded-editor bootstrap: the shared-auth bridge for the WebView.
//!
//! Requirement 2 (shared auth) lands here. The Wylde GUI embeds the n8n
//! editor in a `wry` WebView (Requirement 1). For the user to land
//! inside an already-authenticated editor — without ever seeing n8n's
//! login screen — the WebView is mounted with an *initialization
//! script* that logs in as the Wylde-owned owner before n8n's own app
//! boots.
//!
//! The `n8n.editor_bootstrap` action hands the Shell everything it needs:
//! the loopback URL and that init script. The script:
//!
//!   1. Pins `localStorage['n8n-browserId']` to a fixed value. n8n binds
//!      every session JWT to the `browser-id` header its frontend sends
//!      (read from exactly this localStorage key — verified against
//!      `n8n-editor-ui`'s `useRootStore`). Pinning it means our login
//!      and the app's subsequent requests agree on the browser id, so
//!      the session validates.
//!   2. Synchronously checks `GET /rest/login`; if already authed, stops.
//!   3. Otherwise `POST /rest/login` with the Wylde-owned owner
//!      credentials. The sync XHR runs *before* n8n's app scripts, so
//!      the auth cookie is in place by the time the app calls
//!      `getCurrentUser` — no login screen, no reload in the common
//!      case. A one-shot guarded reload is the belt-and-suspenders path.
//!
//! The credentials embedded in the script are **Wylde-generated** (the
//! owner identity in the out-of-tree data dir), never the user's own
//! secret, and only ever travel over loopback to 127.0.0.1.

use std::sync::OnceLock;

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use crate::secret::N8nIdentity;

/// Fixed browser id for the embedded editor session. Any stable string
/// works (n8n only requires login and later requests to agree); a fixed
/// value keeps the embedded session self-consistent across mounts and
/// distinct from a real browser the user might also open n8n in.
const EMBED_BROWSER_ID: &str = "wylde-embedded-editor";

/// Process-wide editor context, set once at startup when running in
/// managed mode. Absent in unmanaged/external mode — then bootstrap
/// returns the URL with no injection and the GUI loads n8n as-is.
#[derive(Debug, Clone)]
pub struct EditorContext {
    pub base_url: String,
    pub identity: N8nIdentity,
}

static EDITOR_CTX: OnceLock<EditorContext> = OnceLock::new();

/// Install the editor context. Idempotent — a second call is ignored
/// (the first managed-startup wins).
pub fn set_context(ctx: EditorContext) {
    let _ = EDITOR_CTX.set(ctx);
}

/// The current editor context, if managed mode installed one.
pub fn context() -> Option<&'static EditorContext> {
    EDITOR_CTX.get()
}

/// `n8n.editor_bootstrap {}` →
/// `{url, managed, init_js?}`.
///
/// * `url` — the loopback editor URL to mount in the WebView.
/// * `managed` — true when Wylde owns the daemon and shared-auth is wired.
/// * `init_js` — the auto-login script to inject (only in managed mode).
pub async fn handle_editor_bootstrap(_payload: Value) -> Reply {
    match context() {
        Some(ctx) => Reply::ok(json!({
            "url": ctx.base_url,
            "managed": true,
            "init_js": build_login_script(
                &ctx.base_url,
                &ctx.identity.owner_email,
                &ctx.identity.owner_password,
            ),
        })),
        None => {
            // Unmanaged: surface the configured upstream URL so the GUI
            // can still embed an externally-run n8n, but inject nothing —
            // its auth is the operator's to configure.
            let url = crate::config::Config::get().auth.url.clone();
            Reply::ok(json!({
                "url": url,
                "managed": false,
                "init_js": Value::Null,
            }))
        }
    }
}

/// Build the WebView initialization script that logs the embedded editor
/// in as the Wylde-owned owner. `base_url` is informational; the script
/// uses same-origin relative paths so it works regardless of host.
///
/// Pure function over the three strings → unit-testable without a live
/// n8n. Values are interpolated through `serde_json` so any future
/// change to the credential charset can't break out of the JS string
/// literal.
pub fn build_login_script(base_url: &str, email: &str, password: &str) -> String {
    // JS string literals, safely quoted+escaped by serde_json.
    let email_lit = serde_json::to_string(email).unwrap_or_else(|_| "\"\"".into());
    let pass_lit = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".into());
    let bid_lit = serde_json::to_string(EMBED_BROWSER_ID).unwrap_or_else(|_| "\"\"".into());
    // `base_url` is only referenced in a comment; keep it out of the
    // executable body to avoid an unused-warning while documenting intent.
    let _ = base_url;
    format!(
        r#"(function () {{
  try {{
    var BID = {bid_lit};
    // Pin the browser id n8n's frontend will read + send so our login
    // and the app's later requests share one session identity.
    try {{ localStorage.setItem('n8n-browserId', BID); }} catch (e) {{}}
    var H = {{ 'browser-id': BID, 'content-type': 'application/json' }};
    function authed() {{
      try {{
        var g = new XMLHttpRequest();
        g.open('GET', '/rest/login', false);
        g.setRequestHeader('browser-id', BID);
        g.withCredentials = true;
        g.send();
        return g.status === 200;
      }} catch (e) {{ return false; }}
    }}
    if (authed()) {{ return; }}
    var p = new XMLHttpRequest();
    p.open('POST', '/rest/login', false);
    p.setRequestHeader('browser-id', BID);
    p.setRequestHeader('content-type', 'application/json');
    p.withCredentials = true;
    p.send(JSON.stringify({{ emailOrLdapLoginId: {email_lit}, password: {pass_lit} }}));
    if (p.status === 200 && !sessionStorage.getItem('wylde_n8n_reloaded')) {{
      // The cookie is set pre-boot, so a reload is rarely needed; guard
      // it so it can fire at most once per tab session (no loop).
      sessionStorage.setItem('wylde_n8n_reloaded', '1');
      location.reload();
    }}
  }} catch (e) {{}}
}})();
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_script_embeds_credentials_and_browser_id() {
        let js = build_login_script("http://127.0.0.1:5678", "wylde-owner@wylde.local", "Wq7abc");
        assert!(js.contains("n8n-browserId"));
        assert!(js.contains("wylde-embedded-editor"));
        assert!(js.contains("/rest/login"));
        assert!(js.contains("wylde-owner@wylde.local"));
        assert!(js.contains("Wq7abc"));
        assert!(js.contains("emailOrLdapLoginId"));
        // Guards a reload loop.
        assert!(js.contains("wylde_n8n_reloaded"));
    }

    #[test]
    fn login_script_escapes_quote_in_password() {
        // A pathological password with a quote must not break out of the
        // JS string literal.
        let js = build_login_script("http://x", "a@b.c", "p\"; alert(1);//");
        assert!(
            !js.contains("p\"; alert(1)"),
            "raw injection must be escaped"
        );
        assert!(
            js.contains("alert(1)"),
            "value still present, but quoted/escaped"
        );
    }

    #[tokio::test]
    async fn bootstrap_unmanaged_returns_url_without_injection() {
        // No context set → unmanaged shape.
        let reply = handle_editor_bootstrap(json!({})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["managed"], false);
        assert!(reply.data["init_js"].is_null());
        assert!(reply.data["url"].is_string());
    }
}
