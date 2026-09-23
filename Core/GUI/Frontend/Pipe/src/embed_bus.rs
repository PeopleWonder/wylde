//! Web-embed latch — a gpui panel asks the Shell to host a web page over a
//! region the panel paints.
//!
//! Panel crates must stay `wry`-free: the headless L7 walk builds every panel,
//! and it never builds a WebView. The Shell already owns the one WebView host
//! (`wylde_webview::IframeHost`) and its shared-auth plumbing for `iframe`
//! panels. This latch lets a `gpui_view` panel use the same host while keeping
//! its own header, status and controls in gpui:
//!
//!   * the panel calls [`request_embed`] with the page URL (and the optional
//!     `"service:action"` shared-auth hook the Shell already understands),
//!     [`set_embed_rect`] from a `canvas` prepaint with the content region's
//!     window-space bounds, [`reload_embed`] from its Reload control, and
//!     [`withdraw_embed`] when the page is not reachable;
//!   * the Shell reads [`embed_request`] for the selected panel each frame,
//!     mounts the WebView over the rect once the page probes healthy, and
//!     reports a mount failure back through [`set_embed_host_error`] so the
//!     panel can say why its region is empty.
//!
//! Same dependency-graph reason as the other buses: the Shell and the panels
//! both already depend on this crate, and neither depends on the other. Shape:
//! a keyed last-value latch (keyed by the panel's manifest id) — the Shell
//! reads the current value every frame, so there is no channel to drain.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// A window-space rectangle in logical pixels — the units gpui lays out in
/// and `wylde_webview::Bounds` takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmbedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One panel's current embed request.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedRequest {
    /// The page to host.
    pub url: String,
    /// Shared-auth hook (`"service:action"`) whose reply carries an
    /// `init_js` the Shell bakes into the WebView at creation.
    pub auth_bootstrap: Option<String>,
    /// Where to host it. `None` until the panel's region has been laid out.
    pub rect: Option<EmbedRect>,
    /// Bumped by [`reload_embed`]; the Shell rebuilds the WebView when it
    /// sees a new value.
    pub generation: u64,
    /// Set by the Shell when the WebView could not be hosted.
    pub host_error: Option<String>,
}

fn latch() -> &'static Mutex<BTreeMap<String, EmbedRequest>> {
    static LATCH: OnceLock<Mutex<BTreeMap<String, EmbedRequest>>> = OnceLock::new();
    LATCH.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Ask the Shell to host `url` for `panel`. Idempotent for an unchanged
/// request; a new URL or auth hook starts a fresh WebView (the rect is kept).
pub fn request_embed(panel: &str, url: &str, auth_bootstrap: Option<&str>) {
    let Ok(mut map) = latch().lock() else {
        return;
    };
    let auth_bootstrap = auth_bootstrap.map(str::to_owned);
    match map.get_mut(panel) {
        Some(req) if req.url == url && req.auth_bootstrap == auth_bootstrap => {}
        Some(req) => {
            req.url = url.to_owned();
            req.auth_bootstrap = auth_bootstrap;
            req.generation += 1;
            req.host_error = None;
        }
        None => {
            map.insert(
                panel.to_owned(),
                EmbedRequest {
                    url: url.to_owned(),
                    auth_bootstrap,
                    rect: None,
                    generation: 0,
                    host_error: None,
                },
            );
        }
    }
}

/// Record where `panel`'s content region painted. Returns `true` when the
/// rect changed, so the panel can ask for one more frame and the Shell
/// repositions the WebView without waiting for unrelated input.
pub fn set_embed_rect(panel: &str, rect: EmbedRect) -> bool {
    let Ok(mut map) = latch().lock() else {
        return false;
    };
    match map.get_mut(panel) {
        Some(req) if req.rect != Some(rect) => {
            req.rect = Some(rect);
            true
        }
        _ => false,
    }
}

/// Ask the Shell to rebuild `panel`'s WebView (a fresh page load with a fresh
/// shared-auth fetch). Returns `false` when nothing is being hosted.
pub fn reload_embed(panel: &str) -> bool {
    let Ok(mut map) = latch().lock() else {
        return false;
    };
    match map.get_mut(panel) {
        Some(req) => {
            req.generation += 1;
            req.host_error = None;
            true
        }
        None => false,
    }
}

/// Stop hosting anything for `panel`; the Shell drops its WebView.
pub fn withdraw_embed(panel: &str) {
    if let Ok(mut map) = latch().lock() {
        map.remove(panel);
    }
}

/// `panel`'s current request, or `None` when it has not asked for one.
pub fn embed_request(panel: &str) -> Option<EmbedRequest> {
    latch().lock().ok().and_then(|map| map.get(panel).cloned())
}

/// Shell → panel: why the WebView could not be hosted (`None` clears it).
pub fn set_embed_host_error(panel: &str, error: Option<String>) {
    if let Ok(mut map) = latch().lock() {
        if let Some(req) = map.get_mut(panel) {
            req.host_error = error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The latch is process-wide, so each test owns a distinct panel key.

    #[test]
    fn a_request_round_trips_and_withdraws() {
        let p = "embed-test-roundtrip";
        assert_eq!(embed_request(p), None);
        request_embed(p, "http://127.0.0.1:5678", Some("svc:act"));
        let req = embed_request(p).unwrap();
        assert_eq!(req.url, "http://127.0.0.1:5678");
        assert_eq!(req.auth_bootstrap.as_deref(), Some("svc:act"));
        assert_eq!(req.rect, None);
        withdraw_embed(p);
        assert_eq!(embed_request(p), None);
    }

    #[test]
    fn an_unchanged_request_keeps_its_generation_and_a_new_url_bumps_it() {
        let p = "embed-test-generation";
        request_embed(p, "http://a", None);
        request_embed(p, "http://a", None);
        assert_eq!(embed_request(p).unwrap().generation, 0);
        request_embed(p, "http://b", None);
        assert_eq!(embed_request(p).unwrap().generation, 1);
        assert!(reload_embed(p));
        assert_eq!(embed_request(p).unwrap().generation, 2);
        withdraw_embed(p);
        assert!(!reload_embed(p), "nothing hosted, nothing to reload");
    }

    #[test]
    fn set_rect_reports_only_real_changes() {
        let p = "embed-test-rect";
        let r = EmbedRect {
            x: 240.0,
            y: 48.0,
            width: 800.0,
            height: 600.0,
        };
        assert!(!set_embed_rect(p, r), "no request, nothing to place");
        request_embed(p, "http://a", None);
        assert!(set_embed_rect(p, r));
        assert!(!set_embed_rect(p, r), "same rect is not a change");
        assert_eq!(embed_request(p).unwrap().rect, Some(r));
        withdraw_embed(p);
    }

    #[test]
    fn the_host_error_reaches_the_panel_and_a_reload_clears_it() {
        let p = "embed-test-error";
        request_embed(p, "http://a", None);
        set_embed_host_error(p, Some("wry mount: boom".into()));
        assert_eq!(
            embed_request(p).unwrap().host_error.as_deref(),
            Some("wry mount: boom")
        );
        reload_embed(p);
        assert_eq!(embed_request(p).unwrap().host_error, None);
        withdraw_embed(p);
    }
}
