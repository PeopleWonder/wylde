//! Hosting web embeds for `gpui_view` panels.
//!
//! A panel that shows a web page (the Workflows panel's n8n editor) stays
//! `wry`-free: it paints its own gpui header and a content region, and asks for
//! the page through [`wylde_gui_pipe::embed_bus`] — URL, shared-auth hook, and
//! the region's window-space rect. This module is the Shell's half: each frame
//! it reads the selected panel's request and drives the same
//! [`IframeState`] / `IframeHost` machinery `iframe` panels use (URL probe,
//! shared-auth init script, mount-at-bounds), placed over the panel's region
//! instead of the whole slot.
//!
//! Unlike an `iframe` panel, a failure is never turned into the slot's
//! ServiceUnavailable stub — the panel owns its chrome, so the Shell reports
//! the failure back through the latch and the panel says what went wrong.

use gpui::{Context, Window};
use wylde_gui_pipe::embed_bus;
use wylde_gui_shell_chrome::IframeHealth;
use wylde_panel_registry::{PanelRegistry, PanelSource};

use crate::shell_root::{registry_key_matches, FrameRef, IframeState, Shell};

/// One web embed the Shell is hosting for a `gpui_view` panel.
pub struct EmbedState {
    /// The frame — the same probe / auth / host state an `iframe` panel has.
    pub frame: IframeState,
    /// The [`embed_bus::EmbedRequest::generation`] this frame was built for.
    /// A newer generation (a Reload, a new URL) rebuilds the frame.
    pub generation: u64,
}

/// Whether the frame hosted for `current` must be rebuilt to serve `req`.
fn needs_rebuild(current: Option<&EmbedState>, req: &embed_bus::EmbedRequest) -> bool {
    current.is_none_or(|e| e.generation != req.generation || e.frame.url != req.url)
}

impl Shell {
    /// The manifest id of the `gpui_view` panel registered under `key`.
    fn gpui_panel_id(key: &str) -> Option<String> {
        PanelRegistry::global()?
            .entries()
            .into_iter()
            .find(|r| registry_key_matches(&r.origin, &r.entry.id, key))
            .filter(|r| matches!(r.entry.source, PanelSource::GpuiView { .. }))
            .map(|r| r.entry.id.clone())
    }

    /// Drive the selected panel's web embed. Called once per render: cheap
    /// and idempotent once the embed is mounted.
    pub(crate) fn sync_embeds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.nav.selected_key.clone();
        // A panel that is not on screen must not have its WebView painting
        // over the one that is.
        for (key, embed) in self.embeds.iter_mut() {
            if Some(key) != selected.as_ref() {
                embed.frame.host.unmount();
            }
        }
        let Some(key) = selected else {
            return;
        };
        let Some(panel) = Self::gpui_panel_id(&key) else {
            return;
        };
        let Some(req) = embed_bus::embed_request(&panel) else {
            // Withdrawn (or never asked): nothing to host.
            if let Some(mut gone) = self.embeds.remove(&key) {
                gone.frame.host.unmount();
            }
            return;
        };

        if needs_rebuild(self.embeds.get(&key), &req) {
            if let Some(mut old) = self.embeds.remove(&key) {
                old.frame.host.unmount();
            }
            self.embeds.insert(
                key.clone(),
                EmbedState {
                    frame: IframeState::new(req.url.clone(), None, req.auth_bootstrap.clone()),
                    generation: req.generation,
                },
            );
            let r = FrameRef::Embed {
                key: key.clone(),
                generation: req.generation,
            };
            self.spawn_frame_probe(r.clone(), cx);
            if req.auth_bootstrap.is_some() {
                self.spawn_frame_auth(r, cx);
            }
        }

        let Some(embed) = self.embeds.get_mut(&key) else {
            return;
        };
        let Some(rect) = req.rect else {
            // The panel has not laid its region out yet.
            embed.frame.host.unmount();
            return;
        };
        match &embed.frame.health {
            IframeHealth::Healthy if !embed.frame.auth_pending => {
                let bounds = wylde_webview::Bounds::new(rect.x, rect.y, rect.width, rect.height);
                // `mount` is idempotent: once mounted it just repositions.
                if let Err(e) = embed.frame.host.mount(window, bounds) {
                    let msg = format!("wry mount: {e}");
                    embed.frame.health = IframeHealth::Unhealthy(msg.clone());
                    embed_bus::set_embed_host_error(&panel, Some(msg));
                }
            }
            IframeHealth::Unhealthy(msg) => {
                embed.frame.host.unmount();
                if req.host_error.as_deref() != Some(msg.as_str()) {
                    embed_bus::set_embed_host_error(&panel, Some(msg.clone()));
                }
            }
            // Still probing, or the shared-auth script is still on its way
            // (wry bakes init scripts in at creation, so mounting early would
            // drop the user on the embedded app's login screen).
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str, generation: u64) -> embed_bus::EmbedRequest {
        embed_bus::EmbedRequest {
            url: url.into(),
            auth_bootstrap: None,
            rect: None,
            generation,
            host_error: None,
        }
    }

    fn hosted(url: &str, generation: u64) -> EmbedState {
        EmbedState {
            frame: IframeState::new(url, None, None),
            generation,
        }
    }

    #[test]
    fn a_first_request_builds_a_frame() {
        assert!(needs_rebuild(None, &request("http://a", 0)));
    }

    #[test]
    fn an_unchanged_request_keeps_the_mounted_frame() {
        let e = hosted("http://a", 3);
        assert!(!needs_rebuild(Some(&e), &request("http://a", 3)));
    }

    #[test]
    fn a_reload_or_a_new_url_rebuilds_the_frame() {
        let e = hosted("http://a", 3);
        assert!(needs_rebuild(Some(&e), &request("http://a", 4)), "Reload");
        assert!(needs_rebuild(Some(&e), &request("http://b", 3)), "new URL");
    }
}
