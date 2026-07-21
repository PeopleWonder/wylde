//! Main-window root View — sidebar + slot.
//!
//! Built in slice 3.  Replaces the foundation-slice wordmark splash
//! with the actual app chrome:
//!
//!   * Left: `sidebar::render_sidebar` driven by `NavModel::rows`.
//!   * Right: `slot::render_slot` driven by `NavModel::slot_state()`.
//!
//! The Shell owns:
//!   * `nav`     — the immutable row list + the live selection +
//!     the cached per-service health bits.
//!   * `mounted` — `AnyView` cache, populated lazily the first time a
//!     panel is selected.  Each panel's `cx.new(...)` happens at-most
//!     once per process lifetime.
//!   * Slot stub flashes are avoided by treating "no health probe
//!     reply yet" as healthy — the first probe arrives within a few
//!     ms of startup.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, AnyView, AsyncApp, Context, IntoElement, Render, Window};
use wylde_panel_registry::{PanelEntry, PanelOrigin, PanelRegistry, PanelSource};
use wylde_webview::IframeHost;

use crate::nav::{NavModel, NavOrigin, NavRow, SlotState};
use crate::resource_meter::{ResourceSnapshot, SVC_BROKER};
use crate::sidebar::render_sidebar;
use crate::slot::{render_slot, start_service_action, IframeFrame};

/// How often [`Shell::spawn_health_probes`] re-probes each required
/// service.  Chosen to recover the Chat panel's gate within a few
/// seconds of the harness pipe coming up after the GUI launched, while
/// staying light on the lifecycle pipe.  Matches the Dashboard panel's
/// 5 s refresh order-of-magnitude; 3 s here keeps the cold-start
/// "unavailable" flash short.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often [`Shell::spawn_resource_meter`] re-reads the VRAM broker's
/// `system.inventory` to refresh the sidebar's VRAM/RAM footer.  Matches
/// the Dashboard hardware card's 5 s cadence so the two surfaces tick in
/// lock-step and never disagree by more than one interval.
const RESOURCE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Where the iframe's URL-reachability probe lives in its lifecycle.
///
/// The lifecycle is:
///   `Probing` → `Healthy` (slot mounts the WebView)
///   `Probing` → `Unhealthy(msg)` (slot renders the existing
///                ServiceUnavailable stub)
///
/// A `Healthy` result can flip back to `Unhealthy` when the user hits
/// the slot's "Reconnect" affordance (lands in a follow-on slice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IframeHealth {
    Probing,
    Healthy,
    Unhealthy(String),
}

/// Per-iframe-panel runtime state owned by the Shell.
///
/// Held in `Shell::iframes` keyed by the registry key.  The state is
/// created lazily the first time the user selects an iframe panel and
/// kept across selections so a re-select doesn't re-mount the WebView
/// from scratch.
pub struct IframeState {
    pub url: String,
    pub sandbox: Option<String>,
    pub health: IframeHealth,
    pub host: IframeHost,
}

impl IframeState {
    pub fn new(url: impl Into<String>, sandbox: Option<String>) -> Self {
        let url = url.into();
        let host = IframeHost::new(url.clone(), sandbox.clone());
        Self {
            url,
            sandbox,
            health: IframeHealth::Probing,
            host,
        }
    }
}

/// One frame of the Shell's render state.  The struct is small — gpui
/// retains it and calls `Render::render` on every frame.
pub struct Shell {
    pub nav: NavModel,
    pub mounted: BTreeMap<String, AnyView>,
    /// Per-key iframe state.  Populated on first selection of an
    /// iframe panel; kept across selections so a flip back to a
    /// previously-mounted iframe is instant.
    pub iframes: BTreeMap<String, IframeState>,
    /// Last hardware snapshot from the VRAM broker, rendered in the
    /// sidebar footer.  `None` until the first `system.inventory` reply
    /// lands (cold start) — the footer shows em-dashes meanwhile.
    pub resources: Option<ResourceSnapshot>,
    /// Whether the one fire-and-forget startup update check (Phase 12.5,
    /// slice 3d) found a newer release.  Drives the hint dot on the
    /// Settings sidebar row.  Starts `false`; flips when
    /// [`Self::spawn_startup_update_check`]'s task completes.  Stays
    /// `false` when updates are off (the check makes no network call).
    pub update_available: bool,
    /// The version the user dismissed via the update pill's "Ignore" (#196),
    /// keyed exactly so a *newer* release re-shows the pill (see
    /// [`wylde_changelog::pill_visible`]). Seeded at startup from the persisted
    /// `skipped_version` and updated on each "Ignore" click. `None` means "no
    /// version dismissed" — the pill shows whenever an update is available.
    pub dismissed_version: Option<String>,
    /// The changelog pop-up, mounted lazily when the user clicks the pill's
    /// "What's new" affordance and dropped on close. `None` while closed. Held
    /// as an `AnyView` because the Shell only ever renders it and swaps it out —
    /// the viewer drives its own lazy-paging internally.
    pub changelog: Option<AnyView>,
}

impl Shell {
    /// Construct a Shell from the process-wide panel registry.  Pulls
    /// the rows out and applies the default selection.  Returns `None`
    /// when the registry hasn't been installed (the binary's startup
    /// path always installs it; this branch exists for tests).
    pub fn from_global_registry() -> Option<Self> {
        let reg = PanelRegistry::global()?;
        Some(Self::from_registry(reg))
    }

    pub fn from_registry(reg: &PanelRegistry) -> Self {
        let rows = build_rows_from_registry(reg);
        Self {
            nav: NavModel::new(rows, None),
            mounted: BTreeMap::new(),
            iframes: BTreeMap::new(),
            resources: None,
            update_available: false,
            dismissed_version: None,
            changelog: None,
        }
    }

    /// Sidebar click handler.  Updates the selection; the next render
    /// re-evaluates `slot_state()` and mints the panel View if it
    /// wasn't already cached.
    /// Returns `true` when `key` names a known panel (selection applied or
    /// it was already active); `false` when no such panel exists — lets the
    /// cross-panel nav drain log a stale/renamed key instead of dropping it
    /// silently.
    pub fn on_nav_click(&mut self, key: &str) -> bool {
        let known = self.nav.has_key(key);
        let _ = self.nav.select(key);
        known
    }

    /// Health-probe reply handler.  Called from the async startup task
    /// in `main.rs` for each `required_services` membership.
    pub fn apply_service_health(
        &mut self,
        name: &str,
        healthy: bool,
        reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.nav.mark_service_health(name, healthy);
        // Carry the daemon's specific cause (e.g. a min_core incompatibility) so
        // the stub can show *why* rather than a generic "not running".
        self.nav.mark_service_reason(name, reason);
        cx.notify();
    }

    /// Spawn one long-lived health-poll task per unique
    /// `required_services` member declared by the panel registry.  Each
    /// task re-probes on [`HEALTH_POLL_INTERVAL`] for the lifetime of the
    /// Shell entity.  Called once when the Shell entity is created.
    ///
    /// Why a poll loop rather than the original single probe?  The
    /// launcher (`launch_wylde.ps1`) waits only for `\\.\pipe\
    /// wylde-lifecycle` before launching the GUI — it does *not* wait for
    /// the harness / gateway / … pipes, which the daemon binds a beat
    /// later.  A one-shot probe that lands in that window saw the harness
    /// pipe as not-yet-up, cached the row `false`, and — with no
    /// re-probe — left the Chat panel's required-service gate stubbing
    /// out "wylde-harness is not running" *permanently*, even though the
    /// harness came up moments later.  The user's only recovery was the
    /// stub's "Start service" button (which happens to re-probe).
    ///
    /// Polling makes the gate self-heal: the first probe during the
    /// cold-start race marks the row unhealthy, and the next poll (once
    /// the pipe is bound) flips it back to healthy and mounts the panel.
    /// It also keeps tracking liveness afterwards, so a service that dies
    /// later correctly surfaces the stub.
    pub fn spawn_health_probes(&self, cx: &mut Context<Self>) {
        for service in unique_required_services(&self.nav.rows) {
            let svc: Arc<str> = Arc::from(service.as_str());
            cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
                // `is_ok()` alone is not enough for wylde-ollama: its
                // service.health stays ok (with an `upstream` flag) even when
                // the Ollama daemon is down, so gate readiness on the body.
                let (healthy, reason) = match wylde_gui_pipe::service_health(&svc).await {
                    Ok(body) => (
                        crate::nav::service_health_body_is_ready(&svc, &body),
                        crate::nav::service_health_reason(&body),
                    ),
                    Err(_) => (false, None),
                };
                let alive = this
                    .update(app_cx, |this, cx| {
                        this.apply_service_health(&svc, healthy, reason, cx);
                    })
                    .is_ok();
                // Entity torn down (Shell dropped) → stop the loop.
                if !alive {
                    return;
                }
                // gpui's executor has no tokio reactor — use its native
                // timer (`tokio::time::sleep` would panic "no reactor").
                app_cx
                    .background_executor()
                    .timer(HEALTH_POLL_INTERVAL)
                    .await;
                if this.update(app_cx, |_, _| {}).is_err() {
                    return;
                }
            })
            .detach();
        }
    }

    /// Spawn the long-lived poll that feeds the sidebar's VRAM/RAM
    /// footer.  Reuses the existing `system.inventory` verb the Dashboard
    /// already calls on `wylde-vram-broker` — no new IPC surface — and
    /// re-reads every [`RESOURCE_POLL_INTERVAL`] for the Shell's lifetime.
    ///
    /// Soft-fails: a probe that can't reach the broker (cold start, the
    /// broker bounced) leaves the previous snapshot in place rather than
    /// blanking the meter, so a momentary outage doesn't flicker the
    /// footer to em-dashes.  Until the *first* successful read the field
    /// is `None`, which the footer renders as "—".
    ///
    /// Same gpui-executor discipline as [`Self::spawn_health_probes`]:
    /// the wire IO hops to the pipe's tokio bridge and the inter-poll
    /// wait uses gpui's native timer (the executor has no tokio reactor).
    pub fn spawn_resource_meter(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| loop {
            let snapshot = match wylde_gui_pipe::call(
                SVC_BROKER,
                "POST",
                "/__action__",
                Some(serde_json::json!({ "action": "system.inventory", "payload": {} })),
            )
            .await
            {
                Ok(v) => Some(ResourceSnapshot::from_inventory_value(&v)),
                Err(_) => None,
            };
            let alive = this
                .update(app_cx, |this, cx| {
                    // Keep the last good snapshot on a failed probe.
                    if let Some(s) = snapshot {
                        this.resources = Some(s);
                    }
                    cx.notify();
                })
                .is_ok();
            // Entity torn down (Shell dropped) → stop the loop.
            if !alive {
                return;
            }
            app_cx
                .background_executor()
                .timer(RESOURCE_POLL_INTERVAL)
                .await;
            if this.update(app_cx, |_, _| {}).is_err() {
                return;
            }
        })
        .detach();
    }

    /// Fire the one background update check at startup (Phase 12.5,
    /// slice 3d).  Fire-and-forget: the wire IO + blocking updater run on
    /// the pipe's tokio bridge, the UI never blocks, and the result is
    /// cached in `wylde_gui_pipe::updater_state` for the Settings panel to
    /// read.  When it resolves an available update we flip
    /// `update_available` and `cx.notify()` so the sidebar paints the hint
    /// dot on the Settings row.
    ///
    /// Privacy: the check itself is gated inside `run_startup_check` —
    /// it makes no network call unless the user enabled updates *and*
    /// opted into automatic checks (and the cadence window has elapsed).
    /// On an opted-out install this task completes instantly, leaving
    /// `update_available` false and making zero outbound requests.
    pub fn spawn_startup_update_check(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let available =
                wylde_gui_pipe::updater_state::run_startup_check(env!("CARGO_PKG_VERSION")).await;
            // Only disturb a frame when there's something to show — a
            // "no update" result leaves the sidebar untouched.
            if available {
                let _ = this.update(app_cx, |this, cx| {
                    this.update_available = true;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Seed [`Self::dismissed_version`] from the persisted `skipped_version`
    /// (#196) so a version the user already declined stays dismissed across
    /// restarts. Fire-and-forget and best-effort: an unreadable pref just
    /// leaves the pill un-dismissed, and the automatic check already suppresses
    /// a skipped version on its own — this only keeps the *pill's* own gate
    /// consistent with a skip made elsewhere (e.g. the Settings panel).
    pub fn spawn_seed_dismissed_version(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            if let Some(version) = wylde_gui_pipe::updater_state::ignored_version().await {
                let _ = this.update(app_cx, |this, cx| {
                    this.dismissed_version = Some(version);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Update-pill "Update" (#196) → run the existing **whole-stack** install
    /// path for the resolved release. Same download-verify-install the Settings
    /// "Install" button drives; no new backend. Fire-and-forget: the new stack
    /// takes effect on the next launch, and the Settings Updates section is the
    /// surface that reports install progress / "restart to apply" — the pill's
    /// job is only to trigger it. A no-op if there's no resolved update.
    pub fn on_update_click(&mut self, cx: &mut Context<Self>) {
        let Some(info) = wylde_gui_pipe::updater_state::available_info() else {
            return;
        };
        cx.spawn(async move |_this, _app_cx: &mut AsyncApp| {
            let _ = wylde_gui_pipe::updater_state::install(info).await;
        })
        .detach();
    }

    /// Update-pill "Ignore" (#196) → dismiss the pill for **this version only**.
    /// Records the version locally (so the pill hides this frame) and persists
    /// it as `skipped_version` (so it stays hidden across restarts). Because the
    /// dismissal is keyed on the exact version, a later, newer release re-shows
    /// the pill — Ignore never permanently silences updates.
    pub fn on_ignore_click(&mut self, version: String, cx: &mut Context<Self>) {
        self.dismissed_version = Some(version.clone());
        cx.notify();
        cx.spawn(async move |_this, _app_cx: &mut AsyncApp| {
            let _ = wylde_gui_pipe::updater_state::ignore_version(&version).await;
        })
        .detach();
    }

    /// Update-pill "What's new" (#196) → mount the changelog pop-up. The newest
    /// (available) release's notes ride in from the already-fetched
    /// `available_info()`, so opening makes **no** new network call; the older
    /// history is the bundled local changelog. A no-op if it's already open.
    pub fn open_changelog(&mut self, cx: &mut Context<Self>) {
        if self.changelog.is_some() {
            return;
        }
        let headline = wylde_gui_pipe::updater_state::available_info().map(|info| {
            wylde_changelog::HeadlineRelease {
                version: info.version,
                notes: info.notes,
            }
        });
        let view = cx.new(|cx| wylde_changelog::ChangelogView::new(headline, cx));
        self.changelog = Some(view.into());
        cx.notify();
    }

    /// Close the changelog pop-up, dropping the viewer entity.
    pub fn close_changelog(&mut self, cx: &mut Context<Self>) {
        self.changelog = None;
        cx.notify();
    }

    /// Mount the selected panel's View if it isn't cached yet.  Called
    /// at the top of `render` so the slot can paint the view as a
    /// child.  Idempotent — repeated calls are a no-op once the View
    /// is cached.
    ///
    /// Iframe panels skip the AnyView path: there is no gpui View for
    /// them.  Instead this sets up the per-key `IframeState` and kicks
    /// off the URL probe; the slot reads the state on render and
    /// mounts the actual `wry::WebView` once it has a window handle +
    /// the slot's bounds.
    fn ensure_mounted_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.nav.selected_key.clone() else {
            return;
        };
        let Some(reg) = PanelRegistry::global() else {
            return;
        };
        let row = reg
            .entries()
            .into_iter()
            .find(|r| registry_key_matches(&r.origin, &r.entry.id, &key));
        let Some(row) = row else {
            return;
        };
        match &row.entry.source {
            PanelSource::GpuiView { .. } => {
                if self.mounted.contains_key(&key) {
                    return;
                }
                let Some(factory) = row.factory.as_ref() else {
                    return;
                };
                let view = (factory)(window, cx);
                self.mounted.insert(key, view);
            }
            PanelSource::Iframe { url, sandbox, .. } => {
                if self.iframes.contains_key(&key) {
                    return;
                }
                self.iframes
                    .insert(key.clone(), IframeState::new(url.clone(), sandbox.clone()));
                self.spawn_iframe_probe(key, cx);
            }
        }
    }

    /// Probe an iframe's URL once and write the result back into
    /// `self.iframes[key].health`.  Same one-shot pattern the service
    /// health probes use; a future slice can wire a "Reconnect"
    /// button to re-fire this.  3-second budget mirrors the Svelte
    /// alpha's iframe probe.
    pub fn spawn_iframe_probe(&self, key: String, cx: &mut Context<Self>) {
        let url = match self.iframes.get(&key) {
            Some(state) => state.url.clone(),
            None => return,
        };
        const PROBE_TIMEOUT_MS: u64 = 3_000;
        let key_for_async = key.clone();
        cx.spawn(async move |this, app_cx: &mut AsyncApp| {
            let outcome = wylde_webview::probe_url(&url, PROBE_TIMEOUT_MS).await;
            let _ = this.update(app_cx, |this, cx| {
                if let Some(state) = this.iframes.get_mut(&key_for_async) {
                    state.health = match outcome {
                        Ok(()) => IframeHealth::Healthy,
                        Err(e) => IframeHealth::Unhealthy(e),
                    };
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop the WebView for every iframe that isn't the current
    /// selection.  Called once per render so a nav-away frees the
    /// native HWND immediately — the WebView's painting otherwise
    /// stays on top of whatever the user navigated to.
    fn unmount_unselected_iframes(&mut self) {
        let selected = self.nav.selected_key.as_deref();
        for (key, state) in self.iframes.iter_mut() {
            if Some(key.as_str()) != selected {
                state.host.unmount();
            }
        }
    }

    /// Mount the selected iframe's WebView (if healthy + not already
    /// mounted) and resize it to the slot's current rect.  Called from
    /// `render` once per frame: cheap, idempotent.
    ///
    /// `parent` must be the gpui Window the Shell is rendering into —
    /// wry parents the WebView's HWND/NSView under it.
    fn mount_active_iframe(&mut self, window: &mut Window) {
        let Some(key) = self.nav.selected_key.clone() else {
            return;
        };
        let Some(state) = self.iframes.get_mut(&key) else {
            return;
        };
        // Refuse to mount until the URL probe has confirmed the server
        // is reachable.  Same gate the Svelte alpha uses.
        if !matches!(state.health, IframeHealth::Healthy) {
            return;
        }
        let bounds = slot_bounds(window);
        // `mount` is idempotent: a second call with the same WebView
        // just becomes `set_bounds`.  Errors are surfaced via the host
        // state but don't crash the render — a broken wry build would
        // otherwise prevent the rest of the GUI from painting.
        if let Err(e) = state.host.mount(window, bounds) {
            // Demote to unhealthy so the slot renders the stub instead
            // of a blank slot.  The error string mirrors the probe's
            // error shape for log-grep consistency.
            state.health = IframeHealth::Unhealthy(format!("wry mount: {e}"));
        }
    }

    /// "Start service" button click — fires a `service.start` action
    /// at the Lifecycle pipe and refreshes the row's health.  The
    /// service start is best-effort: if it fails the user sees the
    /// stub keep showing, which is the right signal.
    pub fn on_start_service_click(&mut self, service: Arc<str>, cx: &mut Context<Self>) {
        let (verb, payload) = start_service_action(&service);
        let service_for_async: Arc<str> = service.clone();
        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            let _ = wylde_gui_pipe::lifecycle_action(verb, payload).await;
            // Whether the start succeeded or not, re-probe so the UI
            // stops showing the stub the moment the daemon is up. Same
            // body-aware readiness gate as the poll loop — for ollama the
            // pipe answering ok isn't enough; the upstream daemon must be up.
            let (healthy, reason) = match wylde_gui_pipe::service_health(&service_for_async).await {
                Ok(body) => (
                    crate::nav::service_health_body_is_ready(&service_for_async, &body),
                    crate::nav::service_health_reason(&body),
                ),
                Err(_) => (false, None),
            };
            let _ = this.update(cx, |this, cx| {
                this.apply_service_health(&service_for_async, healthy, reason, cx);
            });
        });
        task.detach();
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_mounted_for_selection(window, cx);
        // Drop the native WebView for every iframe panel that isn't
        // the current selection so its HWND stops painting over the
        // active panel.  Cheap (no-op if already unmounted).
        self.unmount_unselected_iframes();
        // If the selected iframe panel is healthy and the slot has
        // bounds, mount the WebView and resize it.  No-op for non-
        // iframe selections.
        self.mount_active_iframe(window);

        let slot_state = self.nav.slot_state();
        let rows = self.nav.rows.clone();
        let selected_key = self.nav.selected_key.clone();
        // Snapshot the meter so the sidebar render borrows a clone, not
        // `self` (which `cx` already borrows mutably this frame).
        let resources = self.resources.clone();
        let update_available = self.update_available;
        let mounted_view = match &slot_state {
            SlotState::Mount { key } => self.mounted.get(key).cloned(),
            _ => None,
        };

        // For iframe panels, build the slot-side `IframeFrame`
        // descriptor: URL, sandbox, current health bucket.  The slot
        // mounts the WebView via `Shell::mount_iframe` on Healthy and
        // synthesises a `ServiceUnavailable` payload on Unhealthy so
        // the existing stub branch handles failure rendering.
        let iframe_frame = selected_iframe_frame(&self.iframes, selected_key.as_deref());

        let synthetic_unavailable = match (&slot_state, &iframe_frame) {
            (SlotState::Mount { key }, Some(frame)) => match &frame.health {
                IframeHealth::Unhealthy(msg) => Some(SlotState::ServiceUnavailable {
                    key: key.clone(),
                    missing: vec![format!("URL probe: {msg}")],
                    // The iframe path already carries its message in `missing`;
                    // no separate incompatibility reason applies here.
                    reasons: vec![None],
                }),
                _ => None,
            },
            _ => None,
        };
        let effective_state = synthetic_unavailable.unwrap_or(slot_state);

        // The bottom-left update pill (#196). Only touch the updater cache when
        // a check actually found something — no lock/clone per frame otherwise.
        let available_version = if update_available {
            wylde_gui_pipe::updater_state::available_info().map(|info| info.version)
        } else {
            None
        };
        let show_pill = wylde_changelog::pill_visible(
            update_available,
            available_version.as_deref(),
            self.dismissed_version.as_deref(),
        );

        // `.relative()` makes this container the positioning context for the
        // absolutely-placed pill and the changelog modal overlaid on top.
        let mut root = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .child(render_sidebar(
                &rows,
                selected_key.as_deref(),
                resources.as_ref(),
                update_available,
                window,
                cx,
            ))
            .child(render_slot(
                &effective_state,
                &rows,
                mounted_view.as_ref(),
                iframe_frame.as_ref(),
                window,
                cx,
            ));

        if show_pill {
            if let Some(version) = available_version {
                root = root.child(crate::update_pill::render_update_pill(&version, cx));
            }
        }

        // The changelog pop-up, layered above everything while open.
        if let Some(view) = self.changelog.as_ref() {
            root = root.child(crate::update_pill::render_changelog_modal(view, cx));
        }

        root
    }
}

/// Build an `IframeFrame` snapshot for the slot to render.  Returns
/// `None` when the selected panel isn't an iframe.  Pure function over
/// the Shell's iframe map — exposed so a future refactor can move the
/// iframe state out of Shell without rewriting the call site.
pub fn selected_iframe_frame(
    iframes: &BTreeMap<String, IframeState>,
    selected_key: Option<&str>,
) -> Option<IframeFrame> {
    let key = selected_key?;
    let state = iframes.get(key)?;
    Some(IframeFrame {
        key: key.to_owned(),
        url: state.url.clone(),
        sandbox: state.sandbox.clone(),
        health: state.health.clone(),
    })
}

/// Materialise the registry into the sidebar's `NavRow` shape.  Pure
/// function over the registry's entries — no gpui types reach it.
pub fn build_rows_from_registry(reg: &PanelRegistry) -> Vec<NavRow> {
    reg.entries()
        .into_iter()
        .map(|row| nav_row_from_entry(&row.origin, &row.entry))
        .collect()
}

/// Unique, ordered set of services any panel in the row list cares
/// about — what `spawn_health_probes` iterates.
pub fn unique_required_services(rows: &[NavRow]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        for svc in &row.required_services {
            if seen.insert(svc.clone()) {
                out.push(svc.clone());
            }
        }
    }
    out
}

fn nav_row_from_entry(origin: &PanelOrigin, entry: &PanelEntry) -> NavRow {
    let (origin_tag, key) = match origin {
        PanelOrigin::FirstParty { service } => {
            (NavOrigin::FirstParty, format!("{service}/{}", entry.id))
        }
        PanelOrigin::Extension { extension_id } => (
            NavOrigin::Extension,
            format!("ext:{extension_id}/{}", entry.id),
        ),
    };
    NavRow {
        key,
        origin: origin_tag,
        title: entry.title.clone(),
        icon: entry.icon.clone(),
        order: entry.order,
        required_services: entry.required_services.clone(),
    }
}

/// Match a registry row's origin+id against a `service/id`-shaped key.
fn registry_key_matches(origin: &PanelOrigin, id: &str, target: &str) -> bool {
    match origin {
        PanelOrigin::FirstParty { service } => target == format!("{service}/{id}"),
        PanelOrigin::Extension { extension_id } => target == format!("ext:{extension_id}/{id}"),
    }
}

/// Compute the slot's rect inside the window — sidebar width + the
/// remaining viewport.  Exposed so a future bounds-observer wiring can
/// share the same arithmetic.  Returns logical pixels matching wry's
/// `LogicalSize`.
pub fn slot_bounds(window: &Window) -> wylde_webview::Bounds {
    let viewport = window.viewport_size();
    let w = (viewport.width.to_f64() - crate::sidebar::SIDEBAR_WIDTH as f64).max(1.0);
    let h = viewport.height.to_f64().max(1.0);
    wylde_webview::Bounds::new(crate::sidebar::SIDEBAR_WIDTH as f64, 0.0, w, h)
}

/// Suppress the dead-code lint until the panel-registry import is
/// actively touched by every cfg path.  `PanelSource` is referenced
/// by the panel-factory machinery indirectly.
#[allow(dead_code)]
fn _imports_used() -> Option<PanelSource> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wylde_panel_registry::registry::RegistryRow;
    use wylde_panel_registry::{PanelEntry, PanelOrigin, PanelSource};

    fn first_party(service: &str, id: &str, title: &str, order: i32) -> RegistryRow {
        RegistryRow {
            origin: PanelOrigin::FirstParty {
                service: service.into(),
            },
            entry: PanelEntry {
                id: id.into(),
                title: title.into(),
                icon: None,
                order,
                version: "0.1.0".into(),
                required_services: vec![],
                source: PanelSource::GpuiView {
                    factory: format!("c::{title}::view"),
                },
            },
            factory: None,
        }
    }

    #[test]
    fn health_poll_interval_is_short_enough_to_self_heal_cold_start() {
        // The launcher only waits for the lifecycle pipe before starting
        // the GUI, so the harness pipe can bind a beat after the first
        // probe. The poll must re-fire promptly (and forever) so the Chat
        // gate flips unavailable→live without user intervention. Frozen so
        // a future bump to a sluggish interval surfaces in review.
        assert!(HEALTH_POLL_INTERVAL <= Duration::from_secs(5));
        assert!(HEALTH_POLL_INTERVAL >= Duration::from_secs(1));
    }

    #[test]
    fn build_rows_keeps_order_and_keys() {
        let mut reg = PanelRegistry::new();
        reg.register_internal(first_party("core", "chat", "Chat", 10))
            .unwrap();
        reg.register_internal(first_party("core", "settings", "Settings", 95))
            .unwrap();
        let rows = build_rows_from_registry(&reg);
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec!["core/chat".to_string(), "core/settings".into()]);
    }

    #[test]
    fn build_rows_carries_required_services_through() {
        let mut reg = PanelRegistry::new();
        let mut row = first_party("core", "settings", "Settings", 95);
        row.entry.required_services = vec!["wylde-harness".into()];
        reg.register_internal(row).unwrap();
        let rows = build_rows_from_registry(&reg);
        assert_eq!(rows[0].required_services, vec!["wylde-harness"]);
    }

    #[test]
    fn registry_key_matches_round_trips_first_party() {
        let origin = PanelOrigin::FirstParty {
            service: "core".into(),
        };
        assert!(registry_key_matches(&origin, "settings", "core/settings"));
        assert!(!registry_key_matches(&origin, "settings", "core/chat"));
    }

    #[test]
    fn unique_required_services_dedupes_and_preserves_order() {
        let rows = vec![
            NavRow {
                key: "core/chat".into(),
                origin: NavOrigin::FirstParty,
                title: "Chat".into(),
                icon: None,
                order: 10,
                required_services: vec!["wylde-harness".into(), "wylde-lifecycle".into()],
            },
            NavRow {
                key: "core/settings".into(),
                origin: NavOrigin::FirstParty,
                title: "Settings".into(),
                icon: None,
                order: 95,
                required_services: vec!["wylde-harness".into()],
            },
        ];
        assert_eq!(
            unique_required_services(&rows),
            vec!["wylde-harness".to_string(), "wylde-lifecycle".into()],
        );
    }

    #[test]
    fn registry_key_matches_round_trips_extension() {
        let origin = PanelOrigin::Extension {
            extension_id: "n8n".into(),
        };
        assert!(registry_key_matches(&origin, "editor", "ext:n8n/editor"));
        assert!(!registry_key_matches(&origin, "editor", "n8n/editor"));
    }
}
