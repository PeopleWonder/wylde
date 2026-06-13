//! Devices panel View — paired devices, pairing flow, tier + revoke.
//!
//! Layout (top → bottom):
//!
//!   * Header — title + manual Refresh + cross-panel jump to
//!     RemoteAccess.
//!   * Pair card — toggles between an "Initiate pairing" button and a
//!     live countdown card with the 6-digit code + QR matrix.  The
//!     1-second tick decrements the visible counter; a separate 2 s
//!     poll watches `get_pairing_status` so the card auto-closes the
//!     instant the mobile finishes pairing.
//!   * Paired list — one row per device with name, fingerprint,
//!     paired-on date, last-seen pill, segmented tier control, recent-
//!     action strip (fed by `device_gate.recent_actions`, wired
//!     2026-05-30), Rotate-token + Revoke buttons.  Inline
//!     confirmation strips replace modals.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyView, App, AppContext, AsyncApp, Context, ElementId, FontWeight,
    IntoElement, Render, SharedString, Stateful, Window,
};
use wylde_theme::colors::{
    BORDER_DEFAULT, BORDER_EMPHASIS, BORDER_SUBTLE, BRAND, BRAND_DIM, BRAND_LIGHT, SURFACE_700,
    SURFACE_800, SURFACE_900, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};
use wylde_theme::typography::{size, weight, FAMILY_INTER};

use crate::ipc::{
    cancel_pairing, get_pairing_status, list_devices, recent_actions, revoke as revoke_device,
    rotate_token, set_tier, start_pairing, tier_blurb, tier_label, ActionEntry, DeviceRow,
    PairingStatus, ALL_TIERS, TIER_DESTRUCTIVE,
};
use crate::qr::{pair_uri, render_matrix, QrMatrix};

/// Cadence the paired-device list re-polls at.  Matches the Svelte
/// page's 10 s timer — `last_seen` advances at most once per minute
/// when the mobile is connected, so anything faster is wasted IPC.
pub const DEVICES_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Cadence the pair card polls `get_pairing_status` to know when to
/// auto-close (mobile finished pairing).  Server lazy-expires the
/// window, so this is also the upper bound on "how long does the card
/// stay open after the user starts pairing".
pub const PAIR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Tick driving the visible "expires in M:SS" counter.
pub const PAIR_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How many recent-action entries to request per device.  The row
/// renders the newest few; a small cap keeps the IPC payload tight.
pub const RECENT_ACTIONS_LIMIT: u32 = 5;

/// How many recent-action entries the row actually renders inline.
const RECENT_ACTIONS_SHOWN: usize = 3;

/// Build the single-line "recent activity" summary for a device row.
///
/// `None` (no fetch yet / service down) and an empty slice both render
/// the muted "No recent activity" fallback so the row never shows a bare
/// em-dash once the verb is wired.  Otherwise we join the newest
/// `RECENT_ACTIONS_SHOWN` action labels — the entries arrive newest-first
/// from `device_gate.recent_actions`, so no re-sort is needed.
fn activity_line(entries: Option<&[ActionEntry]>) -> String {
    let Some(entries) = entries else {
        return "Recent activity · No recent activity".to_owned();
    };
    if entries.is_empty() {
        return "Recent activity · No recent activity".to_owned();
    }
    let shown: Vec<&str> = entries
        .iter()
        .take(RECENT_ACTIONS_SHOWN)
        .map(|e| e.action.as_str())
        .collect();
    format!("Recent activity · {}", shown.join(" · "))
}

pub struct DevicesPanel {
    pub devices: Vec<DeviceRow>,
    pub error: Option<String>,
    pub loading_devices: bool,
    pub initial_load_done: bool,

    /// Active pairing window; populated when the user clicks Initiate
    /// pairing, cleared on cancel / success / timer expiry.
    pub pairing: Option<PairingCard>,

    /// Last-rendered "now" used by the countdown.  Tracked separately
    /// so the View doesn't read the clock during render (which would
    /// make the snapshot non-deterministic for tests).
    pub now_secs: f64,

    /// Which row is currently asking for revoke confirmation.
    pub confirm_revoke: Option<String>,

    /// Which row is currently asking for tier-escalation confirmation
    /// (only fires for `destructive_tool_access`).  Stores the target
    /// tier so confirm-click knows what to set.
    pub confirm_tier_escalation: Option<(String, String)>,

    /// Most recently rotated token, surfaced inline so the user can
    /// copy it.  Wire `device_id` so we know which row to attach the
    /// card to.
    pub rotated: Option<RotatedToken>,

    /// Per-device recent-action log, keyed by `device_id`.  Populated
    /// lazily by `device_gate.recent_actions` on each device-list
    /// refresh; an absent / empty entry renders "No recent activity".
    pub recent_actions: HashMap<String, Vec<ActionEntry>>,
}

/// State of the inline pairing card.
pub struct PairingCard {
    pub code: String,
    pub expires_at: f64,
    pub qr: QrMatrix,
}

pub struct RotatedToken {
    pub device_id: String,
    pub new_token: String,
}

impl DevicesPanel {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            error: None,
            loading_devices: true,
            initial_load_done: false,
            pairing: None,
            now_secs: 0.0,
            confirm_revoke: None,
            confirm_tier_escalation: None,
            rotated: None,
            recent_actions: HashMap::new(),
        }
    }

    pub fn view(_window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let panel = Self::new();
            Self::spawn_refresh(cx);
            Self::spawn_poll_loop(cx);
            Self::spawn_tick_loop(cx);
            panel
        })
        .into()
    }

    pub fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = list_devices().await;
            let device_ids = this
                .update(app_cx, |panel, cx| {
                    let ids: Vec<String> = match outcome {
                        Ok(rows) => {
                            panel.error = None;
                            let ids = rows.iter().map(|d| d.device_id.clone()).collect();
                            panel.devices = rows;
                            ids
                        }
                        Err(e) => {
                            panel.error = Some(format!("list_devices: {e}"));
                            Vec::new()
                        }
                    };
                    panel.loading_devices = false;
                    panel.initial_load_done = true;
                    cx.notify();
                    ids
                })
                .unwrap_or_default();
            // Fan out the per-device action-log reads after the list
            // lands so the rows paint immediately and the activity strip
            // fills in as each reply returns.
            for device_id in device_ids {
                Self::fetch_recent_actions(this.clone(), app_cx, device_id).await;
            }
        })
        .detach();
    }

    /// Read one device's recent-action log and merge it into
    /// `recent_actions`.  Soft-fails: a transport error leaves the prior
    /// entry (or absence) intact so the row keeps its last-known strip.
    async fn fetch_recent_actions(
        this: gpui::WeakEntity<Self>,
        app_cx: &mut AsyncApp,
        device_id: String,
    ) {
        if let Ok(entries) = recent_actions(&device_id, RECENT_ACTIONS_LIMIT).await {
            let _ = this.update(app_cx, |panel, cx| {
                panel.recent_actions.insert(device_id.clone(), entries);
                cx.notify();
            });
        }
    }

    /// Long-lived loop — every `DEVICES_POLL_INTERVAL` we re-read the
    /// device list (so `last_seen`/`is_active` track reality) and, if
    /// a pair card is open, re-read pairing status so we close the
    /// card the instant the mobile completes pairing.
    pub fn spawn_poll_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                // gpui executor has no tokio reactor — native timer.
                app_cx.background_executor().timer(PAIR_POLL_INTERVAL).await;
                // Pair-card refresh fires on every 2 s tick; the device
                // list only on a 10 s subdivide.
                let pair_alive = this
                    .update(app_cx, |panel, _| panel.pairing.is_some())
                    .unwrap_or(false);
                if pair_alive {
                    let status = get_pairing_status().await;
                    let still_alive = this
                        .update(app_cx, |panel, cx| {
                            match status {
                                Ok(PairingStatus::Inactive) => {
                                    // Server says we're done — close the
                                    // card and refresh the device list
                                    // so the freshly-paired row shows up.
                                    if panel.pairing.take().is_some() {
                                        cx.notify();
                                        Self::spawn_refresh(cx);
                                    }
                                }
                                Ok(PairingStatus::Active { code, expires_at }) => {
                                    if let Some(card) = panel.pairing.as_mut() {
                                        // Code rotates if the server
                                        // restarted; rebuild the QR so
                                        // the displayed image matches.
                                        if card.code != code {
                                            card.qr = QrMatrix::encode(&pair_uri(&code));
                                            card.code = code;
                                        }
                                        card.expires_at = expires_at;
                                        cx.notify();
                                    }
                                }
                                Err(_) => { /* keep the card; transient */ }
                            }
                        })
                        .is_ok();
                    if !still_alive {
                        return;
                    }
                }

                // Hash the loop iteration count by checking elapsed
                // device-list interval via floor division on now_secs.
                // Simpler: refresh devices on every 5th pair-tick (10 s
                // when PAIR_POLL_INTERVAL == 2 s).
                let should_refresh = this
                    .update(app_cx, |panel, _| {
                        panel.now_secs.floor() as i64 % DEVICES_POLL_INTERVAL.as_secs() as i64 == 0
                    })
                    .unwrap_or(false);
                if should_refresh {
                    Self::spawn_refresh_only(this.clone(), app_cx).await;
                }
            }
        })
        .detach();
    }

    async fn spawn_refresh_only(this: gpui::WeakEntity<Self>, app_cx: &mut AsyncApp) {
        let outcome = list_devices().await;
        let device_ids = this
            .update(app_cx, |panel, cx| {
                let mut ids = Vec::new();
                if let Ok(rows) = outcome {
                    ids = rows.iter().map(|d| d.device_id.clone()).collect();
                    panel.devices = rows;
                    cx.notify();
                }
                ids
            })
            .unwrap_or_default();
        for device_id in device_ids {
            Self::fetch_recent_actions(this.clone(), app_cx, device_id).await;
        }
    }

    /// Decrement the visible "expires in" counter once per second.
    pub fn spawn_tick_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            loop {
                let still_alive = this
                    .update(app_cx, |panel, cx| {
                        panel.now_secs = now_secs();
                        // Auto-close a pair card when the server-side
                        // deadline runs out — saves a 2 s poll round-
                        // trip in the common timeout case.
                        if let Some(card) = panel.pairing.as_ref() {
                            if card.expires_at <= panel.now_secs {
                                panel.pairing = None;
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !still_alive {
                    return;
                }
                // gpui executor has no tokio reactor — native timer.
                app_cx.background_executor().timer(PAIR_TICK_INTERVAL).await;
            }
        })
        .detach();
    }

    pub fn start_pairing(&mut self, cx: &mut Context<Self>) {
        if self.pairing.is_some() {
            return;
        }
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = start_pairing().await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(PairingStatus::Active { code, expires_at }) => {
                        panel.pairing = Some(PairingCard {
                            qr: QrMatrix::encode(&pair_uri(&code)),
                            code,
                            expires_at,
                        });
                        panel.error = None;
                    }
                    Ok(PairingStatus::Inactive) => {
                        panel.error = Some("device-gate refused to open a pairing window".into());
                    }
                    Err(e) => {
                        panel.error = Some(format!("start_pairing: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn cancel_pairing(&mut self, cx: &mut Context<Self>) {
        let was_open = self.pairing.take().is_some();
        if !was_open {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            // Surface a cancel failure: the card was closed optimistically,
            // but if the backend cancel errors the server-side pairing window
            // is still open (a device could still complete against the code).
            let outcome = cancel_pairing().await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Err(e) = outcome {
                    panel.error = Some(format!("cancel pairing: {e}"));
                }
                cx.notify();
                Self::spawn_refresh(cx);
            });
        })
        .detach();
    }

    pub fn click_tier(&mut self, device_id: String, tier: String, cx: &mut Context<Self>) {
        // No-op if already on the requested tier.
        if let Some(row) = self.devices.iter().find(|d| d.device_id == device_id) {
            if row.tier == tier {
                return;
            }
        }
        if tier == TIER_DESTRUCTIVE {
            self.confirm_tier_escalation = Some((device_id, tier));
            cx.notify();
            return;
        }
        self.apply_tier_change(device_id, tier, cx);
    }

    pub fn confirm_tier(&mut self, cx: &mut Context<Self>) {
        if let Some((device_id, tier)) = self.confirm_tier_escalation.take() {
            cx.notify();
            self.apply_tier_change(device_id, tier, cx);
        }
    }

    pub fn cancel_tier_escalation(&mut self, cx: &mut Context<Self>) {
        if self.confirm_tier_escalation.take().is_some() {
            cx.notify();
        }
    }

    fn apply_tier_change(&mut self, device_id: String, tier: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = set_tier(&device_id, &tier).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(()) => {
                        // Update local view eagerly so the pill flips
                        // before the next poll.  The next list-devices
                        // refresh confirms the server's view.
                        if let Some(row) =
                            panel.devices.iter_mut().find(|d| d.device_id == device_id)
                        {
                            row.tier = tier.clone();
                        }
                        panel.error = None;
                    }
                    Err(e) => {
                        panel.error = Some(format!("set_tier {device_id} → {tier}: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn request_revoke(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.confirm_revoke = Some(device_id);
        cx.notify();
    }

    pub fn cancel_revoke(&mut self, cx: &mut Context<Self>) {
        if self.confirm_revoke.take().is_some() {
            cx.notify();
        }
    }

    pub fn confirm_revoke(&mut self, cx: &mut Context<Self>) {
        let Some(device_id) = self.confirm_revoke.take() else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = revoke_device(&device_id).await;
            let _ = this.update(app_cx, |panel, cx| {
                if let Err(e) = outcome {
                    panel.error = Some(format!("revoke {device_id}: {e}"));
                } else {
                    panel.devices.retain(|d| d.device_id != device_id);
                    // Drop a rotation card pointing at the now-revoked
                    // device so we don't leave a stale token visible.
                    if panel
                        .rotated
                        .as_ref()
                        .map(|r| r.device_id == device_id)
                        .unwrap_or(false)
                    {
                        panel.rotated = None;
                    }
                }
                cx.notify();
                Self::spawn_refresh(cx);
            });
        })
        .detach();
    }

    pub fn rotate_token(&mut self, device_id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = rotate_token(&device_id).await;
            let _ = this.update(app_cx, |panel, cx| {
                match outcome {
                    Ok(new_token) => {
                        panel.rotated = Some(RotatedToken {
                            device_id,
                            new_token,
                        });
                        panel.error = None;
                    }
                    Err(e) => {
                        panel.error = Some(format!("rotate_token: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn dismiss_rotated(&mut self, cx: &mut Context<Self>) {
        if self.rotated.take().is_some() {
            cx.notify();
        }
    }
}

impl Default for DevicesPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for DevicesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div()
            .max_w(px(860.0))
            .flex()
            .flex_col()
            .gap_5()
            .child(header_row(cx));

        if let Some(err) = &self.error {
            column = column.child(error_strip(err));
        }

        column = column.child(section_title("Pair a device"));
        column = column.child(pair_section(self, cx));

        column = column.child(section_title("Paired devices"));
        if !self.initial_load_done && self.loading_devices {
            column = column.child(loading_row());
        } else if self.devices.is_empty() {
            column = column.child(empty_devices_state(cx));
        } else {
            for d in self.devices.clone() {
                column = column.child(device_row(self, &d, cx));
            }
        }

        div()
            .size_full()
            .bg(rgb(pack(SURFACE_900)))
            .p_6()
            .child(column)
    }
}

fn header_row(cx: &mut Context<DevicesPanel>) -> gpui::Div {
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
                        .child(SharedString::from("Devices")),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::XS))
                        .text_color(rgb(pack(TEXT_SECONDARY)))
                        .child(SharedString::from(
                            "Mobile + tablet devices paired with this Wylde install.  The \
                             Gateway checks every external request against the device's \
                             permission tier.",
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
                    "Remote access",
                    "devices-jump-vpn",
                    "core/remote_access",
                    cx,
                ))
                .child(refresh_button(cx)),
        )
}

fn refresh_button(cx: &mut Context<DevicesPanel>) -> Stateful<gpui::Div> {
    let id: ElementId = ElementId::Name("devices-refresh".into());
    div()
        .id(id)
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
            cx.listener(|_this: &mut DevicesPanel, _ev, _w, cx| {
                DevicesPanel::spawn_refresh(cx);
            }),
        )
        .child(SharedString::from("Refresh"))
}

fn jump_button(
    label: &'static str,
    id_str: &'static str,
    nav_key: &'static str,
    cx: &mut Context<DevicesPanel>,
) -> Stateful<gpui::Div> {
    div()
        .id(ElementId::Name(id_str.into()))
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
            cx.listener(move |_this: &mut DevicesPanel, _ev, _w, _cx| {
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

fn pair_section(panel: &DevicesPanel, cx: &mut Context<DevicesPanel>) -> gpui::Div {
    if let Some(card) = panel.pairing.as_ref() {
        return pairing_card_view(panel, card, cx);
    }
    pair_idle_card(cx)
}

fn pair_idle_card(cx: &mut Context<DevicesPanel>) -> gpui::Div {
    card_shell(
        div()
            .flex()
            .flex_row()
            .items_center()
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
                            .text_size(px(size::SM))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(
                                "Generate a 6-digit code and QR for the Wylde mobile app to \
                                 scan.  The code is valid for 5 minutes.",
                            )),
                    )
                    .child(
                        div()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::MICRO))
                            .text_color(rgb(pack(TEXT_MUTED)))
                            .child(SharedString::from(
                                "Newly-paired devices start at Read only.  Promote them once you \
                                 trust them.",
                            )),
                    ),
            )
            .child(
                div()
                    .id(ElementId::Name("devices-pair-start".into()))
                    .px_4()
                    .py_2()
                    .rounded(px(8.0))
                    .bg(rgb(pack(BRAND)))
                    .font_family(FAMILY_INTER)
                    .text_size(px(size::SM))
                    .font_weight(FontWeight(weight::SEMIBOLD as f32))
                    .text_color(rgb(pack(TEXT_PRIMARY)))
                    .cursor_pointer()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| {
                            this.start_pairing(cx);
                        }),
                    )
                    .child(SharedString::from("Initiate pairing")),
            ),
    )
}

fn pairing_card_view(
    panel: &DevicesPanel,
    card: &PairingCard,
    cx: &mut Context<DevicesPanel>,
) -> gpui::Div {
    let remaining = (card.expires_at - panel.now_secs).max(0.0) as i64;
    let countdown = SharedString::from(format!(
        "Expires in {minutes}:{secs:02}",
        minutes = remaining / 60,
        secs = (remaining % 60).max(0),
    ));
    let qr_view = render_matrix(&card.qr);

    card_shell(
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_5()
            .child(qr_view)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::SM))
                            .text_color(rgb(pack(TEXT_PRIMARY)))
                            .child(SharedString::from(
                                "Open the Wylde mobile app and tap Add Server.  Scan the QR or \
                                 enter the code along with your Wylde username + password.",
                            )),
                    )
                    .child(pin_code_view(&card.code))
                    .child(
                        div()
                            .font_family(FAMILY_INTER)
                            .text_size(px(size::XS))
                            .text_color(rgb(pack(BRAND_LIGHT)))
                            .child(countdown),
                    )
                    .child(
                        div().flex().flex_row().gap_2().child(
                            div()
                                .id(ElementId::Name("devices-pair-cancel".into()))
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
                                    cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| {
                                        this.cancel_pairing(cx);
                                    }),
                                )
                                .child(SharedString::from("Cancel")),
                        ),
                    ),
            ),
    )
}

fn pin_code_view(code: &str) -> gpui::Div {
    div()
        .bg(rgb(pack(SURFACE_900)))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .rounded(px(6.0))
        .px_4()
        .py_3()
        .font_family(FAMILY_INTER)
        .text_size(px(28.0))
        .font_weight(FontWeight(weight::SEMIBOLD as f32))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .child(SharedString::from(format_pin(code)))
}

fn device_row(panel: &DevicesPanel, d: &DeviceRow, cx: &mut Context<DevicesPanel>) -> gpui::Div {
    let confirming_revoke = panel.confirm_revoke.as_deref() == Some(d.device_id.as_str());
    let escalating = panel
        .confirm_tier_escalation
        .as_ref()
        .map(|(id, _)| id.as_str() == d.device_id.as_str())
        .unwrap_or(false);
    let rotated_for_this = panel
        .rotated
        .as_ref()
        .map(|r| r.device_id == d.device_id)
        .unwrap_or(false);

    let mut row = div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(if d.is_active {
            BORDER_EMPHASIS
        } else {
            BORDER_SUBTLE
        })))
        .rounded(px(6.0))
        .p_4()
        .flex()
        .flex_col()
        .gap_3();

    row = row.child(device_row_header(panel, d, cx));

    row = row.child(tier_pill_row(d, cx));
    row = row.child(
        div()
            .font_family(FAMILY_INTER)
            .text_size(px(size::MICRO))
            .text_color(rgb(pack(TEXT_MUTED)))
            .child(SharedString::from(tier_blurb(&d.tier).to_owned())),
    );

    if escalating {
        row = row.child(escalation_strip(cx));
    }
    if confirming_revoke {
        row = row.child(revoke_confirm_strip(cx));
    }
    if rotated_for_this {
        if let Some(r) = panel.rotated.as_ref() {
            row = row.child(rotated_token_strip(&r.new_token, cx));
        }
    }
    row
}

/// Top line of a device card: identity (name + online/offline pill, meta,
/// recent-activity) on the left, the rotate/revoke action buttons on the
/// right.
fn device_row_header(
    panel: &DevicesPanel,
    d: &DeviceRow,
    cx: &mut Context<DevicesPanel>,
) -> gpui::Div {
    let name_for_revoke = d.device_id.clone();
    let name_for_rotate = d.device_id.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_family(FAMILY_INTER)
                                .text_size(px(size::SM))
                                .text_color(rgb(pack(TEXT_PRIMARY)))
                                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                                .child(SharedString::from(display_name(d))),
                        )
                        .child(if d.is_active {
                            online_pill()
                        } else {
                            offline_pill()
                        }),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(meta_line(d))),
                )
                .child(
                    div()
                        .font_family(FAMILY_INTER)
                        .text_size(px(size::MICRO))
                        .text_color(rgb(pack(TEXT_MUTED)))
                        .child(SharedString::from(activity_line(
                            panel.recent_actions.get(&d.device_id).map(Vec::as_slice),
                        ))),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(rotate_button(
                    ElementId::Name(format!("devices-rotate::{}", d.device_id).into()),
                    cx.listener(move |this: &mut DevicesPanel, _ev, _w, cx| {
                        this.rotate_token(name_for_rotate.clone(), cx);
                    }),
                ))
                .child(revoke_button(
                    ElementId::Name(format!("devices-revoke::{}", d.device_id).into()),
                    cx.listener(move |this: &mut DevicesPanel, _ev, _w, cx| {
                        this.request_revoke(name_for_revoke.clone(), cx);
                    }),
                )),
        )
}

fn tier_pill_row(d: &DeviceRow, cx: &mut Context<DevicesPanel>) -> gpui::Div {
    let mut row = div()
        .flex()
        .flex_row()
        .rounded(px(6.0))
        .overflow_hidden()
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)));
    let last_idx = ALL_TIERS.len() - 1;
    for (idx, tier) in ALL_TIERS.iter().enumerate() {
        let is_current = d.tier == *tier;
        let tier_owned = (*tier).to_owned();
        let device_owned = d.device_id.clone();
        let id: ElementId =
            ElementId::Name(format!("devices-tier::{}::{tier}", d.device_id).into());
        let mut pill = div()
            .id(id)
            .px_3()
            .py_2()
            .flex_1()
            .cursor_pointer()
            .font_family(FAMILY_INTER)
            .text_size(px(size::XS))
            .text_color(rgb(pack(if is_current {
                BRAND_LIGHT
            } else {
                TEXT_SECONDARY
            })))
            .bg(rgb(pack(if is_current { BRAND_DIM } else { SURFACE_900 })))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this: &mut DevicesPanel, _ev, _w, cx| {
                    this.click_tier(device_owned.clone(), tier_owned.clone(), cx);
                }),
            )
            .child(SharedString::from(tier_label(tier)));
        if idx != last_idx {
            pill = pill.border_r_1().border_color(rgb(pack(BORDER_SUBTLE)));
        }
        row = row.child(pill);
    }
    row
}

fn escalation_strip(cx: &mut Context<DevicesPanel>) -> gpui::Div {
    inline_confirm_strip(
        "Grant full (write/delete/execute) access? Only do this for devices you trust completely.",
        "devices-tier-confirm",
        "devices-tier-cancel",
        "Grant",
        cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| this.confirm_tier(cx)),
        cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| this.cancel_tier_escalation(cx)),
    )
}

fn revoke_confirm_strip(cx: &mut Context<DevicesPanel>) -> gpui::Div {
    inline_confirm_strip(
        "Revoke this device? It will be signed out and have to re-pair.",
        "devices-revoke-confirm",
        "devices-revoke-cancel",
        "Yes, revoke",
        cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| this.confirm_revoke(cx)),
        cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| this.cancel_revoke(cx)),
    )
}

fn rotated_token_strip(token: &str, cx: &mut Context<DevicesPanel>) -> gpui::Div {
    div()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pt_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(
                    "New token issued.  Copy it into the device's recovery channel before \
                     dismissing — Wylde will not show it again.",
                )),
        )
        .child(
            div()
                .bg(rgb(pack(SURFACE_900)))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .rounded(px(4.0))
                .px_3()
                .py_2()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(token.to_owned())),
        )
        .child(
            div()
                .id(ElementId::Name("devices-rotate-dismiss".into()))
                .self_end()
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_DEFAULT)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| {
                        this.dismiss_rotated(cx);
                    }),
                )
                .child(SharedString::from("Dismiss")),
        )
}

fn inline_confirm_strip<F1, F2>(
    prompt: &str,
    confirm_id: &'static str,
    cancel_id: &'static str,
    confirm_label: &'static str,
    confirm: F1,
    cancel: F2,
) -> gpui::Div
where
    F1: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    F2: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    div()
        .border_t_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .pt_2()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .child(SharedString::from(prompt.to_owned())),
        )
        .child(
            div()
                .id(ElementId::Name(confirm_id.into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .bg(rgb(pack(BRAND_DIM)))
                .border_1()
                .border_color(rgb(pack(BORDER_EMPHASIS)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .on_mouse_down(gpui::MouseButton::Left, confirm)
                .child(SharedString::from(confirm_label)),
        )
        .child(
            div()
                .id(ElementId::Name(cancel_id.into()))
                .px_3()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(pack(BORDER_SUBTLE)))
                .cursor_pointer()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_SECONDARY)))
                .on_mouse_down(gpui::MouseButton::Left, cancel)
                .child(SharedString::from("Cancel")),
        )
}

fn empty_devices_state(cx: &mut Context<DevicesPanel>) -> gpui::Div {
    let body = div()
        .id(ElementId::Name("devices-empty-start".into()))
        .cursor_pointer()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this: &mut DevicesPanel, _ev, _w, cx| {
                this.start_pairing(cx);
            }),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::SM))
                .text_color(rgb(pack(TEXT_PRIMARY)))
                .font_weight(FontWeight(weight::SEMIBOLD as f32))
                .child(SharedString::from("No devices paired yet")),
        )
        .child(
            div()
                .font_family(FAMILY_INTER)
                .text_size(px(size::XS))
                .text_color(rgb(pack(TEXT_MUTED)))
                .child(SharedString::from(
                    "Pair your phone or tablet to access Wylde remotely.  Click to start.",
                )),
        );
    div()
        .bg(rgb(pack(SURFACE_800)))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .rounded(px(6.0))
        .p_6()
        .child(body)
}

fn online_pill() -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_EMPHASIS)))
        .bg(rgb(pack(SURFACE_900)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(BRAND_LIGHT)))
        .child(SharedString::from("online"))
}

fn offline_pill() -> gpui::Div {
    div()
        .px_2()
        .py(px(1.0))
        .rounded(px(999.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .bg(rgb(pack(SURFACE_700)))
        .font_family(FAMILY_INTER)
        .text_size(px(size::MICRO))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("offline"))
}

fn rotate_button<F>(id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_SECONDARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from("Rotate token"))
}

fn revoke_button<F>(id: ElementId, listener: F) -> Stateful<gpui::Div>
where
    F: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(pack(BORDER_SUBTLE)))
        .cursor_pointer()
        .font_family(FAMILY_INTER)
        .text_size(px(size::XS))
        .text_color(rgb(pack(TEXT_PRIMARY)))
        .on_mouse_down(gpui::MouseButton::Left, listener)
        .child(SharedString::from("Revoke"))
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

fn loading_row() -> gpui::Div {
    div()
        .font_family(FAMILY_INTER)
        .text_size(px(size::SM))
        .text_color(rgb(pack(TEXT_MUTED)))
        .child(SharedString::from("Loading paired devices…"))
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

// ── Pure projections (testable) ──────────────────────────────────────

pub(crate) fn now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Group the 6-digit pin as "123 · 456" so it reads as two chunks
/// without distorting the count.  Anything not exactly 6 chars passes
/// through verbatim — the View never trusts the wire shape blindly.
pub(crate) fn format_pin(code: &str) -> String {
    if code.chars().count() != 6 {
        return code.to_owned();
    }
    let chars: Vec<char> = code.chars().collect();
    format!(
        "{}{}{} · {}{}{}",
        chars[0], chars[1], chars[2], chars[3], chars[4], chars[5],
    )
}

pub(crate) fn display_name(d: &DeviceRow) -> String {
    if d.name.trim().is_empty() {
        format!("Device {}", d.short_fingerprint())
    } else {
        d.name.clone()
    }
}

pub(crate) fn meta_line(d: &DeviceRow) -> String {
    format!(
        "{} · Paired {} · Last seen {}",
        d.short_fingerprint(),
        relative_time(d.paired_at),
        relative_time(d.last_seen),
    )
}

pub(crate) fn relative_time(ts: f64) -> String {
    if ts <= 0.0 {
        return "—".to_owned();
    }
    let secs = (now_secs() - ts).max(0.0);
    if secs < 60.0 {
        "just now".to_owned()
    } else if secs < 3_600.0 {
        format!("{}m ago", (secs / 60.0).round() as i64)
    } else if secs < 86_400.0 {
        format!("{}h ago", (secs / 3_600.0).round() as i64)
    } else if secs < 86_400.0 * 7.0 {
        format!("{}d ago", (secs / 86_400.0).round() as i64)
    } else {
        format!("{}w ago", (secs / (86_400.0 * 7.0)).round() as i64)
    }
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
    use crate::ipc::{TIER_DESTRUCTIVE, TIER_READ_ONLY, TIER_TOOL_USE};

    #[test]
    fn render_signature_compiles() {
        fn assert_render<T: Render>() {}
        assert_render::<DevicesPanel>();
    }

    #[test]
    fn new_panel_starts_loading_with_no_devices() {
        let p = DevicesPanel::new();
        assert!(p.devices.is_empty());
        assert!(p.pairing.is_none());
        assert!(p.error.is_none());
        assert!(p.loading_devices);
        assert!(!p.initial_load_done);
        assert!(p.confirm_revoke.is_none());
        assert!(p.confirm_tier_escalation.is_none());
        assert!(p.rotated.is_none());
    }

    #[test]
    fn format_pin_groups_six_digits() {
        assert_eq!(format_pin("123456"), "123 · 456");
        // Non-6-char strings pass through unchanged so a tampered wire
        // reply doesn't get silently chopped up.
        assert_eq!(format_pin("12345"), "12345");
        assert_eq!(format_pin("1234567"), "1234567");
        assert_eq!(format_pin(""), "");
    }

    #[test]
    fn display_name_falls_back_to_fingerprint() {
        let d = DeviceRow {
            device_id: "dev_1_abcdef".into(),
            name: "".into(),
            ..DeviceRow::default()
        };
        assert_eq!(display_name(&d), "Device abcdef");
        let named = DeviceRow {
            name: "the Wylde user's Pixel".into(),
            ..d
        };
        assert_eq!(display_name(&named), "the Wylde user's Pixel");
    }

    #[test]
    fn relative_time_uses_em_dash_for_zero() {
        assert_eq!(relative_time(0.0), "—");
        assert_eq!(relative_time(-1.0), "—");
    }

    #[test]
    fn pack_round_trips_known_surface() {
        assert_eq!(pack(SURFACE_900), 0x0a_0e_17);
        assert_eq!(pack(BRAND), 0x0e_74_90);
    }

    #[test]
    fn refresh_interval_pinned_at_ten_seconds() {
        assert_eq!(DEVICES_POLL_INTERVAL.as_secs(), 10);
    }

    #[test]
    fn pair_tick_pinned_at_one_second() {
        assert_eq!(PAIR_TICK_INTERVAL.as_secs(), 1);
    }

    #[test]
    fn pair_poll_pinned_at_two_seconds() {
        assert_eq!(PAIR_POLL_INTERVAL.as_secs(), 2);
    }

    #[test]
    fn tier_pill_row_covers_all_three_tiers() {
        // Surfaces a refactor that drops a tier (would be a behaviour
        // regression — the user could no longer downgrade a device).
        let tiers: Vec<&&str> = ALL_TIERS.iter().collect();
        assert_eq!(tiers.len(), 3);
        assert!(tiers.iter().any(|t| ***t == *TIER_READ_ONLY));
        assert!(tiers.iter().any(|t| ***t == *TIER_TOOL_USE));
        assert!(tiers.iter().any(|t| ***t == *TIER_DESTRUCTIVE));
    }

    #[test]
    fn meta_line_includes_fingerprint() {
        let d = DeviceRow {
            device_id: "dev_1_a1b2c3".into(),
            paired_at: 0.0,
            last_seen: 0.0,
            ..DeviceRow::default()
        };
        let line = meta_line(&d);
        assert!(line.contains("a1b2c3"));
        // Both em-dashes (no timestamps) — the line should not contain
        // any of the bucket suffixes.
        assert!(!line.contains("ago"));
    }
}
