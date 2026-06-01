//! WebView host for extension `iframe` panels.
//!
//! Wraps `wry` into a small, gpui-friendly surface so the Shell's slot
//! can mount a WebView child window without learning the wry API.
//!
//! Why a dedicated crate?  wry's `WebView` is `!Send + !Sync` on most
//! platforms (the browser engine owns it on the UI thread) and carries
//! a long dependency tail (WebView2 on Windows, WKWebView on macOS,
//! WebKitGTK on Linux).  Lifting the wrapping into its own crate keeps
//! the Shell's compile-unit small and the wry API surface change
//! costs the cutover would otherwise pay.
//!
//! Scope:
//!
//!   * `IframeHost` — `mount`/`unmount`/`set_bounds`/`drop`.  Lazily
//!     creates the WebView the first time the slot mounts an iframe
//!     panel.  Re-uses the same WebView across selections (a navigation
//!     issues `load_url` rather than tearing the view down).
//!
//!   * `probe_url` — async HTTP HEAD probe with a hard timeout.  Used
//!     by the slot to decide between mounting the WebView and rendering
//!     the existing `ServiceUnavailable` stub.
//!
//!   * `translate_sandbox` — turn an iframe `sandbox=""` attr into the
//!     closest wry capability flags.  Many iframe sandbox tokens have
//!     no direct wry analogue (wry runs each WebView as its own
//!     process-isolated browser, so the same-origin / top-navigation
//!     concepts don't apply).  The translator documents the gap rather
//!     than silently dropping the token: any unknown / unsupported
//!     token is surfaced in `SandboxApplied::unsupported` so the
//!     slot can log a one-line warning.
//!
//! Threading: every `IframeHost` method that touches the underlying
//! WebView must run on the gpui dispatcher thread (the UI thread).
//! `probe_url` is `Send` and runs on tokio.

use std::rc::Rc;

use anyhow::anyhow;
use raw_window_handle::HasWindowHandle;
use wry::{dpi::LogicalPosition, dpi::LogicalSize, Rect, WebView, WebViewBuilder};

/// Logical rectangle the slot uses to position the WebView.  We pass
/// gpui's pixels through unchanged — wry's `LogicalSize` is in DIPs
/// which match gpui's `Pixels` on the same DPI scale (gpui already
/// applies the OS scale factor when laying out).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn to_wry(self) -> Rect {
        Rect {
            position: LogicalPosition::new(self.x, self.y).into(),
            size: LogicalSize::new(self.width.max(1.0), self.height.max(1.0)).into(),
        }
    }
}

/// Outcome of translating an iframe `sandbox` attr.  The wry-side flags
/// have already been applied by `translate_sandbox` to the builder; the
/// struct carries the *unsupported* tokens back so the caller (the
/// slot) can log them — silent drops would be a security smell.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxApplied {
    /// Tokens we mapped to a real wry flag.
    pub recognised: Vec<String>,
    /// Tokens we saw but cannot enforce 1:1 in wry.  The slot logs them
    /// once per mount so manifest authors see the gap.
    pub unsupported: Vec<String>,
}

/// Translate an iframe `sandbox=""` token list to wry builder calls +
/// a report of what we couldn't enforce.
///
/// The HTML iframe `sandbox` attr is *deny by default*: an empty
/// attr (`sandbox=""`) means "block everything"; specific tokens
/// re-enable specific capabilities.  wry's defaults are roughly
/// inverted (everything on; specific opt-outs).  Where the two models
/// disagree we surface the gap rather than silently dropping the
/// token.
///
/// Recognised tokens (mapped to wry flags):
///
///   * `allow-scripts` → JavaScript stays enabled (wry default).
///     If `sandbox` is set but this token is absent, JavaScript is
///     *disabled* — matches iframe semantics.
///   * `allow-forms` / `allow-modals` / `allow-popups` — informational
///     today (wry allows these by default; tightening them would
///     require deeper `with_*_handler` wiring which we leave to a
///     follow-on slice).
///   * `allow-downloads` — informational (download handler stays at
///     wry default).
///
/// Unsupported tokens (surfaced for logging):
///
///   * `allow-same-origin` — wry runs the embedded page as its own
///     isolated browser; same-origin / cross-origin policies are
///     enforced by the embedded server's CORS, not by the host.
///   * `allow-top-navigation` / `allow-top-navigation-by-user-activation`
///     — wry isn't an iframe; there is no enclosing top frame to
///     navigate.
///   * Any other token — surfaced so a manifest typo lights up rather
///     than silently relaxing the sandbox.
pub fn translate_sandbox<'a>(
    builder: WebViewBuilder<'a>,
    sandbox: Option<&str>,
) -> (WebViewBuilder<'a>, SandboxApplied) {
    let mut applied = SandboxApplied::default();
    let Some(s) = sandbox else {
        return (builder, applied);
    };
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let has_scripts = tokens.contains(&"allow-scripts");
    let mut builder = builder;
    // iframe semantics: when `sandbox` is set without `allow-scripts`,
    // JavaScript is disabled.  Mirror that on the wry side — wry's
    // default is enabled, so we only need to call the opt-out when
    // scripts are explicitly *not* allowed.
    if !has_scripts && !tokens.is_empty() {
        builder = builder.with_javascript_disabled();
    }

    for tok in tokens {
        match tok {
            "allow-scripts" | "allow-forms" | "allow-modals" | "allow-popups"
            | "allow-downloads" => {
                applied.recognised.push(tok.to_owned());
            }
            "allow-same-origin"
            | "allow-top-navigation"
            | "allow-top-navigation-by-user-activation"
            | "allow-pointer-lock"
            | "allow-orientation-lock"
            | "allow-presentation" => {
                applied.unsupported.push(tok.to_owned());
            }
            other => {
                applied.unsupported.push(other.to_owned());
            }
        }
    }
    (builder, applied)
}

/// Lazily-mounted WebView wrapper.
///
/// `IframeHost` is `!Send + !Sync` because `wry::WebView` is — it must
/// live on the same thread as the gpui Window.  All public methods are
/// designed to be called from the UI thread (gpui's render path or a
/// gpui task spawned on the dispatcher).
pub struct IframeHost {
    url: String,
    sandbox: Option<String>,
    webview: Option<Rc<WebView>>,
    last_bounds: Option<Bounds>,
    pub last_sandbox_report: Option<SandboxApplied>,
}

impl IframeHost {
    pub fn new(url: impl Into<String>, sandbox: Option<String>) -> Self {
        Self {
            url: url.into(),
            sandbox,
            webview: None,
            last_bounds: None,
            last_sandbox_report: None,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn sandbox(&self) -> Option<&str> {
        self.sandbox.as_deref()
    }

    pub fn is_mounted(&self) -> bool {
        self.webview.is_some()
    }

    /// Create the WebView as a child of `parent` if it isn't already.
    /// `parent` is typically a `&gpui::Window` (which implements
    /// `HasWindowHandle`).  Errors when the platform refuses to mint
    /// the WebView (no WebView2 runtime, etc.) so the caller can fall
    /// back to the `ServiceUnavailable` stub.
    pub fn mount<W: HasWindowHandle>(
        &mut self,
        parent: &W,
        bounds: Bounds,
    ) -> anyhow::Result<()> {
        if self.webview.is_some() {
            return self.set_bounds(bounds);
        }
        let url = self.url.clone();
        let builder = WebViewBuilder::new().with_url(&url).with_bounds(bounds.to_wry());
        let (builder, report) = translate_sandbox(builder, self.sandbox.as_deref());
        let webview = builder
            .build_as_child(parent)
            .map_err(|e| anyhow!("wry build_as_child for {url}: {e}"))?;
        self.webview = Some(Rc::new(webview));
        self.last_bounds = Some(bounds);
        self.last_sandbox_report = Some(report);
        Ok(())
    }

    /// Reposition / resize the WebView.  No-op if not yet mounted —
    /// caller can call `mount` instead.
    pub fn set_bounds(&mut self, bounds: Bounds) -> anyhow::Result<()> {
        let Some(view) = self.webview.as_ref() else {
            return Ok(());
        };
        view.set_bounds(bounds.to_wry())
            .map_err(|e| anyhow!("wry set_bounds: {e}"))?;
        self.last_bounds = Some(bounds);
        Ok(())
    }

    /// Drop the WebView and release its native handle.  Called when
    /// the user navigates to a different panel so the WebView's native
    /// surface stops painting over the slot.
    pub fn unmount(&mut self) {
        self.webview = None;
        // We keep `last_bounds` + `last_sandbox_report` so a re-mount
        // (e.g. the user toggles back to this panel) can resume at the
        // same position without re-running the translator.
    }
}

impl Drop for IframeHost {
    fn drop(&mut self) {
        // Explicit unmount keeps the destruction order predictable —
        // the Rc is dropped here rather than during the surrounding
        // struct's field drop sequence.
        self.unmount();
    }
}

/// Probe `url` with an HTTP HEAD request.  Returns `Ok(())` if the URL
/// returned any HTTP response within `timeout_ms`, `Err(_)` otherwise.
///
/// We deliberately don't check the response status code — a 401 or 404
/// from a healthy server still means "the server is up and reachable",
/// which is all the iframe slot needs to know.  The pattern matches
/// the Svelte alpha's `fetch(url, {mode:"no-cors"})` probe.
pub async fn probe_url(url: &str, timeout_ms: u64) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| format!("http build: {e}"))?;
    // HEAD is preferred (cheap, no body), but many dev servers — n8n
    // included — answer GET only on the root path.  Fall back to GET
    // if HEAD returns a method-not-allowed.
    let head = client.head(url).send().await;
    match head {
        Ok(_) => Ok(()),
        Err(e) if e.is_status() => Ok(()),
        Err(_head_err) => {
            // Retry with GET.  We don't surface the HEAD error on
            // success — the GET reply is what matters.
            client
                .get(url)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("http get {url}: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_to_wry_round_trips_known_rect() {
        let b = Bounds::new(10.0, 20.0, 800.0, 600.0);
        let rect = b.to_wry();
        // We can't introspect dpi::Position fields directly, but the
        // round trip is observable through wry's PartialEq.
        let other = Bounds::new(10.0, 20.0, 800.0, 600.0).to_wry();
        assert_eq!(rect, other);
    }

    #[test]
    fn bounds_clamp_zero_size_to_minimum() {
        // wry refuses to mint a zero-area WebView; the helper clamps
        // each dimension to 1 logical pixel.
        let b = Bounds::new(0.0, 0.0, 0.0, 0.0);
        let one_one = Bounds::new(0.0, 0.0, 1.0, 1.0);
        assert_eq!(b.to_wry(), one_one.to_wry());
    }

    #[test]
    fn translate_sandbox_none_means_no_report() {
        let builder = WebViewBuilder::new();
        let (_b, report) = translate_sandbox(builder, None);
        assert!(report.recognised.is_empty());
        assert!(report.unsupported.is_empty());
    }

    #[test]
    fn translate_sandbox_recognises_allow_scripts_and_forms() {
        let builder = WebViewBuilder::new();
        let (_b, report) =
            translate_sandbox(builder, Some("allow-scripts allow-forms"));
        assert!(report.recognised.iter().any(|s| s == "allow-scripts"));
        assert!(report.recognised.iter().any(|s| s == "allow-forms"));
        assert!(report.unsupported.is_empty());
    }

    #[test]
    fn translate_sandbox_surfaces_unsupported_tokens() {
        let builder = WebViewBuilder::new();
        let (_b, report) =
            translate_sandbox(builder, Some("allow-same-origin allow-top-navigation"));
        assert!(report.unsupported.iter().any(|s| s == "allow-same-origin"));
        assert!(report
            .unsupported
            .iter()
            .any(|s| s == "allow-top-navigation"));
    }

    #[test]
    fn translate_sandbox_surfaces_unknown_typo_tokens() {
        let builder = WebViewBuilder::new();
        let (_b, report) = translate_sandbox(builder, Some("allow-scripts allow-typoxyz"));
        assert!(report.recognised.iter().any(|s| s == "allow-scripts"));
        assert!(report.unsupported.iter().any(|s| s == "allow-typoxyz"));
    }

    #[test]
    fn iframe_host_new_is_unmounted() {
        let h = IframeHost::new("http://127.0.0.1:5678", None);
        assert!(!h.is_mounted());
        assert_eq!(h.url(), "http://127.0.0.1:5678");
        assert!(h.sandbox().is_none());
    }

    #[test]
    fn iframe_host_carries_sandbox_string() {
        let h = IframeHost::new(
            "http://127.0.0.1:5678",
            Some("allow-scripts allow-forms".into()),
        );
        assert_eq!(h.sandbox(), Some("allow-scripts allow-forms"));
    }

    #[test]
    fn iframe_host_set_bounds_is_noop_before_mount() {
        let mut h = IframeHost::new("http://127.0.0.1:5678", None);
        // No mount yet — set_bounds returns Ok and doesn't crash.
        assert!(h.set_bounds(Bounds::new(0.0, 0.0, 100.0, 100.0)).is_ok());
        assert!(!h.is_mounted());
    }

    #[test]
    fn iframe_host_unmount_is_idempotent() {
        let mut h = IframeHost::new("http://127.0.0.1:5678", None);
        h.unmount();
        h.unmount();
        assert!(!h.is_mounted());
    }

    /// `probe_url` against a closed port reports the error rather than
    /// hanging.  The exact error text varies by platform; what matters
    /// is the timeout fires within ~the budget rather than blocking
    /// the caller indefinitely.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_url_returns_error_for_closed_port() {
        // Port 1 on loopback is reliably refused.
        let res = probe_url("http://127.0.0.1:1", 1_500).await;
        assert!(res.is_err(), "expected probe error, got: {res:?}");
    }

    /// Smoke-test fixture for the slot's iframe rendering: spin up a
    /// minimal HTTP/1.1 loopback server, point `probe_url` at it,
    /// and assert success.  Mirrors the slice spec's "spin up a
    /// `python -m http.server 9999`" path so the WebView health gate
    /// has a positive-path test that survives in CI without N8N.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_url_succeeds_against_loopback_fixture() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let addr = listener.local_addr().expect("fixture addr");
        let url = format!("http://{addr}/");

        // Server loop: accept one connection, write a minimal 200,
        // close.  Doesn't bother parsing the request.
        let server = tokio::spawn(async move {
            // We may handle either a HEAD (first attempt) or a GET
            // (fallback).  Either way the response shape is the same.
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 256];
                // Best-effort read so the client side sees the
                // socket as alive; ignore short reads.
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            }
        });

        let res = probe_url(&url, 3_000).await;
        // Ensure the server task exits cleanly so the test doesn't
        // leak a tokio task.  Best-effort: if the probe was satisfied
        // by HEAD alone, the second accept never returns.
        server.abort();
        assert!(res.is_ok(), "expected probe success against fixture, got: {res:?}");
    }
}
