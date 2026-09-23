//! Remote Access panel View.
//!
//! Layout (top → bottom):
//!
//!   * Header — title + "View paired devices" jump + manual Refresh.
//!   * VPN status card — interface up/down, listen port, server pubkey
//!     short hash, peer count.  Degrades to "wylde-vpn offline" when
//!     the pipe is unreachable.
//!   * Peer list — one row per active peer, auto-refreshed every 5 s.
//!     Click → jump to Devices (the spec asks for the peer ↔ paired-
//!     device link via cross-panel nav).
//!   * DDNS card — current `public_host` + manual-setup hint.  The
//!     dynamic-update verb isn't wired yet (externality); the card
//!     says so.
//!   * Port-forwarding card — fixed eero-mobile steps.  the Wylde user's router
//!     ships no web UI per the network memory.
//!   * DNS rewrites card — the static loopback hosts the RemoteAccess
//!     flow needs.  AdGuard integration isn't piped yet (externality).

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, FontWeight, IntoElement,
    Render, SharedString, Stateful, Window,
};
use wylde_gui_controls::control;
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_LIGHT, SURFACE_800, SURFACE_900,
    TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    read_config, read_peers, read_services, read_status, LinkConfig, LinkStatus, PeerRow,
    ServiceRow,
};

/// Auto-refresh cadence for the peer list + status card.  Matches the
/// pattern the Dashboard's slice-6 implementation uses.  Handshake
/// timestamps move on a wall-clock cadence; 5 s feels live without
/// flooding the pipe.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

pub struct RemoteAccessPanel {
    pub status: LinkStatus,
    pub config: LinkConfig,
    pub peers: Vec<PeerRow>,
    pub services: Vec<ServiceRow>,
    pub status_ever_read: bool,
    pub last_error: Option<String>,
    pub initial_load_done: bool,
}

impl RemoteAccessPanel {
    pub fn new() -> Self {
        Self {
            status: LinkStatus::default(),
            config: LinkConfig::default(),
            peers: Vec::new(),
            services: Vec::new(),
            status_ever_read: false,
            last_error: None,
            initial_load_done: false,
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            Self::spawn_refresh_loop(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                Self::refresh_once(this.clone(), app_cx).await;
                // gpui executor has no tokio reactor — native timer.
                app_cx.background_executor().timer(REFRESH_INTERVAL).await;
                let still_alive = this.update(app_cx, |_, _| {}).is_ok();
                if !still_alive {
                    return;
                }
            }
        })
        .detach();
    }

    pub async fn refresh_once(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let (status, config, peers, services) =
            tokio::join!(read_status(), read_config(), read_peers(), read_services());

        let _ = this.update(app_cx, |panel, cx| {
            // Status: trust a freshly-read shape; keep prior snapshot
            // on transport-level error so the card stays useful.
            match status {
                Ok(s) if !s.is_unknown() => {
                    panel.status = s;
                    panel.status_ever_read = true;
                    panel.last_error = None;
                }
                Ok(s) => {
                    // Empty envelope — server answered but no fields.
                    // Treat as offline-ish: don't clobber the snapshot,
                    // just keep the prior one visible.
                    let _ = s;
                }
                Err(e) => {
                    panel.last_error = Some(format!("wylde-vpn status: {e}"));
                }
            }
            if let Ok(cfg) = config {
                panel.config = cfg;
            }
            if let Ok(p) = peers {
                panel.peers = p;
            }
            if let Ok(s) = services {
                panel.services = s;
            }
            panel.initial_load_done = true;
            cx.notify();
        });
    }

    pub fn spawn_manual_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            Self::refresh_once(this.clone(), app_cx).await;
        })
        .detach();
    }
}

impl Default for RemoteAccessPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for RemoteAccessPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div()
            .max_w(px(860.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header_row(cx));

        if let Some(err) = &self.last_error {
            column = column.child(error_strip(err));
        }

        column = column.child(section_title("WyldeLink status"));
        column = column.child(status_card(self));

        column = column.child(section_title("Connected peers"));
        column = column.child(peers_card(self, cx));

        column = column.child(section_title("Dynamic DNS"));
        column = column.child(ddns_card(self));

        column = column.child(section_title("Router port forwarding"));
        column = column.child(port_forward_card(self.config.listen_port.max(51821)));

        column = column.child(section_title("DNS rewrites"));
        column = column.child(dns_rewrites_card());

        column = column.child(section_title("Services available remotely"));
        column = column.child(services_card(self));

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<RemoteAccessPanel>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::LG))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from("Remote Access")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "WyldeLink — self-hosted WireGuard tunnel for the Wylde mobile + \
                             tablet companions.  All Wylde services live behind this tunnel.",
                        )),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(jump_button(
                    "Paired devices",
                    "remote-jump-devices",
                    "core/devices",
                    cx,
                ))
                .child(refresh_button(cx)),
        )
}

fn refresh_button(cx: &mut Context<RemoteAccessPanel>) -> Stateful<gpui::Div> {
    control(div(), "remote-refresh")
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_DEFAULT)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut RemoteAccessPanel, _ev, _w, cx| {
                RemoteAccessPanel::spawn_manual_refresh(cx);
            }),
        )
        .child(SharedString::from("Refresh"))
}

fn jump_button(
    label: &'static str,
    id_str: &'static str,
    nav_key: &'static str,
    cx: &mut Context<RemoteAccessPanel>,
) -> Stateful<gpui::Div> {
    control(div(), id_str)
        .px_3()
        .py_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |_this: &mut RemoteAccessPanel, _ev, _w, _cx| {
                let _ = wylde_gui_pipe::request_nav(nav_key);
            }),
        )
        .child(SharedString::from(label))
}

fn section_title(label: &str) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .child(SharedString::from(label.to_ascii_uppercase()))
}

fn status_card(panel: &RemoteAccessPanel) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Reading WyldeLink interface state…");
    }
    if panel.status.is_unknown() && !panel.status_ever_read {
        return placeholder_card(
            "wylde-vpn isn't reachable — start it from the Tools panel or run \
             start_wylde_vpn.bat.  The card refreshes once the service comes up.",
        );
    }
    let interface_label = SharedString::from(if panel.status.interface_up {
        "wg1 up"
    } else {
        "wg1 down"
    });
    let listen = SharedString::from(format!(
        "Listen port · {}",
        if panel.status.listen_port > 0 {
            panel.status.listen_port.to_string()
        } else {
            "—".to_owned()
        },
    ));
    let pubkey = SharedString::from(format!(
        "Server key · {}",
        short_pubkey(&panel.status.public_key)
    ));
    let peer_count = SharedString::from(format!("Peer count · {}", panel.peers.len()));
    let endpoint = SharedString::from(format!(
        "Public endpoint · {}",
        if panel.config.public_host.is_empty() {
            "not configured".to_owned()
        } else {
            format!(
                "{}:{}",
                panel.config.public_host,
                if panel.status.listen_port > 0 {
                    panel.status.listen_port
                } else {
                    51821
                },
            )
        },
    ));

    let mut col = div().flex().flex_col().gap_2();
    col = col.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(status_dot(panel.status.interface_up))
            .child(
                div()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .text_color(rgb(pack(if panel.status.interface_up {
                        BRAND_LIGHT
                    } else {
                        TEXT_MUTED
                    })))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .child(interface_label),
            )
            .child(
                div()
                    .ml_2()
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::XS))
                    .text_color(rgb(pack(TEXT_SECONDARY)))
                    .child(SharedString::from(if panel.status.enabled {
                        "Enabled"
                    } else {
                        "Disabled"
                    })),
            ),
    );
    col = col
        .child(card_line(listen))
        .child(card_line(pubkey))
        .child(card_line(peer_count))
        .child(card_line(endpoint));

    if panel.config.restart_required {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(BRAND_LIGHT)))
                .child(SharedString::from(
                    "Configuration changed — wylde-vpn restart required for the change to take \
                     effect.",
                )),
        );
    }

    card_shell(col)
}

fn peers_card(panel: &RemoteAccessPanel, cx: &mut Context<RemoteAccessPanel>) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Listing connected peers…");
    }
    if panel.peers.is_empty() {
        return placeholder_card_clickable(
            "No peers registered yet.  Pair a device from the Devices panel to add one.",
            "remote-empty-peers",
            "core/devices",
            cx,
        );
    }
    let mut col = div().flex().flex_col().gap_2();
    for p in panel.peers.clone() {
        col = col.child(peer_row(&p, cx));
    }
    card_shell(col)
}

fn peer_row(p: &PeerRow, cx: &mut Context<RemoteAccessPanel>) -> Stateful<gpui::Div> {
    let label = if p.label.trim().is_empty() {
        SharedString::from(p.short_key())
    } else {
        SharedString::from(p.label.clone())
    };
    let meta = SharedString::from(format!(
        "{} · {} · last handshake {}",
        if p.tunnel_ip.is_empty() {
            "—".to_owned()
        } else {
            p.tunnel_ip.clone()
        },
        p.short_key(),
        fmt_handshake(&p.last_handshake),
    ));
    let traffic = if p.rx_bytes + p.tx_bytes > 0 {
        Some(SharedString::from(format!(
            "↓ {} · ↑ {}",
            humanize_bytes(p.rx_bytes),
            humanize_bytes(p.tx_bytes),
        )))
    } else {
        None
    };
    let mut row = control(div(), format!("remote-peer::{}", p.public_key))
        .border_b_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pb_2()
        .cursor_pointer()
        .flex()
        .flex_col()
        .gap_1()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|_this: &mut RemoteAccessPanel, _ev, _w, _cx| {
                let _ = wylde_gui_pipe::request_nav("core/devices");
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(status_dot(p.online))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(label),
                ),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(meta),
        );
    if let Some(t) = traffic {
        row = row.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::MICRO))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(t),
        );
    }
    row
}

fn ddns_card(panel: &RemoteAccessPanel) -> gpui::Div {
    let host_line = SharedString::from(if panel.config.public_host.is_empty() {
        "Configured hostname · not set".to_owned()
    } else {
        format!("Configured hostname · {}", panel.config.public_host)
    });
    let mut col = div().flex().flex_col().gap_2();
    col = col.child(card_line(host_line));
    col = col.child(card_line(SharedString::from(
        "Source · static (no DDNS updater verb yet)",
    )));
    col = col.child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(
                "Until the DDNS updater verb lands, set the hostname manually in your DNS \
                 provider and ensure the A record points at your current public IP.  The \
                 RemoteAccess panel will surface the live update timestamp once the verb is \
                 wired.",
            )),
    );
    card_shell(col)
}

fn port_forward_card(listen_port: u32) -> gpui::Div {
    let port_line = SharedString::from(format!(
        "Required forward · UDP {} → this machine (192.168.x.y)",
        listen_port,
    ));
    let steps: [&str; 5] = [
        "Open the eero app on your phone.",
        "Tap the menu (≡) → Discover → Network Settings.",
        "Tap Reservations & Port Forwarding → Add a Reservation.",
        "Pick this machine, then Add a Port → name it \"WyldeLink\".",
        "Set External + Internal Port to the value above, Protocol UDP, Save.",
    ];
    let mut col = div().flex().flex_col().gap_2();
    col = col.child(card_line(port_line));
    for (i, step) in steps.iter().enumerate() {
        col = col.child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .child(SharedString::from(format!("{}. {step}", i + 1))),
        );
    }
    col = col.child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(
                "eero has no web dashboard; these steps assume the iOS / Android app and only \
                 need to be run once per router.",
            )),
    );
    card_shell(col)
}

fn dns_rewrites_card() -> gpui::Div {
    let rows: [(&str, &str); 3] = [
        (
            "wylde.lan",
            "127.0.0.1 — loopback alias the local Wylde services bind to",
        ),
        (
            "cloud.wyldebot.com",
            "Resolves to the WyldeLink public host; redirects to 127.0.0.1 when on-tunnel",
        ),
        (
            "ollama.wylde.lan",
            "Loopback alias for the Ollama daemon — used by the mobile companion",
        ),
    ];
    let mut col = div().flex().flex_col().gap_2();
    for (host, blurb) in rows {
        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from(host.to_owned())),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(blurb.to_owned())),
                ),
        );
    }
    col = col.child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(BRAND_LIGHT)))
            .child(SharedString::from(
                "AdGuard rewrite sync is not yet wired — these hosts are documented here so \
                 they can be reproduced manually on the LAN.",
            )),
    );
    card_shell(col)
}

fn services_card(panel: &RemoteAccessPanel) -> gpui::Div {
    if !panel.initial_load_done {
        return placeholder_card("Reading exposed-services list…");
    }
    if panel.services.is_empty() {
        return placeholder_card(
            "wylde-vpn didn't report any remotely-accessible services.  Add one to your \
             VPN config or check the launcher.",
        );
    }
    let mut col = div().flex().flex_col().gap_2();
    for s in &panel.services {
        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::SM))
                        .text_color(rgb(pack(TEXT_PRIMARY)))
                        .font_weight(FontWeight(weight::SEMIBOLD as f32))
                        .child(SharedString::from(format!("{} · :{}", s.name, s.port))),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(if s.description.is_empty() {
                            "(no description)".to_owned()
                        } else {
                            s.description.clone()
                        })),
                ),
        );
    }
    card_shell(col)
}

fn status_dot(up: bool) -> gpui::Div {
    div()
        .w(px(8.0))
        .h(px(8.0))
        .rounded(px(999.0))
        .bg(rgb(pack(if up { BRAND } else { TEXT_MUTED })))
}

fn card_shell(body: gpui::Div) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .child(body)
}

fn card_line(label: SharedString) -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .child(label)
}

fn placeholder_card(text: &str) -> gpui::Div {
    card_shell(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(text.to_owned())),
    )
}

fn placeholder_card_clickable(
    text: &str,
    id_str: &'static str,
    nav_key: &'static str,
    cx: &mut Context<RemoteAccessPanel>,
) -> gpui::Div {
    let body = control(div(), id_str)
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_MUTED)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |_this: &mut RemoteAccessPanel, _ev, _w, _cx| {
                let _ = wylde_gui_pipe::request_nav(nav_key);
            }),
        )
        .child(SharedString::from(text.to_owned()));
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_4()
        .child(body)
}

fn error_strip(msg: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(4.0))
        .px_3()
        .py_2()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(msg.to_owned()))
}

// ── Pure projections ─────────────────────────────────────────────────

pub(crate) fn short_pubkey(s: &str) -> String {
    if s.is_empty() {
        return "—".to_owned();
    }
    if s.chars().count() <= 12 {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(12).collect::<String>())
    }
}

pub(crate) fn humanize_bytes(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = b as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

/// Convert the server's ISO-8601 handshake string into a relative
/// label like "12s ago".  When the input is empty or can't be parsed
/// we render an em-dash — the View never panics on a malformed time.
pub(crate) fn fmt_handshake(iso: &str) -> String {
    if iso.is_empty() {
        return "—".to_owned();
    }
    let Some(epoch) = parse_iso8601_to_unix(iso) else {
        return iso.to_owned();
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = (now - epoch).max(0.0);
    if secs < 60.0 {
        "just now".to_owned()
    } else if secs < 3_600.0 {
        format!("{}m ago", (secs / 60.0).round() as i64)
    } else if secs < 86_400.0 {
        format!("{}h ago", (secs / 3_600.0).round() as i64)
    } else {
        format!("{}d ago", (secs / 86_400.0).round() as i64)
    }
}

/// Minimal ISO-8601 parser — enough to handle the Python server's
/// `datetime.fromtimestamp(epoch, tz=timezone.utc).isoformat()` shape
/// (`YYYY-MM-DDTHH:MM:SS+00:00`).  Avoids pulling chrono into the
/// workspace for one timestamp format.
pub(crate) fn parse_iso8601_to_unix(iso: &str) -> Option<f64> {
    // Split off the offset portion.  The Python ISO format always
    // ends with `+HH:MM` or `Z` — we treat `Z` as `+00:00` and
    // recompose the offset.
    let (datepart, offset_secs) = if let Some(stripped) = iso.strip_suffix('Z') {
        (stripped, 0i64)
    } else if iso.len() >= 6 {
        let (head, tail) = iso.split_at(iso.len() - 6);
        if (tail.starts_with('+') || tail.starts_with('-')) && tail.as_bytes().get(3) == Some(&b':')
        {
            let sign = if tail.starts_with('+') { 1i64 } else { -1i64 };
            let hh: i64 = tail.get(1..3)?.parse().ok()?;
            let mm: i64 = tail.get(4..6)?.parse().ok()?;
            (head, sign * (hh * 3600 + mm * 60))
        } else {
            // No recognisable offset; assume UTC.
            (iso, 0i64)
        }
    } else {
        return None;
    };

    let (date, time) = datepart.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;

    let mut time_parts = time.split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let second_str = time_parts.next()?;
    let second: f64 = second_str.parse().ok()?;

    let epoch = civil_to_unix(year, month, day, hour, minute, second)?;
    Some(epoch - offset_secs as f64)
}

/// Howard Hinnant's days-from-civil algorithm.  Valid for 0001-01-01
/// onwards.  Returns Unix seconds (no leap-second handling — the
/// Python server reports UTC anyway).
fn civil_to_unix(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: f64,
) -> Option<f64> {
    if !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month as i64;
    let doy = (153 * (m + (if m > 2 { -3 } else { 9 })) + 2) / 5 + day as i64 - 1;
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days as f64 * 86_400.0 + hour as f64 * 3_600.0 + minute as f64 * 60.0 + second;
    Some(seconds)
}

pub(crate) fn pack(c: gpui::Rgba) -> u32 {
    let r = (c.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<RemoteAccessPanel>();
    }

    #[test]
    fn new_panel_starts_with_empty_state() {
        let p = RemoteAccessPanel::new();
        assert!(p.peers.is_empty());
        assert!(p.services.is_empty());
        assert!(p.last_error.is_none());
        assert!(!p.initial_load_done);
        assert!(p.status.is_unknown());
    }

    #[test]
    fn refresh_interval_pinned_at_five_seconds() {
        assert_eq!(REFRESH_INTERVAL.as_secs(), 5);
    }

    #[test]
    fn short_pubkey_handles_empty_and_short() {
        assert_eq!(short_pubkey(""), "—");
        assert_eq!(short_pubkey("abc"), "abc");
        assert!(short_pubkey("abcdefghijklmnopqrstu").ends_with('…'));
    }

    #[test]
    fn humanize_bytes_picks_units_per_magnitude() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(2048), "2 KB");
        assert_eq!(humanize_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(humanize_bytes(7_500_000_000_u64), "7.0 GB");
    }

    #[test]
    fn parse_iso_round_trips_known_epoch() {
        // 2026-05-29T10:00:00+00:00 — 56 years past the epoch (14 leap
        // days included) plus 148 days into the year, plus 10 hours.
        //   (56*365 + 14 + 148) * 86_400 + 10*3600 == 1_780_048_800
        let secs = parse_iso8601_to_unix("2026-05-29T10:00:00+00:00").unwrap();
        assert!((secs - 1_780_048_800.0).abs() < 60.0, "got {secs}",);
    }

    #[test]
    fn parse_iso_respects_offset_sign() {
        // Same wall-clock time but with a `+05:30` offset → 5h30 earlier
        // in UTC.  Verifies the offset path subtracts correctly.
        let utc = parse_iso8601_to_unix("2026-05-29T10:00:00+00:00").unwrap();
        let ist = parse_iso8601_to_unix("2026-05-29T10:00:00+05:30").unwrap();
        let delta = utc - ist;
        assert!((delta - (5.0 * 3600.0 + 30.0 * 60.0)).abs() < 1.0);
    }

    #[test]
    fn parse_iso_accepts_zulu_suffix() {
        assert!(parse_iso8601_to_unix("2026-05-29T10:00:00Z").is_some());
    }

    #[test]
    fn parse_iso_rejects_malformed() {
        assert!(parse_iso8601_to_unix("").is_none());
        assert!(parse_iso8601_to_unix("not-a-date").is_none());
        assert!(parse_iso8601_to_unix("2026-05-29").is_none());
    }

    #[test]
    fn fmt_handshake_empty_returns_em_dash() {
        assert_eq!(fmt_handshake(""), "—");
    }

    #[test]
    fn fmt_handshake_falls_back_to_raw_when_unparseable() {
        assert_eq!(fmt_handshake("bogus"), "bogus");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn status_card_signature_renders() {
        let p = RemoteAccessPanel::new();
        let _ = status_card(&p);
    }

    #[test]
    fn ddns_card_renders_for_empty_host() {
        let p = RemoteAccessPanel::new();
        let _ = ddns_card(&p);
    }
}
